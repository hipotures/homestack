from __future__ import annotations

import base64
import hashlib
import os
from pathlib import Path
import shutil
import stat
import tempfile
import unittest
from unittest.mock import patch

from homestack import setup_guest as guest


def encoded(content: bytes) -> str:
    return base64.b64encode(content).decode("ascii")


def digest(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


class StructuredGuestOperationTests(unittest.TestCase):
    def test_read_missing_and_write_new_file_uses_safe_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            missing = guest.run({
                "operation": "structured-read",
                "home": str(home),
                "relative": ".codex/config.toml",
            })
            self.assertEqual(missing, {"ok": True, "content": None, "sha256": None, "mode": None})

            content = b"approval_policy = 'never'\n"
            result = guest.run({
                "operation": "structured-write",
                "home": str(home),
                "relative": ".codex/config.toml",
                "content": encoded(content),
                "expected_sha256": None,
            })
            self.assertEqual(result, {"ok": True, "changed": True})
            self.assertEqual((home / ".codex/config.toml").read_bytes(), content)
            self.assertEqual(stat.S_IMODE((home / ".codex/config.toml").stat().st_mode), 0o600)

    def test_write_preserves_mode_and_noop_keeps_inode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path = home / ".codex/config.toml"
            path.parent.mkdir()
            original = b"approval_policy = 'user'\n"
            path.write_bytes(original)
            path.chmod(0o640)
            before_inode = path.stat().st_ino
            state = guest.run({
                "operation": "structured-read",
                "home": str(home),
                "relative": ".codex/config.toml",
            })
            self.assertEqual(state["mode"], 0o640)

            no_op = guest.run({
                "operation": "structured-write",
                "home": str(home),
                "relative": ".codex/config.toml",
                "content": state["content"],
                "expected_sha256": state["sha256"],
            })
            self.assertEqual(no_op, {"ok": True, "changed": False})
            self.assertEqual(path.stat().st_ino, before_inode)

            updated = b"approval_policy = 'never'\n"
            changed = guest.run({
                "operation": "structured-write",
                "home": str(home),
                "relative": ".codex/config.toml",
                "content": encoded(updated),
                "expected_sha256": state["sha256"],
            })
            self.assertEqual(changed, {"ok": True, "changed": True})
            self.assertEqual(path.read_bytes(), updated)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o640)

    def test_write_cas_conflict_leaves_file_unchanged(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path = home / "config.json"
            path.write_bytes(b'{"one": 1}\n')
            before = path.read_bytes()
            with self.assertRaisesRegex(guest.GuestError, "CAS conflict"):
                guest.run({
                    "operation": "structured-write",
                    "home": str(home),
                    "relative": "config.json",
                    "content": encoded(b'{"one": 2}\n'),
                    "expected_sha256": digest(b"different"),
                })
            self.assertEqual(path.read_bytes(), before)

    def test_read_stays_on_pinned_parent_when_ancestor_becomes_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            outside = home.parent / (home.name + "-outside")
            outside.mkdir()
            self.addCleanup(shutil.rmtree, outside, ignore_errors=True)
            managed = home / "managed"
            managed.mkdir()
            (managed / "config.toml").write_bytes(b"inside")
            (outside / "config.toml").write_bytes(b"outside")

            real_read = guest._read_pinned_file
            swapped = False

            def swap_after_validation(target, label="Structured configuration"):
                nonlocal swapped
                if not swapped:
                    managed.rename(home / "managed-original")
                    managed.symlink_to(outside, target_is_directory=True)
                    swapped = True
                return real_read(target, label)

            with patch.object(guest, "_read_pinned_file", side_effect=swap_after_validation):
                result = guest.run({
                    "operation": "structured-read",
                    "home": str(home),
                    "relative": "managed/config.toml",
                })
            self.assertEqual(result["content"], encoded(b"inside"))
            self.assertEqual((outside / "config.toml").read_bytes(), b"outside")

    def test_write_stays_on_pinned_parent_when_ancestor_becomes_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            outside = home.parent / (home.name + "-outside")
            outside.mkdir()
            self.addCleanup(shutil.rmtree, outside, ignore_errors=True)
            managed = home / "managed"
            managed.mkdir()
            path = managed / "config.toml"
            before = b"inside"
            path.write_bytes(before)
            outside_path = outside / "config.toml"
            outside_path.write_bytes(b"outside")

            real_read = guest._read_pinned_file
            swapped = False

            def swap_after_validation(target, label="Structured configuration"):
                nonlocal swapped
                if not swapped:
                    managed.rename(home / "managed-original")
                    managed.symlink_to(outside, target_is_directory=True)
                    swapped = True
                return real_read(target, label)

            updated = b"updated"
            with patch.object(guest, "_read_pinned_file", side_effect=swap_after_validation):
                result = guest.run({
                    "operation": "structured-write",
                    "home": str(home),
                    "relative": "managed/config.toml",
                    "content": encoded(updated),
                    "expected_sha256": digest(before),
                })
            self.assertEqual(result, {"ok": True, "changed": True})
            self.assertEqual((home / "managed-original/config.toml").read_bytes(), updated)
            self.assertEqual(outside_path.read_bytes(), b"outside")

    def test_restore_stays_on_pinned_parent_when_ancestor_becomes_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            outside = home.parent / (home.name + "-outside")
            outside.mkdir()
            self.addCleanup(shutil.rmtree, outside, ignore_errors=True)
            managed = home / "managed"
            managed.mkdir()
            path = managed / "config.toml"
            original = b"original"
            candidate = b"candidate"
            path.write_bytes(original)
            snapshot = guest.run({
                "operation": "snapshot",
                "home": str(home),
                "paths": ["managed/config.toml"],
                "items": ["codex"],
                "vmid": 200,
                "name": "workspace",
            })
            path.write_bytes(candidate)
            outside_path = outside / "config.toml"
            outside_path.write_bytes(b"outside")

            real_read = guest._read_pinned_file
            swapped = False

            def swap_after_validation(target, label="Structured configuration"):
                nonlocal swapped
                if not swapped:
                    managed.rename(home / "managed-original")
                    managed.symlink_to(outside, target_is_directory=True)
                    swapped = True
                return real_read(target, label)

            with patch.object(guest, "_read_pinned_file", side_effect=swap_after_validation):
                result = guest.run({
                    "operation": "structured-restore",
                    "home": str(home),
                    "relative": "managed/config.toml",
                    "snapshot": snapshot["id"],
                    "expected_sha256": digest(candidate),
                    "existed": True,
                })
            self.assertEqual(result, {"ok": True, "changed": True})
            self.assertEqual((home / "managed-original/config.toml").read_bytes(), original)
            self.assertEqual(outside_path.read_bytes(), b"outside")

    def test_fifo_is_rejected_without_blocking(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            fifo = home / "config.toml"
            os.mkfifo(fifo)
            with self.assertRaisesRegex(guest.GuestError, "regular file"):
                guest.run({
                    "operation": "structured-read",
                    "home": str(home),
                    "relative": "config.toml",
                })

    def test_symlink_is_rejected_for_read_write_and_restore(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            target = home / "target"
            target.write_bytes(b"private")
            link = home / "config.toml"
            link.symlink_to(target)
            for operation in ("structured-read", "structured-write"):
                data = {
                    "operation": operation,
                    "home": str(home),
                    "relative": "config.toml",
                }
                if operation == "structured-write":
                    data.update({"content": encoded(b"new"), "expected_sha256": digest(b"private")})
                with self.subTest(operation=operation), self.assertRaisesRegex(guest.GuestError, "[Ss]ymlink"):
                    guest.run(data)

            with self.assertRaisesRegex(guest.GuestError, "[Ss]ymlink"):
                guest.run({
                    "operation": "structured-restore",
                    "home": str(home),
                    "relative": "config.toml",
                    "snapshot": None,
                    "expected_sha256": digest(b"private"),
                    "existed": False,
                })
            self.assertTrue(link.is_symlink())
            self.assertEqual(target.read_bytes(), b"private")

    def test_target_ownership_is_checked_before_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path = home / "config.json"
            path.write_bytes(b"{}")
            actual_uid = os.getuid()
            with patch.object(guest.os, "getuid", side_effect=[actual_uid, actual_uid + 1]):
                with self.assertRaisesRegex(guest.GuestError, "[Oo]wnership"):
                    guest.run({
                        "operation": "structured-read",
                        "home": str(home),
                        "relative": "config.json",
                    })

    def test_restore_existing_file_uses_snapshot_and_preserves_original_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path = home / ".codex/config.toml"
            path.parent.mkdir()
            original = b"approval_policy = 'user'\n"
            path.write_bytes(original)
            path.chmod(0o640)
            snapshot = guest.run({
                "operation": "snapshot",
                "home": str(home),
                "paths": [".codex/config.toml"],
                "items": ["codex"],
                "vmid": 200,
                "name": "workspace",
            })
            self.assertTrue(snapshot["created"])

            candidate = b"approval_policy = 'never'\n"
            guest.run({
                "operation": "structured-write",
                "home": str(home),
                "relative": ".codex/config.toml",
                "content": encoded(candidate),
                "expected_sha256": digest(original),
            })
            restored = guest.run({
                "operation": "structured-restore",
                "home": str(home),
                "relative": ".codex/config.toml",
                "snapshot": snapshot["id"],
                "expected_sha256": digest(candidate),
                "existed": True,
            })
            self.assertEqual(restored, {"ok": True, "changed": True})
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o640)

    def test_restore_new_file_removes_only_matching_candidate_without_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            candidate = b"{\"new\": true}\n"
            guest.run({
                "operation": "structured-write",
                "home": str(home),
                "relative": ".config/new.json",
                "content": encoded(candidate),
                "expected_sha256": None,
            })
            restored = guest.run({
                "operation": "structured-restore",
                "home": str(home),
                "relative": ".config/new.json",
                "snapshot": None,
                "expected_sha256": digest(candidate),
                "existed": False,
            })
            self.assertEqual(restored, {"ok": True, "changed": True})
            self.assertFalse((home / ".config/new.json").exists())

    def test_restore_rejects_candidate_cas_mismatch_and_snapshot_membership(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path = home / "config.toml"
            path.write_bytes(b"before")
            snapshot = guest.run({
                "operation": "snapshot",
                "home": str(home),
                "paths": ["config.toml"],
                "items": ["codex"],
                "vmid": 200,
                "name": "workspace",
            })
            path.write_bytes(b"after")
            with self.assertRaisesRegex(guest.GuestError, "CAS conflict"):
                guest.run({
                    "operation": "structured-restore",
                    "home": str(home),
                    "relative": "config.toml",
                    "snapshot": snapshot["id"],
                    "expected_sha256": digest(b"different"),
                    "existed": True,
                })
            with self.assertRaisesRegex(guest.GuestError, "not part"):
                guest.run({
                    "operation": "structured-restore",
                    "home": str(home),
                    "relative": "other.toml",
                    "snapshot": snapshot["id"],
                    "expected_sha256": digest(b"after"),
                    "existed": True,
                })


if __name__ == "__main__":
    unittest.main()
