from __future__ import annotations

import os
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import config, models
from support import test_config


class ConfigTests(unittest.TestCase):
    def test_default_config_uses_xdg_config_home(self) -> None:
        with patch.dict(os.environ, {'XDG_CONFIG_HOME': '/tmp/example-xdg'}, clear=False):
            self.assertEqual(
                config.default_config_path(),
                Path('/tmp/example-xdg/homestack/config.toml'),
            )

    def test_default_config_falls_back_to_home_config(self) -> None:
        with patch.dict(os.environ, {}, clear=True), patch.object(
            config.Path, 'home', return_value=Path('/tmp/example-home')
        ):
            self.assertEqual(
                config.default_config_path(),
                Path('/tmp/example-home/.config/homestack/config.toml'),
            )

    def test_repository_config_loads_workspace_ssh_section(self) -> None:
        example = Path(__file__).resolve().parents[1] / 'config.example.toml'
        loaded = config.load_config(example)
        self.assertEqual(loaded.transport_type, 'herdr')
        self.assertEqual(loaded.herdr_workspace, 'PVE')
        self.assertEqual(loaded.workspace_ssh.user, 'developer')
        self.assertEqual(
            loaded.workspace_ssh.identity_files,
            ('~/.ssh/example-hardware-key',),
        )
        self.assertTrue(loaded.workspace_ssh.identities_only)
        self.assertEqual(loaded.workspace_ssh.log_level, 'FATAL')
        self.assertEqual(loaded.repo_owner, 'example-owner')
        self.assertEqual(loaded.repo_checkout_root, '~/DEV')

    def test_workspace_ssh_validation_matches_ssh_config_contract(self) -> None:
        example = Path(__file__).resolve().parents[1] / 'config.example.toml'
        original = example.read_text(encoding='utf-8')
        invalid_cases = {
            'missing table': original.replace('[workspace_ssh]', '[removed_workspace_ssh]'),
            'empty user': original.replace('user = "developer"', 'user = ""', 1),
            'mismatched user': original.replace(
                'user = "developer"', 'user = "operator"', 1
            ),
            'empty identities': original.replace(
                'identity_files = [\n    "~/.ssh/example-hardware-key"\n]',
                'identity_files = []',
            ),
            'whitespace identity': original.replace(
                '~/.ssh/example-hardware-key', '~/.ssh/example hardware key'
            ),
            'duplicate identity': original.replace(
                '"~/.ssh/example-hardware-key"',
                '"~/.ssh/example-hardware-key", "~/.ssh/example-hardware-key"',
            ),
            'nonboolean identities only': original.replace(
                'identities_only = true', 'identities_only = "yes"'
            ),
            'unsupported log level': original.replace(
                'log_level = "FATAL"', 'log_level = "TRACE"'
            ),
        }
        with tempfile.TemporaryDirectory() as directory:
            for label, text in invalid_cases.items():
                with self.subTest(label=label):
                    path = Path(directory) / f'{label.replace(" ", "-")}.toml'
                    path.write_text(text, encoding='utf-8')
                    with self.assertRaises(models.AppError):
                        config.load_config(path)

    def test_repo_config_validation(self) -> None:
        example = Path(__file__).resolve().parents[1] / 'config.example.toml'
        original = example.read_text(encoding='utf-8')
        invalid_cases = {
            'invalid owner': original.replace(
                'owner = "example-owner"',
                'owner = "https://github.com/example-owner"',
            ),
            'checkout outside home': original.replace(
                'checkout_root = "~/DEV"',
                'checkout_root = "/tmp/DEV"',
            ),
            'checkout traversal': original.replace(
                'checkout_root = "~/DEV"',
                'checkout_root = "~/DEV/../other"',
            ),
        }
        with tempfile.TemporaryDirectory() as directory:
            for label, text in invalid_cases.items():
                with self.subTest(label=label):
                    path = Path(directory) / 'config.toml'
                    path.write_text(text, encoding='utf-8')
                    with self.assertRaises(models.AppError):
                        config.load_config(path)

    def test_unsupported_transport_fails_clearly(self) -> None:
        example = Path(__file__).resolve().parents[1] / 'config.example.toml'
        text = example.read_text().replace('type = "herdr"', 'type = "ssh"')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            path.write_text(text)
            with self.assertRaisesRegex(models.AppError, 'Unsupported transport type'):
                config.load_config(path)

    def test_root_and_home_must_use_different_disk_slots(self) -> None:
        example = Path(__file__).resolve().parents[1] / 'config.example.toml'
        text = example.read_text(encoding='utf-8').replace(
            '[home]\ndisk = "scsi1"', '[home]\ndisk = "scsi0"'
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            path.write_text(text, encoding='utf-8')
            with self.assertRaisesRegex(
                models.AppError, 'root.*home.*different slots'
            ):
                config.load_config(path)

    def test_generated_toml_round_trips_through_normal_loader(self) -> None:
        cfg = replace(
            test_config(),
            sync_paths=('~/.config/app/', '~/notes.txt'),
            sync_commands=('first --flag', 'second'),
            sync_verbose=True,
            repo_owner='example-owner',
            repo_checkout_root='~/DEV',
        )
        loaded = config.validate_config_text(config.config_to_toml(cfg))
        self.assertEqual(loaded.storage_layouts, cfg.storage_layouts)
        self.assertEqual(loaded.workspace_ssh, cfg.workspace_ssh)
        self.assertEqual(loaded.sync_paths, cfg.sync_paths)
        self.assertEqual(loaded.sync_commands, cfg.sync_commands)
        self.assertEqual(loaded.repo_owner, cfg.repo_owner)
        self.assertEqual(loaded.repo_checkout_root, cfg.repo_checkout_root)
        self.assertNotIn('install_draft', config.config_to_toml(cfg))

    def test_publish_new_config_uses_private_mode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'homestack' / 'config.toml'
            cfg = replace(test_config(), path=path)
            self.assertIsNone(config.publish_config(cfg))
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(config.load_config(path).gold_vmid, cfg.gold_vmid)

    def test_publish_existing_config_creates_backup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            old = replace(test_config(), path=path, gold_vmid=101)
            config.publish_config(old)
            new = replace(old, gold_vmid=102)
            backup = config.publish_config(new)
            self.assertIsNotNone(backup)
            assert backup is not None
            self.assertEqual(config.load_config(path).gold_vmid, 102)
            self.assertEqual(config.load_config(backup).gold_vmid, 101)

    def test_atomic_replace_failure_leaves_original_intact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            old = replace(test_config(), path=path, gold_vmid=101)
            config.publish_config(old)
            original = path.read_bytes()
            with patch.object(config.os, 'replace', side_effect=OSError('replace failed')):
                with self.assertRaisesRegex(OSError, 'replace failed'):
                    config.publish_config(replace(old, gold_vmid=102))
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(path.parent.glob('.*.tmp')), [])

    def test_validation_failure_never_replaces_existing_config(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            cfg = replace(test_config(), path=path)
            config.publish_config(cfg)
            original = path.read_bytes()
            invalid = replace(cfg, storage_layouts={})
            with self.assertRaises(models.AppError):
                config.publish_config(invalid)
            self.assertEqual(path.read_bytes(), original)



class InstallDraftTests(unittest.TestCase):
    def test_runtime_loader_rejects_install_draft_with_resume_hint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            config.publish_install_draft(
                path,
                """version = 1
install_draft = true

[install]
completed = ["transport"]
""",
            )
            self.assertTrue(config.is_install_draft(path))
            draft = config.load_install_draft(path)
            self.assertEqual(draft["install"]["completed"], ["transport"])
            with self.assertRaisesRegex(models.AppError, "configuration is incomplete"):
                config.load_config(path)

    def test_final_publish_replaces_draft_without_creating_draft_backup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            config.publish_install_draft(
                path,
                """version = 1
install_draft = true

[install]
completed = ["transport"]
""",
            )
            cfg = replace(test_config(), path=path)
            backup = config.publish_config(cfg)
            self.assertIsNone(backup)
            self.assertFalse(config.is_install_draft(path))
            self.assertEqual(config.load_config(path).gold_vmid, cfg.gold_vmid)
