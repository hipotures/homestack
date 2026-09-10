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
        self.commands = []
        for name in ('owner/one', 'owner/two'):
            source = Path(self.tmp.name) / name.replace('/', '-')
            self.git('init', '--bare', str(source))
            self.sources[name] = source
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

    def test_two_repositories_reuse_keys_isolate_ssh_and_preserve_dirty_worktrees(self):
        with patch.object(repo, '_github_json', side_effect=self.github), patch.object(repo, '_add_key', side_effect=self.add_key), patch.object(repo, '_delete_key', side_effect=AssertionError('Unexpected key revocation')):
            for identity in self.sources:
                state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', identity, verify_access=False)
                result = repo.setup_repository(self.cfg, self.ws, state, quiet=True)
                self.assertTrue(result['ready'])
                self.assertTrue(result['changed'])
            one = self.home / 'DEV/one'
            (one / 'uncommitted').write_text('User data stays untouched')
            key_before = (self.home / '.ssh/homestack/github/owner-one').read_bytes()
            state = repo.inspect_repository(self.cfg, self.ws, 200, 'workspace', 'owner/one')
            self.assertEqual(state['working_tree'], 'modified')
            result = repo.setup_repository(self.cfg, self.ws, state, quiet=True)
            self.assertFalse(result['changed'])
            self.assertEqual((one / 'uncommitted').read_text(), 'User data stays untouched')
            self.assertEqual((self.home / '.ssh/homestack/github/owner-one').read_bytes(), key_before)
        identities = []
        for name in ('one', 'two'):
            identities.append(self.git('-C', str(self.home / 'DEV' / name), 'config', '--local', '--get', 'core.sshCommand').stdout)
        self.assertNotEqual(*identities)
        self.assertEqual(sum('git clone --' in c for c in self.commands), 2)
        self.assertFalse(any('git pull' in c or 'git reset' in c or 'git clean' in c for c in self.commands))

    def test_permissions_and_read_only_key_block_preflight_without_mutations(self):
        entry = setup_config.Entry('owner/one', 'repo', 'repository', 'One', 'Repository', setup_config.RepositoryParams('owner/one'))
        plan = setup.Plan({'vmid': 200, 'name': 'workspace'}, (entry,))
        with patch.object(repo, '_github_json', return_value={'full_name': 'owner/one', 'permissions': {'admin': False}}):
            with self.assertRaisesRegex(AppError, 'permission'):
                setup.preflight_entry(self.ws, self.cfg, plan, entry)
        self.assertFalse(self.commands)
        state = {'tools': {'git': True, 'ssh': True, 'ssh-keygen': True}, 'checkout_state': 'ready', 'key_state': 'ready',
                 'deploy_key_state': 'read-only', 'origin': 'git@github.com:owner/one.git', 'ssh_config_state': 'ready'}
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
