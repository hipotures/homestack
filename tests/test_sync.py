from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import cli, config, models, sync

from support import test_config

class SyncTests(unittest.TestCase):

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

    def test_sync_remains_files_only_through_shared_cli(self):
        from homestack import setup_cli
        cfg = replace(test_config(), sync_paths=('~/fixture',), sync_commands=('printf never',))
        args = cli.build_parser().parse_args(['sync', 'test1', '--yes', '--json'])
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / 'fixture').write_text('fixture')
            with patch.object(Path, 'home', return_value=home), patch.object(setup_cli, 'open_transport'), patch.object(setup_cli, 'resolve_target', return_value={'name': 'test1', 'vmid': 200}), patch.object(setup_cli, 'execute_plan', return_value={'ok': True, 'results': []}) as execute:
                self.assertEqual(setup_cli.run_setup(args, cfg, json_mode=True, assume_yes=True), 0)
            self.assertEqual([e.handler for e in execute.call_args.args[1].entries], ['file'])

    def test_missing_selected_source_blocks_sync_before_ssh(self):
        from homestack import setup
        from homestack.setup_config import effective_entries
        cfg = replace(test_config(), sync_paths=('~/missing-fixture',))
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            with self.assertRaisesRegex(models.AppError, 'missing'):
                setup.build_plan(cfg, {'name': 'test1', 'vmid': 200}, tuple(e for e in effective_entries(cfg) if e.handler == 'file'))
