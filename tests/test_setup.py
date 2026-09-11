from __future__ import annotations

from contextlib import contextmanager, nullcontext
from dataclasses import replace
from io import StringIO
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch
import tomllib

from homestack import cli, config, install, setup, setup_catalog as catalog, setup_config as definitions, setup_guest as guest, workspace_ssh
from homestack.models import AppError
from support import test_config

TARGET = {"vmid": 200, "name": "workspace", "ip": "192.0.2.200", "home": "/home/user", "user": "user"}


def entry(identity):
    return next(e for e in definitions.defaults() if e.id == identity)


def completed(code=0, output=""):
    return subprocess.CompletedProcess([], code, output, "SECRET_OUTPUT")


class DefinitionTests(unittest.TestCase):
    def test_defaults_and_override_roundtrip(self):
        cfg = replace(test_config(), setup=definitions.parse_setup({"items": [{"id": "codex", "label": "My Codex"}]}), repo_sort="created")
        restored = config.validate_config_text(config.config_to_toml(cfg))
        self.assertEqual(cfg.setup, restored.setup)
        self.assertEqual(restored.repo_sort, "created")
        self.assertEqual(len([e for e in restored.setup.items if e.id == 'codex']), 1)
        self.assertEqual({e.id for e in restored.setup.items}, {'bash', 'zsh', 'fish', 'nu', 'codex', 'opencode', 'hermes'})

    def test_changed_command_loses_inherited_safety_claim(self):
        cfg = definitions.parse_setup({'items': [{'id': 'codex', 'command': 'custom SECRET'}]})
        p = next(e.params for e in cfg.items if e.id == 'codex')
        self.assertEqual(p.command, 'custom SECRET')
        self.assertEqual(p.interaction, 'interactive')
        self.assertFalse(p.check)
        self.assertFalse(p.non_interactive)

    def test_application_backup_paths_round_trip_and_validate_below_home(self):
        setup_cfg = definitions.parse_setup({'items': [{'id': 'codex', 'backup_paths': ['~/.config/codex/', '~/.codex/settings.json']}]})
        params = next(e.params for e in setup_cfg.items if e.id == 'codex')
        self.assertEqual(params.backup_paths, ('~/.config/codex/', '~/.codex/settings.json'))
        with self.assertRaises(AppError):
            definitions.parse_setup({'items': [{'id': 'codex', 'backup_paths': ['/tmp/outside']}]})

    def test_exact_legacy_payloads_and_codex_dedup(self):
        payload = '  printf "private value"\n'
        cfg = replace(test_config(), sync_commands=(definitions.CODEX_RECIPE, payload, payload), sync_paths=('~/notes',))
        loaded = config.validate_config_text(config.config_to_toml(cfg))
        self.assertEqual(loaded.sync_commands, cfg.sync_commands)
        items = definitions.effective_entries(loaded)
        self.assertEqual(sum(isinstance(e.params, definitions.ApplicationParams) and e.params.command == definitions.CODEX_RECIPE for e in items), 1)
        legacy = [e for e in items if e.id.startswith('legacy-app-')]
        self.assertEqual(len(legacy), 1)
        self.assertEqual(legacy[0].params.command, payload)
        self.assertEqual(legacy[0].params.interaction, 'interactive')
        self.assertEqual(legacy[0].id, definitions.legacy_id('app', payload))

    def test_draft_preserves_setup_and_repo_without_reconstruction(self):
        cfg = replace(test_config(), repo_owner='example', repo_sort='name', setup=definitions.parse_setup({'items': [{'id': 'codex', 'label': 'Custom label'}]}))
        text = install._install_draft_to_toml(cfg, {'transport'}, {})
        restored = install._config_from_install_draft(cfg.path, tomllib.loads(text))
        self.assertEqual(restored.setup, cfg.setup)
        self.assertEqual(restored.repo_owner, cfg.repo_owner)
        self.assertEqual(restored.repo_sort, cfg.repo_sort)

    def test_invalid_typed_definitions(self):
        for data in ({'items': 'bad'}, {'groups': 'bad'}, {'items': [{'id': 'codex', 'interpreter': 'sh'}]},
                     {'items': [{'id': 'codex', 'prerequisites': ['curl; exit 0']}]},
                     {'items': [{'id': 'codex', 'unknown': True}]}, {'items': [{'id': 'codex'}, {'id': 'codex'}]},
                     {'items': [{'id': 'bash', 'profile': 'powershell'}]}):
            with self.subTest(data=data), self.assertRaises(AppError):
                definitions.parse_setup(data)


