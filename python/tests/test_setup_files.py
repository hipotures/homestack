from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import config, models, setup_files
from homestack import setup_config as definitions

from support import test_config

class SetupFilesTests(unittest.TestCase):

    def test_home_path_spec_uses_trailing_slash_for_directory(self) -> None:
        self.assertEqual(config.validate_home_path_spec('~/.config/example/settings.json'), ('.config/example/settings.json', False))
        self.assertEqual(config.validate_home_path_spec('~/.config/example-tool/'), ('.config/example-tool', True))
        for invalid in ('/tmp/outside', '~/', '~/.config/../parent', '~/.config//item'):
            with self.assertRaises(models.AppError):
                config.validate_home_path_spec(invalid)

    def test_file_plan_item_checks_file_and_directory_type(self) -> None:
        cfg = replace(test_config(), setup=definitions.parse_setup({'items': [
            {'id': 'example-file', 'group': 'files', 'handler': 'file', 'label': 'Example file',
             'description': 'Example file', 'path': '~/.config/example/settings.json'},
            {'id': 'example-dir', 'group': 'files', 'handler': 'file', 'label': 'Example directory',
             'description': 'Example directory', 'path': '~/.config/example-tool/'},
            {'id': 'missing-file', 'group': 'files', 'handler': 'file', 'label': 'Missing file',
             'description': 'Missing file', 'path': '~/.missing/item'},
        ]}))
        file_path = next(e.params.path for e in cfg.setup.items if e.id == 'example-file')
        directory_path = next(e.params.path for e in cfg.setup.items if e.id == 'example-dir')
        missing_path = next(e.params.path for e in cfg.setup.items if e.id == 'missing-file')
        mismatch_path = definitions.FileParams('~/.config/example-tool').path
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / '.config' / 'example').mkdir(parents=True)
            (home / '.config' / 'example' / 'settings.json').write_text('example', encoding='utf-8')
            (home / '.config' / 'example-tool').mkdir(parents=True)
            with patch.object(Path, 'home', return_value=home):
                file_item = setup_files.file_plan_item(cfg, file_path)
                dir_item = setup_files.file_plan_item(cfg, directory_path)
                mismatch = setup_files.file_plan_item(cfg, mismatch_path)
                missing = setup_files.file_plan_item(cfg, missing_path)
            self.assertEqual(file_item['status'], 'ready')
            self.assertEqual(file_item['type'], 'file')
            self.assertEqual(file_item['destination'], '/home/user/.config/example/settings.json')
            self.assertEqual(dir_item['status'], 'ready')
            self.assertEqual(dir_item['type'], 'directory')
            self.assertEqual(dir_item['destination'], '/home/user/.config/example-tool/')
            self.assertEqual(mismatch['status'], 'type mismatch')
            self.assertEqual(missing['status'], 'missing')

    def test_missing_selected_source_blocks_setup_before_ssh(self):
        from homestack import setup
        cfg = replace(test_config(), setup=definitions.parse_setup({'items': [
            {'id': 'missing-file', 'group': 'files', 'handler': 'file', 'label': 'Missing file',
             'description': 'Missing file', 'path': '~/missing-fixture'},
        ]}))
        selected = tuple(e for e in cfg.setup.items if e.id == 'missing-file')
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            with self.assertRaisesRegex(models.AppError, 'missing'):
                setup.build_plan(cfg, {'name': 'test1', 'vmid': 200}, selected)
