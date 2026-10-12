"""Exercise existing repository services on real temporary Git working trees."""
from __future__ import annotations

from dataclasses import replace
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from homestack import repo, setup, setup_config
from homestack.models import AppError
from support import test_config


@unittest.skipUnless(shutil.which('git') and shutil.which('ssh-keygen'), 'Git and ssh-keygen are needed for checkout integration tests')
class RepositoryIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name) / 'home'
        self.home.mkdir()
        self.cfg = test_config()
        self.keys = {}
        self.sources = {}
        self.seeds = {}
        self.commands = []
        for name in ('owner/one', 'owner/two'):
            source = Path(self.tmp.name) / name.replace('/', '-')
            self.git('init', '--bare', str(source))
            seed = Path(self.tmp.name) / ('seed-' + name.replace('/', '-'))
            self.git('init', '-b', 'main', str(seed))
            self.git('-C', str(seed), 'config', 'user.name', 'HomeStack tests')
            self.git('-C', str(seed), 'config', 'user.email', 'tests@example.invalid')
            (seed / 'README').write_text(f'{name} initial\n')
            (seed / '.gitignore').write_text('secret\n')
            self.git('-C', str(seed), 'add', 'README', '.gitignore')
            self.git('-C', str(seed), 'commit', '-m', 'Initial commit')
            self.git('-C', str(seed), 'remote', 'add', 'origin', str(source))
            self.git('-C', str(seed), 'push', '-u', 'origin', 'main')
            self.git('--git-dir', str(source), 'symbolic-ref', 'HEAD', 'refs/heads/main')
            self.sources[name] = source
            self.seeds[name] = seed
        from types import SimpleNamespace
        self.ws = SimpleNamespace(run=self.run_workspace)

    def git(self, *args):
        p = subprocess.run(['git', *args], text=True, capture_output=True)
        self.assertEqual(p.returncode, 0, p.stderr)
        return p

    def run_workspace(self, command, *, check=True):
        self.commands.append(command)
        command = command.replace('/home/user', str(self.home))
        if 'git ls-remote' in command:
            return subprocess.CompletedProcess([], 0, '', '')
        if 'git clone --' in command:
            words = shlex.split(command)
            url, destination = words[-2:]
            identity = repo.repository_from_remote(url)
            result = self.git('clone', '--', str(self.sources[identity]), destination)
            self.git('-C', destination, 'remote', 'set-url', 'origin', url)
            return result
        for identity, source in self.sources.items():
            command = command.replace(
                f'git@github.com:{identity}.git', str(source)
            )
        result = subprocess.run(['/bin/sh', '-c', command], text=True, capture_output=True, stdin=subprocess.DEVNULL)
        if check and result.returncode:
            raise AppError(f'Temporary Git fixture command failed ({result.returncode})')
        result.stdout = result.stdout.replace(str(self.home), "/home/user")
        return result

    def github(self, args, expected):
        endpoint = args[-1].split('?')[0]
        if endpoint.endswith('/keys'):
            identity = endpoint.removeprefix('repos/').removesuffix('/keys')
            return [list(self.keys.get(identity, []))]
        identity = endpoint.removeprefix('repos/')
        return {'full_name': identity, 'permissions': {'admin': True}}

    def add_key(self, identity, title, public):
        self.keys.setdefault(identity, []).append({'id': len(self.keys) + 1, 'title': title, 'key': public, 'read_only': False})

    def _setup_repository(self, identity='owner/one'):
        state = repo.inspect_repository(
            self.cfg, self.ws, 200, 'workspace', identity, verify_access=False
        )
        result = repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertTrue(result['ready'])
        return result

    def _advance_source(self, identity='owner/one'):
        seed = self.seeds[identity]
        marker = seed / 'advance'
        marker.write_text('new upstream content\n')
        self.git('-C', str(seed), 'add', 'advance')
        self.git('-C', str(seed), 'commit', '-m', 'Advance upstream')
        self.git('-C', str(seed), 'push', 'origin', 'main')

    def test_two_repositories_reuse_keys_isolate_ssh(self):
        with patch.object(repo, '_github_json', side_effect=self.github), patch.object(repo, '_add_key', side_effect=self.add_key), patch.object(repo, '_delete_key', side_effect=AssertionError('Unexpected key revocation')):
            for identity in self.sources:
                result = self._setup_repository(identity)
                self.assertTrue(result['changed'])
        identities = []
        for name in ('one', 'two'):
            identities.append(self.git('-C', str(self.home / 'DEV' / name), 'config', '--local', '--get', 'core.sshCommand').stdout)
        self.assertNotEqual(*identities)
        self.assertEqual(sum('git clone --' in c for c in self.commands), 2)
        self.assertFalse(any('git pull' in c or 'git reset' in c or 'git clean' in c for c in self.commands))

    def test_existing_dirty_repository_is_rejected(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            dirty = checkout / 'uncommitted'
            dirty.write_text('User data stays untouched')
            key_before = (self.home / '.ssh/homestack/github/owner-one').read_bytes()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one')
            self.assertEqual(state['working_tree'], 'modified')
            self.commands.clear()
            with self.assertRaisesRegex(AppError, 'uncommitted'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertEqual(dirty.read_text(), 'User data stays untouched')
        self.assertEqual((self.home / '.ssh/homestack/github/owner-one').read_bytes(), key_before)
        self.assertNotIn('fetch --no-tags', ' '.join(self.commands))

    def _repository_patches(self):
        return patch.object(repo, '_github_json', side_effect=self.github), patch.object(repo, '_add_key', side_effect=self.add_key), patch.object(repo, '_delete_key', side_effect=AssertionError('Unexpected key revocation'))

    def test_existing_up_to_date_repository_is_a_noop(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            self.commands.clear()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            result = repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertFalse(result['changed'])
        self.assertEqual(result['message'], 'Repository is already up to date.')
        self.assertIn('fetch --no-tags', ' '.join(self.commands))
        self.assertNotIn('merge --ff-only', ' '.join(self.commands))

    def test_existing_behind_repository_is_fast_forwarded(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            self._advance_source()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            result = repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        checkout = self.home / 'DEV/one'
        self.assertTrue(result['changed'])
        self.assertEqual(result['message'], 'Repository fast-forwarded to its upstream.')
        self.assertEqual(self.git('-C', str(checkout), 'rev-list', '--left-right', '--count', 'HEAD...@{upstream}').stdout.strip(), '0\t0')
        self.assertEqual(sum('merge --ff-only' in command for command in self.commands), 1)

    def test_existing_behind_repository_ignores_unsafe_merge_options(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'config', 'branch.main.mergeOptions', '--squash')
            self._advance_source()
            result = repo.setup_repository(
                self.cfg,
                self.ws,
                repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False),
                quiet=True,
            )
        self.assertTrue(result['changed'])
        self.assertEqual(self.git('-C', str(checkout), 'rev-list', '--left-right', '--count', 'HEAD...@{upstream}').stdout.strip(), '0\t0')
        merge = next(command for command in self.commands if 'merge --ff-only' in command)
        self.assertIn('--no-squash --no-autostash --no-overwrite-ignore', merge)
        self.assertIn('core.hooksPath=/dev/null', merge)

    def test_fast_forward_does_not_overwrite_an_ignored_untracked_file(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            secret = checkout / 'secret'
            secret.write_text('local ignored data\n')
            self.assertFalse(self.git('-C', str(checkout), 'status', '--porcelain').stdout)
            upstream_secret = self.seeds['owner/one'] / 'secret'
            upstream_secret.write_text('upstream data\n')
            self.git('-C', str(self.seeds['owner/one']), 'add', '-f', 'secret')
            self.git('-C', str(self.seeds['owner/one']), 'commit', '-m', 'Track ignored path')
            self.git('-C', str(self.seeds['owner/one']), 'push', 'origin', 'main')
            head_before = self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            with self.assertRaisesRegex(AppError, 'could not be fast-forwarded'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertEqual(secret.read_text(), 'local ignored data\n')
        self.assertEqual(self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip(), head_before)

    def test_existing_ahead_repository_is_rejected(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'config', 'user.name', 'HomeStack tests')
            self.git('-C', str(checkout), 'config', 'user.email', 'tests@example.invalid')
            (checkout / 'local').write_text('local commit\n')
            self.git('-C', str(checkout), 'add', 'local')
            self.git('-C', str(checkout), 'commit', '-m', 'Local commit')
            head_before = self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            self.commands.clear()
            with self.assertRaisesRegex(AppError, 'local commits'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertIn('fetch --no-tags', ' '.join(self.commands))
        self.assertNotIn('merge --ff-only', ' '.join(self.commands))
        self.assertEqual(self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip(), head_before)

    def test_setup_reports_rejected_repository_as_a_failed_item(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'config', 'user.name', 'HomeStack tests')
            self.git('-C', str(checkout), 'config', 'user.email', 'tests@example.invalid')
            (checkout / 'local').write_text('local commit\n')
            self.git('-C', str(checkout), 'add', 'local')
            self.git('-C', str(checkout), 'commit', '-m', 'Local commit')
            entry = setup_config.Entry(
                'owner/one',
                'repo',
                'repository',
                'One',
                'Repository',
                setup_config.RepositoryParams('owner/one'),
            )
            plan = setup.Plan({'vmid': 200, 'name': 'workspace'}, (entry,))
            with patch.object(setup, 'guest', return_value={'ok': True}):
                result = setup.execute_plan(self.cfg, plan, workspace=self.ws)
        self.assertFalse(result['ok'])
        self.assertEqual(result['results'][0]['status'], 'failed')
        self.assertIn('local commits ahead', result['results'][0]['detail'])

    def test_existing_diverged_repository_is_rejected(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'config', 'user.name', 'HomeStack tests')
            self.git('-C', str(checkout), 'config', 'user.email', 'tests@example.invalid')
            (checkout / 'local').write_text('local commit\n')
            self.git('-C', str(checkout), 'add', 'local')
            self.git('-C', str(checkout), 'commit', '-m', 'Local commit')
            head_before = self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip()
            self._advance_source()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            self.assertEqual((state['ahead'], state['behind']), (1, 0))
            with self.assertRaisesRegex(AppError, 'diverged'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertNotIn('merge --ff-only', ' '.join(self.commands))
        self.assertEqual(self.git('-C', str(checkout), 'rev-parse', 'HEAD').stdout.strip(), head_before)

    def test_existing_detached_head_is_rejected(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'checkout', '--detach', 'HEAD')
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            with self.assertRaisesRegex(AppError, 'detached HEAD'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)

    def test_existing_missing_upstream_is_rejected(self):
        with self._repository_patches()[0], self._repository_patches()[1], self._repository_patches()[2]:
            self._setup_repository()
            checkout = self.home / 'DEV/one'
            self.git('-C', str(checkout), 'branch', '--unset-upstream')
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            with self.assertRaisesRegex(AppError, 'no configured upstream'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)

    def test_permissions_and_read_only_key_block_preflight_without_mutations(self):
        entry = setup_config.Entry('owner/one', 'repo', 'repository', 'One', 'Repository', setup_config.RepositoryParams('owner/one'))
        plan = setup.Plan({'vmid': 200, 'name': 'workspace'}, (entry,))
        with patch.object(repo, '_github_json', return_value={'full_name': 'owner/one', 'permissions': {'admin': False}}):
            with self.assertRaisesRegex(AppError, 'permission'):
                setup.preflight_entry(self.ws, self.cfg, plan, entry)
        self.assertFalse(self.commands)
        state = {'tools': {'git': True, 'ssh': True, 'ssh-keygen': True}, 'checkout_state': 'ready', 'key_state': 'ready',
                 'deploy_key_state': 'read-only', 'origin': 'git@github.com:owner/one.git', 'ssh_config_state': 'ready',
                 'working_tree': 'clean', 'head_state': 'attached', 'branch': 'main',
                 'upstream': 'origin/main', 'upstream_remote': 'origin',
                 'upstream_merge': 'refs/heads/main',
                 'upstream_ref': 'refs/remotes/origin/main', 'ahead': 0, 'behind': 0}
        with patch.object(repo, '_github_json', return_value={'full_name': 'owner/one', 'permissions': {'admin': True}}), patch.object(setup, 'guest'), patch.object(repo, 'inspect_repository', return_value=state):
            with self.assertRaisesRegex(AppError, 'maintenance'):
                setup.preflight_entry(self.ws, self.cfg, plan, entry)

    def test_existing_non_git_directory_is_never_replaced(self):
        destination = self.home / 'DEV/one'
        destination.mkdir(parents=True)
        (destination / 'keep').write_text('Existing data')
        with patch.object(repo, '_github_json', side_effect=self.github):
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one', verify_access=False)
            with self.assertRaisesRegex(AppError, 'unsafe'):
                repo.setup_repository(self.cfg, self.ws, state, quiet=True)
        self.assertEqual((destination / 'keep').read_text(), 'Existing data')