class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.environment = patch.dict(os.environ, {'XDG_STATE_HOME': self.tmp.name})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.cfg = replace(test_config(), repo_owner='owner')
        self.identity = {'host': 'github.com', 'account': 'owner', 'account_id': 42, 'owner': 'owner'}

    def test_alias_sets_conflicts_all_and_invalid_inputs(self):
        self.assertEqual(catalog.parse_assignments(['f=0', 'e=bash,bash', 'env=bash'], self.cfg), {'files': ('all',), 'env': ('bash',)})
        for tokens in (['cred=0'], ['env=0,bash'], ['env=all,1'], ['env='], ['env=,bash'], ['env=bash', 'e=zsh'], ['env'], ['app=x=y']):
            with self.subTest(tokens=tokens), self.assertRaises(AppError):
                catalog.select_entries(self.cfg, tokens)
        for value in ('-1', '1.2', 'missing'):
            with self.assertRaises(AppError):
                catalog.select_entries(self.cfg, [f'env={value}'])

    def test_numeric_requires_snapshot_and_no_github_for_nonrepo(self):
        with self.assertRaisesRegex(AppError, 'setup list'):
            catalog.select_entries(self.cfg, ['env=1'])
        c = catalog.load_catalog(self.cfg)
        identifier = catalog.save_snapshot(self.cfg, c)
        with patch.object(catalog, 'github_identity', side_effect=AssertionError('unexpected GitHub')):
            selected, used = catalog.select_entries(self.cfg, ['e=1,2'], catalog_id=identifier)
        self.assertEqual([e.id for e in selected], ['bash', 'zsh'])
        self.assertEqual(used, identifier)
        with self.assertRaisesRegex(AppError, 'out of range'):
            catalog.select_entries(self.cfg, ['env=99'])

    def test_paginated_private_repos_sorts_null_dates_and_ties(self):
        def r(name, created, pushed, private=True):
            return {'full_name': name, 'created_at': created, 'pushed_at': pushed, 'private': private}
        rows = [[r('owner/z', '2025-01-01T00:00:00Z', None), r('other/x', '2026-01-01T00:00:00Z', None)],
                [r('owner/b', '2024-01-01T00:00:00Z', '2025-01-01T00:00:00Z'), r('owner/a', '2025-01-01T00:00:00Z', None)]]
        with patch.object(catalog, '_github_json', side_effect=[{'login': 'owner', 'id': 42}, rows]) as github:
            identity, repos = catalog.discover_repositories(self.cfg)
        self.assertEqual([r['full_name'] for r in repos], ['owner/a', 'owner/b', 'owner/z'])
        self.assertTrue(all(r['private'] for r in repos))
        self.assertIn('--paginate', github.call_args.args[0])
        self.assertIn('--slurp', github.call_args.args[0])
        for sort, expected in [('created', ['owner/a', 'owner/z', 'owner/b']), ('name', ['owner/a', 'owner/b', 'owner/z'])]:
            with patch.object(catalog, '_github_json', side_effect=[{'login': 'owner', 'id': 42}, rows]):
                _, repos = catalog.discover_repositories(replace(self.cfg, repo_sort=sort))
            self.assertEqual([r['full_name'] for r in repos], expected)

    def test_immutable_snapshot_no_numeric_drift_and_bindings(self):
        repositories = [definitions.Entry(name, 'repo', 'repository', name, 'Repository', definitions.RepositoryParams(name)) for name in ['owner/first', 'owner/second']]
        c = catalog.Catalog((*definitions.defaults(), *repositories), {}, github=self.identity)
        first = catalog.save_snapshot(self.cfg, c)
        c.entries = (*definitions.defaults(), *reversed(repositories))
        second = catalog.save_snapshot(self.cfg, c)
        with patch.object(catalog, 'github_identity', return_value=self.identity), patch.object(catalog, 'discover_repositories', side_effect=AssertionError('renumbered')):
            self.assertEqual(catalog.select_entries(self.cfg, ['repo=1'], catalog_id=first)[0][0].id, 'owner/first')
            self.assertEqual(catalog.select_entries(self.cfg, ['repo=1'])[0][0].id, 'owner/second')
        self.assertNotEqual(first, second)
        for cfg in (replace(self.cfg, sync_commands=('private command',)), replace(self.cfg, path=Path('/tmp/different.toml'))):
            with self.assertRaises(AppError):
                catalog.select_entries(cfg, ['env=1'], catalog_id=first)
        with patch.object(catalog, 'github_identity', return_value={**self.identity, 'account_id': 9}), self.assertRaisesRegex(AppError, 'identity'):
            catalog.select_entries(self.cfg, ['repo=1'], catalog_id=first)
        text = (catalog.snapshot_directory(self.cfg) / f'{first}.json').read_text()
        self.assertNotIn(definitions.CODEX_RECIPE, text)
        self.assertNotIn('command', text)

    def test_discovery_failure_does_not_hide_other_groups(self):
        with patch.object(catalog, 'discover_repositories', side_effect=AppError('Unavailable')):
            c = catalog.load_catalog(self.cfg, repositories=True)
        self.assertEqual(c.repository_error, 'Unavailable')
        self.assertIn('bash', [e.id for e in c.entries])
        self.assertTrue(all(row['guest_state'] == 'unknown' for row in c.rows(self.cfg)))


