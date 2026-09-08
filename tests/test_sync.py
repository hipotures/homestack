from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import cli, config, models, sync

from support import test_config

class SyncTests(unittest.TestCase):

    @staticmethod
    def successful_plan(*, commands: tuple[str, ...]=()) -> dict[str, object]:
        plan: dict[str, object] = {'command': 'sync', 'vmid': 200, 'name': 'test1', 'node': 'example-node-1', 'status': 'running', 'ip': '192.0.2.200', 'user': 'user', 'target_home': '/home/user', 'configured': 1, 'ready': 1, 'preflight_failed': 0, 'transfer': 'rsync over SSH', 'verbose': False, 'delete': False, 'config': '/tmp/test.toml', 'items': [{'path': '~/.config/example/settings.json', 'relative': '.config/example/settings.json', 'type': 'file', 'is_directory': False, 'local_path': '/tmp/settings.json', 'destination': '/home/user/.config/example/settings.json', 'status': 'ready', 'detail': ''}]}
        if commands:
            plan['commands'] = list(commands)
            plan['commands_configured'] = len(commands)
        return plan

    def run_successful_path_sync(self, commands: tuple[str, ...]=(), *, failed_command: str | None=None) -> tuple[dict[str, object], list[list[str]]]:
        cfg = replace(test_config(), sync_paths=('~/.config/example/settings.json',), sync_commands=commands)
        calls: list[list[str]] = []

        def fake_run(cmd: list[str], *, check: bool=True):
            calls.append(cmd)
            returncode = 9 if cmd[-1] == failed_command else 0
            return __import__('subprocess').CompletedProcess(
                cmd, returncode, 'OUTPUT_MARKER_MUST_NOT_LEAK', 'ERROR_MARKER_MUST_NOT_LEAK'
            )
        master_ok = __import__('subprocess').CompletedProcess(['ssh'], 0, '', '')
        with patch.object(sync, 'run_local_passthrough', return_value=master_ok), patch.object(sync, 'run_local', side_effect=fake_run):
            result = sync.sync_workspace(cfg, self.successful_plan(commands=commands), json_mode=True)
        return (result, calls)

    def test_sync_path_spec_uses_trailing_slash_for_directory(self) -> None:
        self.assertEqual(config.validate_sync_path_spec('~/.config/example/settings.json'), ('.config/example/settings.json', False))
        self.assertEqual(config.validate_sync_path_spec('~/.config/example-tool/'), ('.config/example-tool', True))
        for invalid in ('/tmp/outside', '~/', '~/.config/../parent', '~/.config//item'):
            with self.assertRaises(models.AppError):
                config.validate_sync_path_spec(invalid)

    def test_sync_plan_item_checks_file_and_directory_type(self) -> None:
        cfg = test_config()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / '.config' / 'example').mkdir(parents=True)
            (home / '.config' / 'example' / 'settings.json').write_text('example', encoding='utf-8')
            (home / '.config' / 'example-tool').mkdir(parents=True)
            with patch.object(Path, 'home', return_value=home):
                file_item = sync.sync_plan_item(cfg, '~/.config/example/settings.json')
                dir_item = sync.sync_plan_item(cfg, '~/.config/example-tool/')
                mismatch = sync.sync_plan_item(cfg, '~/.config/example-tool')
                missing = sync.sync_plan_item(cfg, '~/.missing/item')
            self.assertEqual(file_item['status'], 'ready')
            self.assertEqual(file_item['type'], 'file')
            self.assertEqual(file_item['destination'], '/home/user/.config/example/settings.json')
            self.assertEqual(dir_item['status'], 'ready')
            self.assertEqual(dir_item['type'], 'directory')
            self.assertEqual(dir_item['destination'], '/home/user/.config/example-tool/')
            self.assertEqual(mismatch['status'], 'type mismatch')
            self.assertEqual(missing['status'], 'missing')

    def test_sync_parser_accepts_vmid_or_name(self) -> None:
        parser = cli.build_parser()
        numeric = parser.parse_args(['sync', '200'])
        named = parser.parse_args(['sync', 'test1'])
        self.assertEqual(numeric.command, 'sync')
        self.assertEqual(numeric.target, '200')
        self.assertEqual(named.target, 'test1')

    def test_sync_without_ready_sources_returns_report_failure_without_ssh(self) -> None:
        cfg = replace(test_config(), sync_paths=('~/.missing/item',), sync_commands=('printf never',))
        plan = {'command': 'sync', 'vmid': 200, 'name': 'test1', 'node': 'example-node-2', 'status': 'running', 'ip': '192.0.2.200', 'user': 'user', 'target_home': '/home/user', 'configured': 1, 'ready': 0, 'preflight_failed': 1, 'transfer': 'rsync over SSH', 'delete': False, 'config': '/tmp/test.toml', 'commands': ['printf never'], 'commands_configured': 1, 'items': [{'path': '~/.missing/item', 'relative': '.missing/token', 'type': 'file', 'is_directory': False, 'local_path': '/tmp/missing/item', 'destination': '/home/user/.missing/token', 'status': 'missing', 'detail': 'source does not exist'}]}
        with patch.object(sync, 'run_local_passthrough') as interactive:
            result = sync.sync_workspace(cfg, plan, json_mode=True)
        interactive.assert_not_called()
        self.assertFalse(result['ok'])
        self.assertEqual(result['synced'], 0)
        self.assertEqual(result['failed'], 1)
        self.assertEqual(result['items'][0]['result'], 'missing')
        self.assertEqual(result['command_items'][0]['result'], 'skipped')

    def test_sync_verbose_defaults_off_in_test_config(self) -> None:
        self.assertFalse(test_config().sync_verbose)
        self.assertEqual(test_config().sync_commands, ())

    def test_sync_without_commands_preserves_existing_behavior(self) -> None:
        result, calls = self.run_successful_path_sync()
        self.assertTrue(result['ok'])
        self.assertEqual(result['synced'], 1)
        self.assertNotIn('command_items', result)
        self.assertNotIn('commands_succeeded', result)
        self.assertFalse(any((cmd[-1] == 'printf post-sync' for cmd in calls)))

    def test_sync_runs_one_successful_command_as_workspace_user(self) -> None:
        command = 'printf post-sync'
        result, calls = self.run_successful_path_sync((command,))
        self.assertTrue(result['ok'])
        self.assertEqual(result['commands_succeeded'], 1)
        command_call = next((cmd for cmd in calls if cmd[-1] == command))
        self.assertEqual(command_call[0], 'ssh')
        self.assertIn('user@192.0.2.200', command_call)
        self.assertNotIn('root@192.0.2.200', command_call)

    def test_sync_runs_multiple_commands_in_config_order(self) -> None:
        commands = ('printf first', 'printf second', 'printf third')
        result, calls = self.run_successful_path_sync(commands)
        executed = [cmd[-1] for cmd in calls if cmd[-1] in commands]
        self.assertEqual(executed, list(commands))
        self.assertEqual([item['result'] for item in result['command_items']], ['succeeded', 'succeeded', 'succeeded'])

    def test_failed_command_stops_subsequent_commands(self) -> None:
        commands = ('printf first', 'exit 9', 'printf never')
        result, calls = self.run_successful_path_sync(commands, failed_command='exit 9')
        executed = [cmd[-1] for cmd in calls if cmd[-1] in commands]
        self.assertEqual(executed, ['printf first', 'exit 9'])
        self.assertEqual([item['result'] for item in result['command_items']], ['succeeded', 'failed', 'skipped'])
        self.assertNotIn('OUTPUT_MARKER_MUST_NOT_LEAK', result['command_items'][1]['detail'])

    def test_command_failure_makes_sync_fail(self) -> None:
        result, _ = self.run_successful_path_sync(('exit 9',), failed_command='exit 9')
        self.assertFalse(result['ok'])
        self.assertEqual(result['commands_failed'], 1)
        self.assertEqual(result['commands_succeeded'], 0)
        self.assertEqual(result['commands_skipped'], 0)

    def test_sync_ssh_transport_failure_is_not_reported_as_missing_rsync(self) -> None:
        cfg = replace(test_config(), sync_paths=('~/.config/example/settings.json',), sync_verbose=False)
        plan = {'command': 'sync', 'vmid': 200, 'name': 'test1', 'node': 'example-node-1', 'status': 'running', 'ip': '192.0.2.200', 'user': 'user', 'target_home': '/home/user', 'configured': 1, 'ready': 1, 'preflight_failed': 0, 'transfer': 'rsync over SSH', 'verbose': False, 'delete': False, 'config': '/tmp/test.toml', 'items': [{'path': '~/.config/example/settings.json', 'relative': '.config/example/settings.json', 'type': 'file', 'is_directory': False, 'local_path': '/tmp/settings.json', 'destination': '/home/user/.config/example/settings.json', 'status': 'ready', 'detail': ''}]}
        master_ok = __import__('subprocess').CompletedProcess(['ssh'], 0, '', '')
        check_ok = __import__('subprocess').CompletedProcess(['ssh'], 0, 'Master running', '')
        transport_fail = __import__('subprocess').CompletedProcess(['ssh'], 255, '', 'mux_client_request_session: read from master failed')
        with patch.object(sync, 'run_local_passthrough', return_value=master_ok), patch.object(sync, 'run_local', side_effect=[check_ok, transport_fail, check_ok]):
            result = sync.sync_workspace(cfg, plan, json_mode=True)
        self.assertFalse(result['ok'])
        self.assertEqual(result['items'][0]['result'], 'failed')
        self.assertIn('mux_client_request_session', result['items'][0]['detail'])
        self.assertNotIn('not installed', result['items'][0]['detail'])
