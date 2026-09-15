from __future__ import annotations

import argparse
import base64
from contextlib import nullcontext
import io
import json
import os
import stat
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from homestack import backup, setup, setup_catalog, setup_config, setup_guest
from homestack.models import AppError
from homestack.setup_cli import run_setup
from homestack.setup_tui import SetupApp
from support import test_config


TARGET = {
    "vmid": 200,
    "name": "workspace",
    "ip": "192.0.2.200",
    "home": "/home/user",
    "user": "user",
}


def bk_entry():
    return next(entry for entry in setup_config.defaults() if entry.id == "bk")


def completed(code: int = 0, output: str = ""):
    return subprocess.CompletedProcess([], code, output, "")


class FakeWorkspace:
    def __init__(self, *, timer_enabled: bool = True, timer_active: bool = True):
        self.timer_enabled = timer_enabled
        self.timer_active = timer_active
        self.commands: list[str] = []

    def run(self, command, **_kwargs):
        self.commands.append(command)
        if "systemctl --user enable --now backup.timer" in command:
            self.timer_enabled = True
            self.timer_active = True
        if "systemctl --user is-enabled backup.timer" in command:
            return completed(0 if self.timer_enabled else 1)
        if "systemctl --user is-active backup.timer" in command:
            return completed(0 if self.timer_active else 3)
        return completed()


class MemoryGuest:
    def __init__(self, files=None):
        self.files = dict(files or {})
        self.calls = []

    def __call__(self, _ws, _cfg, operation, **values):
        self.calls.append((operation, values))
        if operation == "paths":
            return {"ok": True}
        if operation == "managed-files-inspect":
            items = []
            for path in values["paths"]:
                current = self.files.get(path)
                if current is None:
                    items.append({"path": path, "exists": False})
                    continue
                content, mode = current
                import hashlib

                items.append(
                    {
                        "path": path,
                        "exists": True,
                        "type": "file",
                        "sha256": hashlib.sha256(content).hexdigest(),
                        "size": len(content),
                        "mode": mode,
                    }
                )
            return {"ok": True, "items": items}
        if operation == "managed-files-install":
            for item in values["files"]:
                self.files[item["path"]] = (
                    base64.b64decode(item["content"], validate=True),
                    item["mode"],
                )
            return {"ok": True, "changed": [item["path"] for item in values["files"]]}
        raise AssertionError(operation)


def desired_memory_files():
    contents = backup.load_assets()
    modes = {asset.relative_path: asset.mode for asset in backup.MANAGED_ASSETS}
    return {path: (content, modes[path]) for path, content in contents.items()}


class BackupDefinitionTests(unittest.TestCase):
    def test_group_item_selectors_catalog_and_plan_are_first_class(self):
        cfg = test_config()
        self.assertEqual(cfg.setup.groups[-1].id, "backup")
        self.assertEqual(cfg.setup.groups[-1].label, "Backup")
        entry = bk_entry()
        self.assertEqual(entry.group, "backup")
        self.assertEqual(entry.handler, "backup")
        self.assertIsInstance(entry.params, setup_config.BackupParams)

        for selector in ("backup=bk", "b=bk"):
            selected, catalog_id = setup_catalog.select_entries(cfg, [selector])
            self.assertEqual(selected, (entry,))
            self.assertIsNone(catalog_id)
        self.assertEqual(
            setup_catalog.select_entries(cfg, ["e=bash"])[0][0].id, "bash"
        )
        self.assertEqual(
            setup_catalog.select_entries(cfg, ["a=codex"])[0][0].id, "codex"
        )
        with tempfile.TemporaryDirectory() as directory, patch.dict(
            os.environ, {"XDG_STATE_HOME": directory}
        ):
            snapshot = setup_catalog.save_snapshot(
                cfg, setup_catalog.load_catalog(cfg)
            )
            numeric, used = setup_catalog.select_entries(
                cfg, ["backup=1"], catalog_id=snapshot
            )
        self.assertEqual(numeric, (entry,))
        self.assertEqual(used, snapshot)

        catalog = setup_catalog.load_catalog(cfg)
        row = next(row for row in catalog.rows(cfg) if row["id"] == "bk")
        self.assertEqual(row["group"], "backup")
        self.assertIn("~/.local/bin/bk", row["path_or_repository"])
        plan = setup.build_plan(cfg, TARGET, (entry,))
        action = plan.public()["actions"][0]
        self.assertEqual(action["handler"], "backup")
        self.assertEqual(
            action["destinations"],
            ["~/" + path for path in backup.managed_paths()],
        )

    def test_dry_run_contains_bk_without_execution(self):
        args = argparse.Namespace(
            target="workspace",
            selectors=["backup=bk"],
            catalog=None,
            dry_run=True,
            non_interactive=False,
        )
        output = io.StringIO()
        with patch("homestack.setup_cli.open_transport", return_value=nullcontext(Mock())), \
             patch("homestack.setup_cli.resolve_target", return_value=TARGET), \
             patch("homestack.setup_cli.execute_plan") as execute, \
             patch("sys.stdout", output):
            code = run_setup(args, test_config(), json_mode=True, assume_yes=False)
        self.assertEqual(code, 0)
        execute.assert_not_called()
        result = json.loads(output.getvalue())
        self.assertTrue(result["dry_run"])
        self.assertEqual(result["plan"]["actions"][0]["id"], "bk")