class PlanTests(unittest.TestCase):
    def test_repository_clone_never_snapshots_checkout_contents(self):
        cfg = test_config()
        repository = definitions.Entry(
            'owner/project', 'repo', 'repository', 'Project', 'Repository',
            definitions.RepositoryParams('owner/project'),
        )
        checkout = f'/home/{cfg.user_name}/src/project'

        self.assertEqual(setup.backup_paths_for_entry(cfg, repository, {
            'repository': {'checkout_state': 'missing', 'checkout': checkout},
            'actions': ('generate-key', 'clone', 'verify-access'),
        }), ())
        self.assertEqual(setup.backup_paths_for_entry(cfg, repository, {
            'repository': {'checkout_state': 'ready', 'checkout': checkout},
            'actions': ('set-origin', 'verify-access'),
        }), ('src/project/.git/config',))

    def test_independent_apps_repositories_and_explicit_dependencies(self):
        cfg = replace(test_config(), sync_paths=('~/missing',))
        self.assertEqual(setup.build_plan(cfg, TARGET, (entry('codex'),)).entries, (entry('codex'),))
        selected, _ = catalog.select_entries(cfg, ['repo=owner/project'])
        self.assertEqual(setup.build_plan(cfg, TARGET, selected).entries, selected)
        custom = replace(entry('codex'), depends_on=('bash',))
        with self.assertRaisesRegex(AppError, 'explicit selection'):
            setup.build_plan(cfg, TARGET, (custom,))
        self.assertEqual([e.id for e in setup.build_plan(cfg, TARGET, (custom, entry('bash'))).entries], ['bash', 'codex'])

    def test_selected_overlap_missing_source_and_source_symlink(self):
        cfg = test_config()
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            home = Path(tmp)
            (home / '.bashrc').write_text('custom')
            file = definitions.Entry('dotfile', 'files', 'file', 'Dotfile', 'A file', definitions.FileParams('~/.bashrc'))
            with self.assertRaisesRegex(AppError, 'Overlapping'):
                setup.build_plan(cfg, TARGET, (file, entry('bash')))
            (home / 'link').symlink_to(home, target_is_directory=True)
            file = replace(file, params=definitions.FileParams('~/link/.bashrc'))
            with self.assertRaisesRegex(AppError, 'symlink'):
                setup.build_plan(cfg, TARGET, (file,))

    def test_repo_destination_collision(self):
        selected, _ = catalog.select_entries(test_config(), ['repo=owner/project,another/project'])
        with self.assertRaisesRegex(AppError, 'Overlapping'):
            setup.build_plan(test_config(), TARGET, selected)

    def test_unattended_interactive_rejected_even_yes_no_implicit_actions(self):
        custom = replace(entry('codex'), params=definitions.ApplicationParams('read -r answer'))
        with self.assertRaisesRegex(AppError, 'interactive terminal'):
            setup.build_plan(test_config(), TARGET, (custom,), unattended=True)
        safe = replace(custom, params=replace(custom.params, non_interactive='printf ready'))
        setup.build_plan(test_config(), TARGET, (safe,), unattended=True)
        with self.assertRaises(AppError):
            setup.build_plan(test_config(), TARGET, ())


