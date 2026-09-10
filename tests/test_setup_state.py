from __future__ import annotations

from contextlib import nullcontext
from dataclasses import replace
from io import StringIO
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from homestack import cli, setup, setup_catalog, setup_guest, setup_config
from homestack.setup_cli import run_setup
from homestack.setup_tui import SetupApp
from support import test_config

TARGET = {"vmid": 200, "name": "workspace", "ip": "192.0.2.200", "home": "/home/user", "user": "user"}


def entry(identity):
    return next(item for item in setup_config.defaults() if item.id == identity)


class GuestStateTests(unittest.TestCase):
    def test_snapshot_is_central_and_registry_tracks_managed_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / ".bashrc").write_text("export CUSTOM=1\n", encoding="utf-8")
            snapshot = setup_guest.run({
                "operation": "snapshot",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "items": ["bash"],
                "paths": [".bashrc", ".profile"],
            })
            self.assertTrue(snapshot["created"])
            root = home / ".local" / "state" / "homestack"
            saved = root / snapshot["id"]
            self.assertEqual((saved / "home" / ".bashrc").read_text(), "export CUSTOM=1\n")
            manifest = json.loads((saved / "snapshot.json").read_text())
            before = {item["path"]: item for item in manifest["files"]}
            self.assertTrue(before[".bashrc"]["exists"])
            self.assertFalse(before[".profile"]["exists"])
            self.assertIn("sha256", before[".bashrc"])

            result = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": ["~/.local/bin"],
                "apply": True,
            })
            self.assertTrue(result["changed"])
            self.assertFalse(list(home.glob("*.homestack-backup-*")))
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "bash",
                "handler": "environment",
                "paths": result["paths"],
                "snapshot": snapshot["id"],
            })
            registry = json.loads((root / "setup.json").read_text())
            self.assertEqual(registry["workspace"]["home_label"], "HS_HOME_200")
            bash = registry["items"]["bash"]
            self.assertEqual({item["path"] for item in bash["files"]}, set(result["paths"]))
            self.assertTrue(all(item.get("sha256") for item in bash["files"]))
            self.assertTrue(all(item.get("mtime_ns") for item in bash["files"]))
            self.assertEqual(bash["last_snapshot"], snapshot["id"])

    def test_application_registry_stores_dates_but_no_version_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            first = setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "codex",
                "handler": "application",
                "installed": True,
                "paths": [],
                "snapshot": None,
            })["item"]
            second = setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "codex",
                "handler": "application",
                "installed": False,
                "paths": [],
                "snapshot": None,
            })["item"]
            self.assertEqual(first["installed_at"], second["installed_at"])
            self.assertIn("first_managed_at", second)
            self.assertIn("last_applied_at", second)
            self.assertNotIn("version", second)

    def test_registry_follows_same_persistent_home_after_workspace_rename(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "old-name",
                "id": "codex",
                "handler": "application",
                "installed": True,
                "paths": [],
                "snapshot": None,
            })
            state = setup_guest.run({
                "operation": "state-read",
                "home": str(home),
                "vmid": 200,
                "name": "new-name",
            })
            self.assertEqual(state["registry"]["workspace"]["vmid"], 200)
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "new-name",
                "id": "codex",
                "handler": "application",
                "installed": False,
                "paths": [],
                "snapshot": None,
            })
            registry = json.loads((home / ".local/state/homestack/setup.json").read_text())
            self.assertEqual(registry["workspace"]["name"], "new-name")

    def test_snapshot_copies_previous_registry_only_when_overwrite_exists(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            setup_guest.run({
                "operation": "state-record", "home": str(home), "vmid": 200,
                "name": "workspace", "id": "codex", "handler": "application",
                "installed": True, "paths": [], "snapshot": None,
            })
            no_change = setup_guest.run({
                "operation": "snapshot", "home": str(home), "vmid": 200,
                "name": "workspace", "items": ["codex"], "paths": [".missing"],
            })
            self.assertFalse(no_change["created"])
            (home / ".config").mkdir()
            (home / ".config" / "app.conf").write_text("old")
            changed = setup_guest.run({
                "operation": "snapshot", "home": str(home), "vmid": 200,
                "name": "workspace", "items": ["codex"], "paths": [".config/app.conf"],
            })
            snap = home / ".local/state/homestack" / changed["id"]
            self.assertTrue((snap / "setup.before.json").is_file())
            self.assertEqual((snap / "home/.config/app.conf").read_text(), "old")


class HostStateTests(unittest.TestCase):
    def test_inspection_uses_live_check_and_registry_metadata_without_storing_version(self):
        cfg = test_config()
        ws = Mock()
        ws.run.return_value = Mock(returncode=0, stdout="", stderr="")
        state_doc = {
            "ok": True,
            "registry": {"items": {"codex": {"first_managed_at": "2026-09-11T00:00:00Z", "last_applied_at": "2026-09-11T00:01:00Z"}}},
            "state_path": "~/.local/state/homestack/setup.json",
        }
        with patch.object(setup, "require_tool"), patch.object(setup, "guest", side_effect=[{}, state_doc, {"items": []}]):
            result = setup.inspect_workspace_state(ws, cfg, TARGET, (entry("codex"),))
        item = result["items"]["codex"]
        self.assertTrue(item["ready"])
        self.assertEqual(item["state"], "installed")
        self.assertEqual(item["last_applied_at"], "2026-09-11T00:01:00Z")
        self.assertNotIn("version", item)

    def test_execute_plan_can_reuse_caller_owned_workspace(self):
        cfg = test_config()
        plan = setup.build_plan(cfg, TARGET, (entry("codex"),))
        ws = Mock()
        factory = Mock(side_effect=AssertionError("must not create another SSH session"))
        with patch.object(setup, "require_tool"), \
             patch.object(setup, "guest", return_value={"ok": True}), \
             patch.object(setup, "preflight_entry", return_value={"installed": False}), \
             patch.object(setup, "apply_entry", return_value=("succeeded", "ok")), \
             patch.object(setup, "record_entry_state"):
            result = setup.execute_plan(cfg, plan, workspace=ws, connection_factory=factory)
        self.assertTrue(result["ok"])
        factory.assert_not_called()

    def test_snapshot_failure_blocks_writes_before_any_selected_action(self):
        cfg = test_config()
        custom = replace(
            entry("codex"),
            params=replace(entry("codex").params, backup_paths=("~/.config/codex/settings.json",)),
        )
        plan = setup.build_plan(cfg, TARGET, (custom,))
        ws = Mock()

        def guest_call(_ws, _cfg, operation, **values):
            if operation == "snapshot":
                raise setup.AppError("snapshot failed")
            return {"ok": True}

        with patch.object(setup, "require_tool"), \
             patch.object(setup, "guest", side_effect=guest_call), \
             patch.object(setup, "preflight_entry", return_value={"installed": True}) as preflight, \
             patch.object(setup, "apply_entry") as apply, \
             patch.object(setup, "record_entry_state") as record:
            result = setup.execute_plan(cfg, plan, workspace=ws)
        self.assertFalse(result["ok"])
        self.assertEqual(result["results"][0]["status"], "blocked")
        self.assertIn("snapshot failed", result["results"][0]["detail"])
        preflight.assert_called_once()
        apply.assert_not_called()
        record.assert_not_called()


class SetupStatusCLITests(unittest.TestCase):
    def test_setup_status_is_read_only_and_uses_one_workspace_connection(self):
        from homestack import setup_cli

        cfg = test_config()
        state = {
            "ok": True,
            "checked_at": "2026-09-11T00:00:00Z",
            "state_path": "~/.local/state/homestack/setup.json",
            "registry_present": False,
            "items": {"codex": {"id": "codex", "group": "app", "label": "Codex", "handler": "application", "state": "installed", "ready": True, "managed": False, "files": []}},
        }
        workspace = Mock()
        manager = Mock()
        manager.__enter__ = Mock(return_value=workspace)
        manager.__exit__ = Mock(return_value=False)
        output = StringIO()
        with patch("sys.argv", ["homestack", "setup", "status", "workspace", "--json"]), \
             patch("sys.stdout", output), \
             patch.object(cli, "load_config", return_value=cfg), \
             patch.object(setup_cli, "load_catalog") as load_catalog, \
             patch.object(setup_cli, "open_transport"), \
             patch.object(setup_cli, "resolve_target", return_value=TARGET), \
             patch.object(setup_cli.WorkspaceSSH, "configured", return_value=manager), \
             patch.object(setup_cli, "inspect_workspace_state", return_value=state):
            load_catalog.return_value = setup_catalog.load_catalog(cfg)
            code = cli.main()
        self.assertEqual(code, 0)
        payload = json.loads(output.getvalue())
        self.assertEqual(payload["command"], "setup status")
        self.assertEqual(payload["items"]["codex"]["state"], "installed")
        manager.__enter__.assert_called_once()
        manager.__exit__.assert_called_once()


class SetupTUIStateTests(unittest.IsolatedAsyncioTestCase):
    async def test_ready_item_is_green_and_selected_ready_item_is_red(self):
        state = {
            "items": {"codex": {"state": "installed", "ready": True, "will_overwrite": True}},
            "checked_at": "2026-09-11T00:00:00Z",
            "registry_present": False,
            "state_path": "~/.local/state/homestack/setup.json",
        }
        app = SetupApp(test_config(), TARGET, state=state)
        async with app.run_test(size=(120, 30)) as pilot:
            codex = entry("codex")
            self.assertEqual(app.entry_style(codex), "green")
            app.toggle_node(app.nodes["codex"])
            await pilot.pause()
            self.assertEqual(app.entry_style(codex), "bold red")
