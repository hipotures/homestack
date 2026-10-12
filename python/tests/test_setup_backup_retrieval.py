from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from homestack import backup, setup, setup_guest
from homestack.models import AppError
from support import test_config
from test_setup_backup import FakeWorkspace


class BackupRetrievalIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.desktop = Path(temporary.name) / "desktop"
        self.guest_home = Path(temporary.name) / "guest"
        self.desktop.mkdir()
        self.guest_home.mkdir()
        home = patch.object(Path, "home", return_value=self.desktop)
        home.start()
        self.addCleanup(home.stop)
        self.calls = []
        remote = patch.object(setup, "guest", side_effect=self.guest)
        remote.start()
        self.addCleanup(remote.stop)
        self.workspace = FakeWorkspace(timer_enabled=False, timer_active=False)
        self.cfg = test_config()

    def guest(self, workspace, cfg, operation, **values):
        self.assertIs(workspace, self.workspace)
        self.calls.append((operation, values))
        return setup_guest.run({"operation": operation, "home": str(self.guest_home),
                                **values})

    def apply(self):
        state = backup.preflight(self.workspace, self.cfg, 200)
        backup.apply(self.workspace, self.cfg, 200, state)
        return state

    def key_state(self):
        directory = self.desktop / ".ssh/homestack/backup"
        return {path.name: (path.read_bytes(), path.stat().st_mtime_ns)
                for path in directory.iterdir() if path.is_file()}

    def test_fresh_install_and_repeated_setup_preserve_keys_and_need_no_backup(self):
        self.apply()
        keys = self.key_state()
        self.assertEqual(len(keys), 4)
        state = backup.inspect(self.workspace, self.cfg, 200)
        self.assertEqual(state["state"], "configured")
        self.assertFalse((self.guest_home / "backup/backup.yaml").exists())
        self.assertFalse((self.guest_home / "backup/backup.tgz").exists())
        self.assertFalse((self.guest_home / "backup/status.json").exists())
        authorized = self.guest_home / ".ssh/authorized_keys"
        original = (authorized.read_bytes(), authorized.stat().st_mtime_ns)
        self.apply()
        self.assertEqual(self.key_state(), keys)
        self.assertEqual((authorized.read_bytes(), authorized.stat().st_mtime_ns), original)
        for operation, payload in self.calls:
            if operation == "backup-authorized-keys":
                serialized = json.dumps(payload)
                self.assertNotIn("PRIVATE KEY", serialized)
                for content, _ in keys.values():
                    if b"PRIVATE KEY" in content:
                        self.assertNotIn(content.decode(), serialized)

    def test_missing_guest_entry_repairs_without_rotating_keys(self):
        self.apply()
        keys = self.key_state()
        authorized = self.guest_home / ".ssh/authorized_keys"
        canonical = authorized.read_bytes()
        authorized.write_bytes(b"# Keep this comment\n" + canonical.splitlines(keepends=True)[0])
        state = backup.inspect(self.workspace, self.cfg, 200)
        self.assertEqual(state["state"], "needs update")
        self.assertIn(".ssh/authorized_keys", state["snapshot_paths"])
        backup.apply(self.workspace, self.cfg, 200, state)
        self.assertEqual(self.key_state(), keys)
        self.assertEqual(authorized.read_bytes(), b"# Keep this comment\n" + canonical)

    def test_asset_update_preserves_credentials_and_runtime_data(self):
        self.apply()
        keys = self.key_state()
        authorized = self.guest_home / ".ssh/authorized_keys"
        original = (authorized.read_bytes(), authorized.stat().st_mtime_ns)
        runtime = {"backup.yaml": b"user configuration", "backup.tgz": b"archive",
                   "status.json": b"status", "archive/keep": b"history",
                   ".work/keep": b"work", "backup.log": b"log", ".bk.lock": b"lock"}
        for relative, content in runtime.items():
            path = self.guest_home / "backup" / relative
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(content)
        (self.guest_home / ".local/bin/bk").write_bytes(b"outdated asset")
        self.apply()
        self.assertEqual(self.key_state(), keys)
        self.assertEqual((authorized.read_bytes(), authorized.stat().st_mtime_ns), original)
        for relative, content in runtime.items():
            self.assertEqual((self.guest_home / "backup" / relative).read_bytes(), content)

    def test_inspection_does_not_generate_desktop_keys(self):
        state = backup.inspect(self.workspace, self.cfg, 200)
        self.assertFalse(state["ready"])
        self.assertFalse((self.desktop / ".ssh").exists())
        self.assertFalse((self.guest_home / ".ssh").exists())

    def test_mismatched_pair_blocks_before_guest_mutation(self):
        backup.reconcile_retrieval_keys(200)
        directory = self.desktop / ".ssh/homestack/backup"
        (directory / "vm200-bk-status.pub").write_bytes(
            (directory / "vm200-bk-archive.pub").read_bytes()
        )
        before = self.key_state()
        with self.assertRaises(AppError):
            self.apply()
        self.assertEqual(self.key_state(), before)
        self.assertFalse((self.guest_home / ".local/bin/bk").exists())
        self.assertFalse((self.guest_home / ".ssh/authorized_keys").exists())
        self.assertFalse(any(operation == "managed-files-install"
                             or (operation == "backup-authorized-keys" and payload.get("apply"))
                             for operation, payload in self.calls))