class EngineTests(unittest.TestCase):
    def test_execution_activity_reports_preflight_snapshot_shell_write_and_state_record(self):
        cfg = test_config()
        bash = entry('bash')
        plan = setup.build_plan(cfg, TARGET, (bash,))
        ws = Mock()
        events = []

        def guest_operation(_ws, _cfg, operation, **values):
            if operation == 'snapshot':
                return {'created': True, 'id': 'snapshot-id', 'path': '~/.local/state/homestack/snapshot-id'}
            if operation == 'environment':
                return {'changed': ['.profile']}
            return {}

        state = {'changed': ['.profile'], 'paths': ['.bashrc', '.profile']}
        with patch.object(setup, 'require_tool'), \
                patch.object(setup, 'preflight_entry', return_value=state), \
                patch.object(setup, 'guest', side_effect=guest_operation):
            result = setup.execute_plan(
                cfg,
                plan,
                workspace=ws,
                activity=lambda identity, message: events.append((identity, message)),
            )

        self.assertTrue(result['ok'])
        messages = [message for _identity, message in events]
        for expected in (
            'Verify workspace connection',
            'Preflight Bash',
            'Create operation snapshot',
            'Operation snapshot created: ~/.local/state/homestack/snapshot-id',
            'Write Bash shell configuration',
            'Record setup state',
            'Setup execution complete',
        ):
            self.assertIn(expected, messages)

    def test_all_preflight_before_mutation_one_connection_and_stop_on_failure(self):
        cfg = test_config()
        plan = setup.build_plan(cfg, TARGET, (entry('bash'), entry('codex'), entry('opencode')))
        ws = Mock()
        factory = Mock(return_value=ws)
        ws.__enter__ = Mock(return_value=ws)
        ws.__exit__ = Mock(return_value=False)
        seen = []
        def preflight(w, c, p, e):
            self.assertIs(w, ws)
            seen.append('check:' + e.id)
            return {}
        def apply(w, c, p, e, state, terminal, **kwargs):
            self.assertEqual(seen[:3], ['check:bash', 'check:codex', 'check:opencode'])
            seen.append('apply:' + e.id)
            if e.id == 'codex':
                raise AppError('Installer failed')
            return 'succeeded', 'Verified'
        with patch.object(setup, 'require_tool'), patch.object(setup, 'guest'), patch.object(setup, 'preflight_entry', side_effect=preflight), patch.object(setup, 'apply_entry', side_effect=apply):
            result = setup.execute_plan(cfg, plan, connection_factory=factory)
        self.assertEqual([r['status'] for r in result['results']], ['succeeded', 'failed', 'not-run'])
        factory.assert_called_once()
        ws.__exit__.assert_called_once()

    def test_blocked_preflight_prevents_all_selected_writes(self):
        cfg = test_config()
        plan = setup.build_plan(cfg, TARGET, (entry('bash'), entry('codex')))
        ws = Mock()
        with patch.object(setup, 'require_tool'), patch.object(setup, 'guest'), patch.object(setup, 'preflight_entry', side_effect=[AppError('Missing bash'), {}]), patch.object(setup, 'apply_entry') as apply:
            result = setup.execute_plan(cfg, plan, connection_factory=lambda c,t: nullcontext(ws))
        apply.assert_not_called()
        self.assertEqual([r['status'] for r in result['results']], ['blocked', 'not-run'])

    def test_failed_download_pipeline_real_shell_and_no_output_leak(self):
        cfg = test_config()
        # HOME/cwd are replaced only inside this local execution test, never production.
        with tempfile.TemporaryDirectory() as tmp:
            def run(command, **kwargs):
                command = command.replace('/home/user', tmp)
                return subprocess.run(shlex.split(command), text=True, capture_output=True)
            ws = Mock()
            ws.run.side_effect = run
            p = definitions.ApplicationParams('curl -fsSL file:///a-file-that-does-not-exist | sh', interaction='non-interactive')
            custom = replace(entry('codex'), params=p)
            with self.assertRaisesRegex(AppError, 'Installer failed') as raised:
                setup.apply_entry(ws, cfg, setup.Plan(TARGET, (custom,), unattended=True), custom, {}, nullcontext)
            self.assertNotIn('SECRET', str(raised.exception))

    def test_installed_application_selection_runs_update_and_failed_postcheck_stops(self):
        cfg = test_config()
        ws = Mock()
        ws.run.return_value = completed()
        with patch.object(setup, 'require_tool'), patch.object(setup, 'guest', return_value={'ok': True}):
            state = setup.preflight_entry(ws, cfg, setup.Plan(TARGET, (entry('codex'),)), entry('codex'))
        self.assertTrue(state['installed'])
        ws.run.reset_mock()
        ws.run.side_effect = [completed(), completed()]
        status, detail = setup.apply_entry(ws, cfg, setup.Plan(TARGET, (entry('codex'),)), entry('codex'), state, nullcontext)
        self.assertEqual(status, 'succeeded')
        self.assertIn('updated', detail)
        self.assertEqual(ws.run.call_count, 2)
        ws.run.side_effect = [completed(), completed(1)]
        with self.assertRaisesRegex(AppError, 'verification failed'):
            setup.apply_entry(ws, cfg, setup.Plan(TARGET, (entry('codex'),)), entry('codex'), {'installed': False}, nullcontext)

    def test_interactive_pty_handoff_restores_on_failure(self):
        events = []
        def terminal(operation):
            events.append('suspend')
            try:
                return operation()
            finally:
                events.append('restore')
        custom = replace(entry('codex'), params=definitions.ApplicationParams('read answer'))
        ws = Mock()
        ws.run.return_value = completed(4)
        with self.assertRaises(AppError):
            setup.apply_entry(ws, test_config(), setup.Plan(TARGET, (custom,)), custom, {}, terminal)
        self.assertEqual(events, ['suspend', 'restore'])
        self.assertTrue(ws.run.call_args.kwargs['interactive'])

    def test_transport_failure_is_not_missing_tool(self):
        ws = Mock()
        ws.run.return_value = completed(255)
        with self.assertRaisesRegex(AppError, 'transport'):
            setup.require_tool(ws, test_config(), 'rsync')