class BackupResourceAndGuestTests(unittest.TestCase):
    def test_resources_load_from_the_homestack_package(self):
        assets = backup.load_assets()
        self.assertEqual(set(assets), set(backup.managed_paths()))
        self.assertTrue(assets[".local/bin/bk"].startswith(b"#!/usr/bin/env python3"))
        self.assertIn(b"ExecStart=%h/.local/bin/bk run", assets[".config/systemd/user/backup.service"])
        self.assertIn(b"Persistent=true", assets[".config/systemd/user/backup.timer"])

    def test_guest_install_is_atomic_idempotent_and_preserves_runtime_data(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            runtime = {
                "backup/backup.yaml": b"user configuration\n",
                "backup/backup.tgz": b"archive\n",
                "backup/backup.log": b"log\n",
                "backup/status.json": b"{}\n",
                "backup/.bk.lock": b"",
                "backup/archive/old.tgz": b"history\n",
                "backup/.work/keep": b"transient\n",
            }
            for relative, content in runtime.items():
                path = home / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)

            contents = backup.load_assets()
            files = []
            for asset in backup.MANAGED_ASSETS:
                content = contents[asset.relative_path]
                import hashlib

                files.append(
                    {
                        "path": asset.relative_path,
                        "content": base64.b64encode(content).decode("ascii"),
                        "sha256": hashlib.sha256(content).hexdigest(),
                        "expected_sha256": None,
                        "mode": asset.mode,
                    }
                )
            result = setup_guest.run(
                {
                    "operation": "managed-files-install",
                    "home": str(home),
                    "directories": list(backup.MANAGED_DIRECTORIES),
                    "files": files,
                }
            )
            self.assertEqual(set(result["changed"]), set(backup.managed_paths()))
            inspected = setup_guest.run(
                {
                    "operation": "managed-files-inspect",
                    "home": str(home),
                    "paths": list(backup.managed_paths()),
                }
            )
            by_path = {item["path"]: item for item in inspected["items"]}
            for asset in backup.MANAGED_ASSETS:
                path = home / asset.relative_path
                self.assertEqual(path.read_bytes(), contents[asset.relative_path])
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), asset.mode)
                self.assertEqual(by_path[asset.relative_path]["mode"], asset.mode)
            self.assertTrue((home / "backup").is_dir())
            for relative, content in runtime.items():
                self.assertEqual((home / relative).read_bytes(), content)

            inodes = {path: (home / path).stat().st_ino for path in backup.managed_paths()}
            repeat_files = []
            for item in files:
                repeat_files.append(
                    {
                        **item,
                        "expected_sha256": by_path[item["path"]]["sha256"],
                    }
                )
            repeated = setup_guest.run(
                {
                    "operation": "managed-files-install",
                    "home": str(home),
                    "directories": list(backup.MANAGED_DIRECTORIES),
                    "files": repeat_files,
                }
            )
            self.assertEqual(repeated["changed"], [])
            self.assertEqual(
                inodes,
                {path: (home / path).stat().st_ino for path in backup.managed_paths()},
            )

    def test_guest_install_creates_only_required_directories_without_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            setup_guest.run(
                {
                    "operation": "managed-files-install",
                    "home": str(home),
                    "directories": list(backup.MANAGED_DIRECTORIES),
                    "files": [],
                }
            )
            for relative in backup.MANAGED_DIRECTORIES:
                self.assertTrue((home / relative).is_dir())
            self.assertFalse((home / "backup/backup.yaml").exists())

    def test_guest_install_rejects_preflight_race(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            target = home / ".local/bin/bk"
            target.parent.mkdir(parents=True)
            target.write_bytes(b"changed after preflight")
            content = backup.load_assets()[".local/bin/bk"]
            import hashlib

            with self.assertRaisesRegex(setup_guest.GuestError, "changed since preflight"):
                setup_guest.run(
                    {
                        "operation": "managed-files-install",
                        "home": str(home),
                        "directories": list(backup.MANAGED_DIRECTORIES),
                        "files": [
                            {
                                "path": ".local/bin/bk",
                                "content": base64.b64encode(content).decode("ascii"),
                                "sha256": hashlib.sha256(content).hexdigest(),
                                "expected_sha256": "0" * 64,
                                "mode": 0o755,
                            }
                        ],
                    }
                )
            self.assertEqual(target.read_bytes(), b"changed after preflight")


class BackupHandlerTests(unittest.TestCase):
    def test_inspection_distinguishes_missing_current_drift_and_disabled_timer(self):
        cfg = test_config()
        workspace = FakeWorkspace(timer_enabled=False, timer_active=False)
        memory = MemoryGuest()
        with patch.object(setup, "guest", side_effect=memory):
            missing = backup.inspect(workspace, cfg)
        self.assertEqual(missing["state"], "not installed")
        self.assertFalse(missing["ready"])

        workspace = FakeWorkspace()
        memory = MemoryGuest(desired_memory_files())
        with patch.object(setup, "guest", side_effect=memory):
            current = backup.inspect(workspace, cfg)
        self.assertEqual(current["state"], "configured")
        self.assertTrue(current["ready"])

        files = desired_memory_files()
        files[".local/bin/bk"] = (b"old BK", 0o755)
        files[".config/systemd/user/backup.timer"] = (
            files[".config/systemd/user/backup.timer"][0],
            0o600,
        )
        memory = MemoryGuest(files)
        with patch.object(setup, "guest", side_effect=memory):
            drifted = backup.inspect(workspace, cfg)
        self.assertEqual(drifted["state"], "needs update")
        self.assertEqual(
            set(drifted["snapshot_paths"]),
            {".local/bin/bk", ".config/systemd/user/backup.timer"},
        )

        workspace.timer_enabled = False
        memory = MemoryGuest(desired_memory_files())
        with patch.object(setup, "guest", side_effect=memory):
            disabled = backup.inspect(workspace, cfg)
        self.assertEqual(disabled["state"], "needs update")
        self.assertFalse(disabled["will_overwrite"])

        for relative in backup.managed_paths():
            with self.subTest(changed_asset=relative):
                files = desired_memory_files()
                files[relative] = (b"outdated", files[relative][1])
                with patch.object(setup, "guest", side_effect=MemoryGuest(files)):
                    drifted = backup.inspect(FakeWorkspace(), cfg)
                self.assertEqual(drifted["state"], "needs update")
                self.assertIn(relative, drifted["changed_paths"])

    def test_preflight_checks_runtime_and_user_systemd(self):
        cfg = test_config()
        workspace = FakeWorkspace()
        checked = []
        with patch.object(setup, "require_tool", side_effect=lambda _ws, _cfg, tool: checked.append(tool)), \
             patch.object(setup, "guest", side_effect=MemoryGuest()):
            backup.preflight(workspace, cfg)
        self.assertEqual(checked, ["python3", "file", "git", "systemctl"])
        self.assertTrue(any("import curses, rich, sqlite3" in command for command in workspace.commands))
        self.assertTrue(any("systemctl --user show-environment" in command for command in workspace.commands))

        class MissingRich(FakeWorkspace):
            def run(self, command, **kwargs):
                if "import curses, rich, sqlite3" in command:
                    return completed(1)
                return super().run(command, **kwargs)

        with patch.object(setup, "require_tool"):
            with self.assertRaisesRegex(AppError, "curses, rich and sqlite3"):
                backup.preflight(MissingRich(), cfg)

    def test_apply_installs_enables_verifies_and_is_idempotent(self):
        cfg = test_config()
        workspace = FakeWorkspace(timer_enabled=False, timer_active=False)
        memory = MemoryGuest()
        with patch.object(setup, "guest", side_effect=memory):
            state = backup.inspect(workspace, cfg)
            status, detail = backup.apply(workspace, cfg, state)
            current = backup.inspect(workspace, cfg)
            install_calls = len(
                [operation for operation, _values in memory.calls if operation == "managed-files-install"]
            )
            command_count = len(workspace.commands)
            repeat_status, _ = backup.apply(workspace, cfg, current)
        self.assertEqual(status, "succeeded")
        self.assertIn("installed", detail)
        self.assertEqual(repeat_status, "already-ready")
        self.assertEqual(
            install_calls,
            len([operation for operation, _values in memory.calls if operation == "managed-files-install"]),
        )
        self.assertEqual(command_count, len(workspace.commands))
        self.assertTrue(any("systemctl --user daemon-reload" in command for command in workspace.commands))
        self.assertTrue(any("systemctl --user enable --now backup.timer" in command for command in workspace.commands))

    def test_update_sends_only_the_changed_managed_asset(self):
        cfg = test_config()
        files = desired_memory_files()
        service = ".config/systemd/user/backup.service"
        files[service] = (b"old service", 0o644)
        memory = MemoryGuest(files)
        workspace = FakeWorkspace()
        with patch.object(setup, "guest", side_effect=memory):
            state = backup.inspect(workspace, cfg)
            backup.apply(workspace, cfg, state)
        install = next(
            values
            for operation, values in memory.calls
            if operation == "managed-files-install"
        )
        self.assertEqual([item["path"] for item in install["files"]], [service])
        self.assertEqual(memory.files, desired_memory_files())

    def test_generic_workspace_inspection_exposes_backup_state(self):
        live = {
            "state": "configured",
            "ready": True,
            "exists": True,
            "will_overwrite": False,
            "files": [],
            "managed_paths": list(backup.managed_paths()),
            "changed_paths": [],
            "snapshot_paths": [],
            "timer_enabled": True,
            "timer_active": True,
        }

        def guest_call(_ws, _cfg, operation, **_values):
            if operation == "state-read":
                return {"registry": None, "state_path": "~/.local/state/homestack/setup.json"}
            return {"ok": True}

        with patch.object(setup, "require_tool"), \
             patch.object(setup, "guest", side_effect=guest_call), \
             patch.object(setup.backup, "inspect", return_value=live):
            state = setup.inspect_workspace_state(
                Mock(), test_config(), TARGET, (bk_entry(),)
            )
        self.assertEqual(state["items"]["bk"]["state"], "configured")
        self.assertTrue(state["items"]["bk"]["ready"])

    def test_execute_uses_one_workspace_snapshots_only_changed_assets_and_records_paths(self):
        cfg = test_config()
        entry = bk_entry()
        plan = setup.build_plan(cfg, TARGET, (entry,))
        workspace = Mock()
        factory = Mock(side_effect=AssertionError("must not open another workspace session"))
        calls = []

        def guest_call(_ws, _cfg, operation, **values):
            calls.append((operation, values))
            if operation == "snapshot":
                return {"created": True, "id": "snapshot", "path": "~/.local/state/homestack/snapshot"}
            return {"ok": True}

        state = {
            "ready": False,
            "state": "needs update",
            "snapshot_paths": [".local/bin/bk"],
        }
        with patch.object(setup, "require_tool"), \
             patch.object(setup, "preflight_entry", return_value=state), \
             patch.object(setup, "apply_entry", return_value=("succeeded", "BK updated")), \
             patch.object(setup, "guest", side_effect=guest_call):
            result = setup.execute_plan(
                cfg, plan, workspace=workspace, connection_factory=factory
            )
        self.assertTrue(result["ok"])
        factory.assert_not_called()
        snapshot = next(values for operation, values in calls if operation == "snapshot")
        self.assertEqual(snapshot["paths"], [".local/bin/bk"])
        record = next(values for operation, values in calls if operation == "state-record")
        self.assertEqual(record["handler"], "backup")
        self.assertEqual(record["paths"], list(backup.managed_paths()))
        self.assertNotIn("backup/backup.yaml", json.dumps(calls))

    def test_invalid_backup_directory_blocks_the_whole_plan_before_mutation(self):
        cfg = test_config()
        plan = setup.build_plan(cfg, TARGET, (bk_entry(),))
        workspace = FakeWorkspace(timer_enabled=False, timer_active=False)
        operations = []

        def guest_call(_ws, _cfg, operation, **_values):
            operations.append(operation)
            if operation == "paths":
                raise AppError("File type conflict at ~/backup")
            return {"ok": True}

        with patch.object(setup, "require_tool"), \
             patch.object(setup, "guest", side_effect=guest_call), \
             patch.object(setup, "apply_entry") as apply:
            result = setup.execute_plan(cfg, plan, workspace=workspace)
        self.assertFalse(result["ok"])
        self.assertEqual(result["results"][0]["status"], "blocked")
        self.assertIn("File type conflict", result["results"][0]["detail"])
        self.assertNotIn("snapshot", operations)
        self.assertNotIn("managed-files-install", operations)
        self.assertNotIn("state-record", operations)
        apply.assert_not_called()


class BackupTUITests(unittest.IsolatedAsyncioTestCase):
    async def test_backup_group_and_item_are_rendered_by_the_generic_tree(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(100, 35)):
            self.assertIn("backup", app.nodes)
            self.assertIn("bk", app.nodes)
            self.assertIn("Backup", str(app.nodes["backup"].label))
            self.assertIn("BK", str(app.nodes["bk"].label))