class SSHTests(unittest.TestCase):
    def test_one_master_private_socket_options_and_fail_closed_children(self):
        calls = []
        def run(argv, **kwargs):
            calls.append((argv, kwargs))
            return completed()
        options = ('-F', '/dev/null', '-o', 'IdentityAgent=none', '-o', 'IdentitiesOnly=yes', '-i', '/tmp/test-key')
        with patch('subprocess.run', side_effect=run), patch.object(workspace_ssh.shutil, 'which', return_value='/usr/bin/ssh'):
            with workspace_ssh.WorkspaceSSH('user@192.0.2.200', 200, options=options) as ws:
                directory = Path(ws.control).parent
                self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
                ws.run('true')
                ws.run('read answer', interactive=True)
                ws.transfer('/tmp/source', '/home/user/file')
            self.assertFalse(directory.exists())
        masters = [args for args, kwargs in calls if '-M' in args]
        self.assertEqual(len(masters), 1)
        self.assertIn('ControlPersist=yes', masters[0])
        children = [args for args, kwargs in calls if args[0] == 'ssh' and '-M' not in args]
        self.assertTrue(all('ProxyCommand=false' in args and 'PubkeyAuthentication=no' in args for args in children))
        self.assertTrue(all('/tmp/test-key' in args and 'IdentityAgent=none' in args and '/dev/null' in args for args in children))
        pty = next((args, kwargs) for args, kwargs in calls if '-tt' in args)
        self.assertNotIn('input', pty[1])
        self.assertNotIn('stdin', pty[1])
        rsync = next(args for args, kwargs in calls if args[0] == 'rsync')
        self.assertIn('ProxyCommand=false', rsync[rsync.index('-e') + 1])
        self.assertNotIn('--delete', rsync)

    def test_master_loss_and_cancellation_cleanup(self):
        ws = workspace_ssh.WorkspaceSSH('workspace', 200)
        with patch('subprocess.run', return_value=completed()), patch.object(workspace_ssh, 'run_local', return_value=completed()):
            with self.assertRaises(KeyboardInterrupt):
                with ws:
                    raise KeyboardInterrupt
            self.assertFalse(ws.opened)
        with self.assertRaisesRegex(AppError, 'master'):
            ws.run('true')


class CLITests(unittest.TestCase):
    def invoke(self, argv, cfg=None, *, tty=False):
        output = StringIO()
        with patch('sys.argv', ['homestack', *argv]), patch('sys.stdout', output), patch('sys.stdin.isatty', return_value=tty), patch.object(cli, 'load_config', return_value=cfg or test_config()):
            code = cli.main()
        return code, output.getvalue()

    def test_list_json_without_herdr_or_guest(self):
        from homestack import setup_cli
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {'XDG_STATE_HOME': tmp}), patch.object(setup_cli, 'open_transport', side_effect=AssertionError('Herdr')), patch.object(catalog, 'discover_repositories', side_effect=AppError('Unavailable')):
            code, output = self.invoke(['setup', 'list', '--json'])
        result = json.loads(output)
        self.assertEqual(code, 0)
        self.assertTrue(result['catalog'])
        self.assertTrue(result['items'])
        self.assertEqual(result['guest_state'], 'unknown')

    def test_missing_target_selectors_and_invalid_input_no_transport(self):
        from homestack import setup_cli
        with patch.object(setup_cli, 'open_transport', side_effect=AssertionError('Herdr')):
            for args in (['setup', '--json'], ['setup', 'workspace', '--yes', '--json'], ['setup', 'workspace', 'app=missing', '--yes', '--json'], ['setup', 'workspace', 'f=1', 'files=2', '--json']):
                code, output = self.invoke(args)
                self.assertEqual(code, 1)
                self.assertFalse(json.loads(output)['ok'])

    def test_dry_run_confirmation_and_unattended_json(self):
        from homestack import setup_cli
        with patch.object(setup_cli, 'open_transport'), patch.object(setup_cli, 'resolve_target', return_value=TARGET), patch.object(setup_cli, 'execute_plan', return_value={'ok': True, 'results': []}) as execute:
            code, output = self.invoke(['setup', 'workspace', 'env=bash', 'app=codex', '--dry-run', '--json'])
            self.assertEqual(code, 0)
            self.assertTrue(json.loads(output)['dry_run'])
            execute.assert_not_called()
            code, output = self.invoke(['setup', '200', 'env=bash', '--json'])
            self.assertEqual(code, 3)
            self.assertTrue(json.loads(output)['confirmation_required'])
            execute.assert_not_called()
            code, output = self.invoke(['setup', 'workspace', 'app=codex', '--yes', '--non-interactive', '--json'])
            self.assertEqual(code, 0)
            self.assertTrue(json.loads(output)['ok'])
            self.assertTrue(execute.call_args.args[1].unattended)

    def test_explicit_selectors_never_enter_tui_and_tty_review_can_cancel(self):
        from homestack import setup_cli
        with patch.object(setup_cli, 'open_transport'), patch.object(setup_cli, 'resolve_target', return_value=TARGET), patch.object(setup_cli.Confirm, 'ask', return_value=False), patch.object(setup_cli, 'execute_plan') as execute, patch('homestack.setup_tui.SetupApp', side_effect=AssertionError('Unexpected TUI')):
            code, output = self.invoke(['setup', 'workspace', 'env=bash'], tty=True)
        self.assertEqual(code, 0)
        self.assertIn('Cancelled', output)
        execute.assert_not_called()


class EnvironmentTests(unittest.TestCase):
    def apply(self, home, profile='bash'):
        return guest.run({'operation': 'environment', 'home': str(home), 'profile': profile, 'bins': ['~/.local/bin', '~/.opencode/bin'], 'apply': True})

    def preflight(self, home, profile='bash'):
        return guest.run({'operation': 'environment', 'home': str(home), 'profile': profile,
                          'bins': ['~/.local/bin', '~/.opencode/bin'], 'apply': False})

    def test_bash_generates_canonical_files_replaces_existing_content_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / '.bashrc').write_text('export CUSTOM_BASHRC=preserved\n')
            (home / '.bash_profile').write_text('export CUSTOM_LOGIN=preserved\n')
            (home / '.profile').write_text('export WRONG_LOGIN=used\n')
            before = {(home / name): (home / name).read_bytes() for name in ('.bashrc', '.bash_profile', '.profile')}
            inodes = {(home / name): (home / name).stat().st_ino for name in ('.bashrc', '.bash_profile', '.profile')}
            self.assertEqual(self.preflight(home)['changed'], ['.bashrc', '.bash_profile'])
            self.assertEqual({path: path.read_bytes() for path in before}, before)
            self.assertEqual({path: path.stat().st_ino for path in inodes}, inodes)

            result = self.apply(home)
            self.assertEqual(result['changed'], ['.bashrc', '.bash_profile'])
            expected = guest.environment_updates(home, 'bash', ['~/.local/bin', '~/.opencode/bin'])
            self.assertEqual((home / '.bashrc').read_text(), expected['.bashrc'])
            self.assertEqual((home / '.bash_profile').read_text(), expected['.bash_profile'])
            self.assertNotIn('CUSTOM_BASHRC', (home / '.bashrc').read_text())
            self.assertNotIn('CUSTOM_LOGIN', (home / '.bash_profile').read_text())
            self.assertEqual((home / '.profile').read_bytes(), before[home / '.profile'])
            first = (home / '.bashrc').read_bytes()
            self.assertEqual(self.apply(home)['changed'], [])
            self.assertEqual(first, (home / '.bashrc').read_bytes())
            self.assertFalse(list(home.glob('.bashrc.homestack-backup-*')))
            env = {**os.environ, 'HOME': str(home), 'PATH': '/usr/bin:/bin'}
            for args in (['bash', '--noprofile', '--rcfile', str(home / '.bashrc'), '-ic'], ['bash', '--noprofile', '-lc']):
                # -noprofile avoids the desktop's /etc/profile resetting the temporary HOME.
                command = ('source "$HOME/.bash_profile"; ' if '-lc' in args else '') + 'printf "%s|%s|%s" "${CUSTOM_BASHRC:-}" "${CUSTOM_LOGIN:-}" "$PATH"'
                result = subprocess.run([*args, command], env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('preserved', result.stdout)
                self.assertEqual(result.stdout.count(str(home / '.local/bin')), 1)
            self.assertEqual((home / '.profile').read_text(), 'export WRONG_LOGIN=used\n')

    def test_bash_login_precedence_and_unsafe_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            self.assertEqual(set(self.apply(home)['paths']), {'.bashrc', '.profile'})
            (home / '.bashrc').unlink()
            (home / '.bashrc').symlink_to(home / '.profile')
            with self.assertRaisesRegex(guest.GuestError, 'Symlink'):
                self.apply(home)
            (home / '.bashrc').unlink()
            (home / '.bash_profile').symlink_to(home / '.profile')
            with self.assertRaisesRegex(guest.GuestError, 'Symlink'):
                self.apply(home)

    def test_non_bash_templates_replace_existing_content_deterministically(self):
        cases = {
            'zsh': ('.zshenv', '.zshrc'),
            'fish': ('.config/fish/conf.d/homestack.fish',),
            'nu': ('.config/nushell/env.nu', '.config/nushell/config.nu'),
        }
        for profile, relatives in cases.items():
            with self.subTest(profile=profile), tempfile.TemporaryDirectory() as tmp:
                home = Path(tmp)
                for relative in relatives:
                    path = home / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(f'custom {profile}\n')
                updates = guest.environment_updates(home, profile, ['~/.local/bin', '~/.opencode/bin'])
                self.assertEqual(tuple(updates), relatives)
                for relative, content in updates.items():
                    self.assertNotIn(f'custom {profile}', content)
                    self.assertTrue(guest.atomic_write(home / relative, content))
                    self.assertEqual((home / relative).read_text(), content)
                    self.assertFalse(guest.atomic_write(home / relative, content))

    def test_atomic_write_is_noop_when_unchanged_and_never_creates_sidecar_backups(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / '.profile'
            p.write_text('custom\n')
            self.assertFalse(guest.atomic_write(p, 'custom\n'))
            self.assertTrue(guest.atomic_write(p, 'custom\nnew\n'))
            self.assertEqual(p.read_text(), 'custom\nnew\n')
            self.assertFalse(list(Path(tmp).glob('*.homestack-backup-*')))

    def test_destination_symlink_ancestor_and_recursive_conflict(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / 'real').mkdir()
            (home / 'link').symlink_to(home / 'real')
            with self.assertRaisesRegex(guest.GuestError, 'Symlink'):
                guest.safe_path(home, 'link/file')
            (home / 'real/nested').symlink_to('/tmp')
            with self.assertRaisesRegex(guest.GuestError, 'symlink'):
                guest.safe_path(home, 'real', directory=True, recursive=True)

    @unittest.skipUnless(shutil.which('zsh'), 'Zsh is not installed on the test desktop')
    def test_zsh_profile_syntax_behavior(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.apply(Path(tmp), 'zsh')
            result = subprocess.run(['zsh', '-lic', 'print -r -- $PATH'], env={**os.environ, 'ZDOTDIR': tmp, 'HOME': tmp}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.count(tmp + '/.local/bin'), 1)

    @unittest.skipUnless(shutil.which('fish'), 'Fish is not installed on the test desktop')
    def test_fish_profile_syntax_behavior(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.apply(Path(tmp), 'fish')
            result = subprocess.run(['fish', '-lic', 'string join : $PATH'], env={**os.environ, 'HOME': tmp, 'XDG_CONFIG_HOME': tmp + '/.config'}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.count(tmp + '/.local/bin'), 1)

    @unittest.skipUnless(shutil.which('nu'), 'Nushell is not installed on the test desktop')
    def test_nushell_profile_syntax_behavior(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.apply(Path(tmp), 'nu')
            result = subprocess.run(['nu', '--env-config', tmp + '/.config/nushell/env.nu', '--config', tmp + '/.config/nushell/config.nu', '-l', '-c', '$env.PATH | str join ":"'], env={**os.environ, 'HOME': tmp}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.count(tmp + '/.local/bin'), 1)


class AdditionalSafetyTests(unittest.TestCase):
    def test_actual_identity_guard_rejects_hostname_and_mount_mismatches(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            data = {'home': str(home), 'user': 'user', 'uid': os.getuid(), 'gid': os.getgid(), 'name': 'expected', 'vmid': 200}
            with patch.dict(os.environ, {'HOME': str(home)}), patch('pwd.getpwuid', return_value=SimpleNamespace(pw_name='user', pw_dir=str(home))), patch.object(guest.subprocess, 'run', return_value=completed(output='different\n')):
                with self.assertRaisesRegex(guest.GuestError, 'hostname'):
                    guest.verify_identity(data)
            with patch.dict(os.environ, {'HOME': str(home)}), patch('pwd.getpwuid', return_value=SimpleNamespace(pw_name='user', pw_dir=str(home))), patch.object(guest.subprocess, 'run', side_effect=[completed(output='expected\n'), completed(output='/ ext4 root\n')]):
                with self.assertRaisesRegex(guest.GuestError, 'mount'):
                    guest.verify_identity(data)

    def test_master_disappears_without_running_or_authenticating_child(self):
        ws = workspace_ssh.WorkspaceSSH('user@192.0.2.200', 200)
        ws.opened = True
        ws.control = '/tmp/nonexistent-owned-test-master'
        with patch.object(workspace_ssh, 'run_local', return_value=completed(255)), patch('subprocess.run') as run:
            with self.assertRaisesRegex(AppError, 'master'):
                ws.run('true')
        run.assert_not_called()

    def test_unattended_guard_is_enforced_by_execution_service(self):
        custom = replace(entry('codex'), params=definitions.ApplicationParams('read answer'))
        connection = Mock(side_effect=AssertionError('Unexpected authentication'))
        result = setup.execute_plan(test_config(), setup.Plan(TARGET, (custom,), unattended=True), connection_factory=connection)
        self.assertFalse(result['ok'])
        self.assertEqual(result['results'][0]['status'], 'blocked')
        connection.assert_not_called()

    def test_reserved_ids_and_typed_arrays_fail_as_application_errors(self):
        for raw in ({'groups': [{'id': 'bash', 'label': 'Other'}]}, {'items': [{'id': 'codex', 'interaction': []}]},
                    {'items': [{'id': 'bash', 'profile': []}]}, {'items': [{'id': 'codex', 'bin_dirs': ['~/a:/etc']}]},
                    {'items': [{'id': 'codex', 'prerequisite_checks': ['a\0b']}]}):
            with self.subTest(raw=raw), self.assertRaises(AppError):
                definitions.parse_setup(raw)

    def test_same_connection_serves_actual_environment_file_and_application_handlers(self):
        cfg = test_config()
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            (Path(tmp) / 'fixture').write_text('Synthetic fixture')
            file = definitions.Entry('fixture', 'files', 'file', 'Fixture', 'Synthetic file', definitions.FileParams('~/fixture'))
            application = replace(entry('codex'), params=definitions.ApplicationParams('printf done', interaction='non-interactive'))
            plan = setup.build_plan(cfg, TARGET, (file, application, entry('bash')))
            ws = Mock()
            ws.run.return_value = completed()
            operations = []
            def remote(connection, c, operation, **params):
                self.assertIs(connection, ws)
                operations.append((operation, params))
                return {'ok': True, 'changed': ['.bashrc']}
            with patch.object(setup, 'guest', side_effect=remote):
                result = setup.execute_plan(cfg, plan, connection_factory=lambda c,t: nullcontext(ws))
            self.assertTrue(result['ok'])
            ws.transfer.assert_called_once_with(str(Path(tmp) / 'fixture'), '/home/user/fixture')
            self.assertTrue(any(op == 'environment' and args.get('apply') for op, args in operations))
            self.assertTrue(any('printf done' in call.args[0] for call in ws.run.call_args_list))
