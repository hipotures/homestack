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

    def test_state_record_preserves_snapshot_for_noop_and_replaces_new_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / ".bashrc").write_text("export CUSTOM=1\n", encoding="utf-8")
            first_snapshot = setup_guest.run({
                "operation": "snapshot",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "items": ["bash"],
                "paths": [".bashrc"],
            })
            self.assertTrue(first_snapshot["created"])

            first_apply = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": ["~/.local/bin"],
                "apply": True,
            })
            self.assertTrue(first_apply["changed"])
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "bash",
                "handler": "environment",
                "paths": first_apply["paths"],
                "snapshot": first_snapshot["id"],
            })

            no_op = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": ["~/.local/bin"],
                "apply": True,
            })
            self.assertFalse(no_op["changed"])
            preserved = setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "bash",
                "handler": "environment",
                "paths": no_op["paths"],
                "snapshot": None,
            })["item"]
            self.assertEqual(preserved["last_snapshot"], first_snapshot["id"])

            (home / ".bashrc").write_text("export CUSTOM=2\n", encoding="utf-8")
            second_snapshot = setup_guest.run({
                "operation": "snapshot",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "items": ["bash"],
                "paths": [".bashrc"],
            })
            self.assertTrue(second_snapshot["created"])
            self.assertNotEqual(second_snapshot["id"], first_snapshot["id"])
            replaced = setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": 200,
                "name": "workspace",
                "id": "bash",
                "handler": "environment",
                "paths": no_op["paths"],
                "snapshot": second_snapshot["id"],
            })["item"]
            self.assertEqual(replaced["last_snapshot"], second_snapshot["id"])

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
    def test_matching_environment_reports_no_files_changed(self):
        cfg = test_config()
        bash = entry("bash")
        plan = setup.build_plan(cfg, TARGET, (bash,))
        with patch.object(setup, "guest", return_value={"changed": []}):
            result = setup.apply_entry(Mock(), cfg, plan, bash, {}, lambda operation: operation())
        self.assertEqual(result, (
            "already-ready", "Shell configuration already matches; no files changed",
        ))

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

    def test_environment_inspection_reports_file_drift_requiring_overwrite(self):
        cfg = test_config()
        bash = entry("bash")
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            applied = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": setup.all_bins(cfg),
                "apply": True,
            })
            self.assertTrue(applied["changed"])
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": TARGET["vmid"],
                "name": TARGET["name"],
                "id": bash.id,
                "handler": bash.handler,
                "paths": applied["paths"],
                "snapshot": None,
            })

            ws = Mock()
            ws.run.return_value = Mock(returncode=0, stdout="", stderr="")

            def inspect_guest(_ws, _cfg, operation, **values):
                if operation == "identity":
                    return {"ok": True}
                return setup_guest.run({"operation": operation, "home": str(home), **values})

            def inspect():
                with patch.object(setup, "require_tool"), patch.object(setup, "guest", side_effect=inspect_guest):
                    return setup.inspect_workspace_state(ws, cfg, TARGET, (bash,))["items"][bash.id]

            unchanged = inspect()
            self.assertTrue(unchanged["ready"])
            self.assertFalse(unchanged["will_overwrite"])
            self.assertEqual(unchanged["changed_since_apply"], [])

            with (home / ".profile").open("a", encoding="utf-8") as handle:
                handle.write("#\n")
            modified = inspect()
            self.assertFalse(modified["ready"])
            self.assertTrue(modified["will_overwrite"])
            self.assertEqual(modified["changed_since_apply"], [".profile"])
            self.assertEqual(modified["state"], "needs update")

            (home / ".bashrc").unlink()
            missing = inspect()
            self.assertIn(".bashrc", missing["changed_since_apply"])
            self.assertIn(".profile", missing["changed_since_apply"])

    def test_execute_plan_snapshots_external_shell_drift_before_recording_baseline(self):
        cfg = test_config()
        bash = entry("bash")
        plan = setup.build_plan(cfg, TARGET, (bash,))
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            initial = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": setup.all_bins(cfg),
                "apply": True,
            })
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": TARGET["vmid"],
                "name": TARGET["name"],
                "id": bash.id,
                "handler": bash.handler,
                "paths": initial["paths"],
                "snapshot": None,
            })
            root = home / ".local/state/homestack"
            baseline = json.loads((root / "setup.json").read_text())
            baseline_profile = next(item for item in baseline["items"][bash.id]["files"] if item["path"] == ".profile")
            original_profile = (home / ".profile").read_bytes()
            original_profile_inode = (home / ".profile").stat().st_ino
            original_bashrc = (home / ".bashrc").read_bytes()
            original_bashrc_inode = (home / ".bashrc").stat().st_ino
            with (home / ".profile").open("a", encoding="utf-8") as handle:
                handle.write("# external edit\n")
            modified_profile = (home / ".profile").read_bytes()

            workspace = Mock()
            workspace.run.return_value = Mock(returncode=0, stdout="", stderr="")

            def route_guest(_ws, _cfg, operation, **values):
                if operation == "identity":
                    return {"ok": True}
                return setup_guest.run({"operation": operation, "home": str(home), **values})

            with patch.object(setup, "guest", side_effect=route_guest), patch.object(setup, "require_tool"):
                result = setup.execute_plan(cfg, plan, workspace=workspace)

            self.assertTrue(result["ok"])
            self.assertEqual(result["results"][0]["status"], "succeeded")
            self.assertEqual(result["results"][0]["detail"], "Shell configuration verified")
            self.assertTrue(result["snapshot"]["created"])
            snapshot = root / result["snapshot"]["id"]
            manifest = json.loads((snapshot / "snapshot.json").read_text())
            self.assertEqual([item["path"] for item in manifest["files"]], [".profile"])
            self.assertEqual((snapshot / "home/.profile").read_bytes(), modified_profile)
            self.assertNotEqual((home / ".profile").stat().st_ino, original_profile_inode)
            self.assertEqual((home / ".profile").read_bytes(), original_profile)
            self.assertNotEqual((home / ".profile").read_bytes(), modified_profile)
            self.assertEqual((home / ".bashrc").stat().st_ino, original_bashrc_inode)
            self.assertEqual((home / ".bashrc").read_bytes(), original_bashrc)
            before = json.loads((snapshot / "setup.before.json").read_text())
            before_profile = next(item for item in before["items"][bash.id]["files"] if item["path"] == ".profile")
            self.assertEqual(before_profile["sha256"], baseline_profile["sha256"])
            current = json.loads((root / "setup.json").read_text())
            current_profile = next(item for item in current["items"][bash.id]["files"] if item["path"] == ".profile")
            self.assertEqual(current_profile["sha256"], setup_guest.path_metadata(home, ".profile")["sha256"])

            snapshots = sorted(path.name for path in root.iterdir() if path.is_dir())
            with patch.object(setup, "guest", side_effect=route_guest), patch.object(setup, "require_tool"):
                repeated = setup.execute_plan(cfg, plan, workspace=workspace)
            self.assertTrue(repeated["ok"])
            self.assertEqual(repeated["results"][0]["status"], "already-ready")
            self.assertFalse(repeated["snapshot"]["created"])
            self.assertEqual(snapshots, sorted(path.name for path in root.iterdir() if path.is_dir()))

    def test_execute_plan_snapshot_failure_leaves_shell_and_registry_untouched(self):
        cfg = test_config()
        bash = entry("bash")
        plan = setup.build_plan(cfg, TARGET, (bash,))
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            initial = setup_guest.run({
                "operation": "environment",
                "home": str(home),
                "profile": "bash",
                "bins": setup.all_bins(cfg),
                "apply": True,
            })
            setup_guest.run({
                "operation": "state-record",
                "home": str(home),
                "vmid": TARGET["vmid"],
                "name": TARGET["name"],
                "id": bash.id,
                "handler": bash.handler,
                "paths": initial["paths"],
                "snapshot": None,
            })
            with (home / ".profile").open("a", encoding="utf-8") as handle:
                handle.write("# external edit\n")
            profile_before = (home / ".profile").read_bytes()
            profile_inode_before = (home / ".profile").stat().st_ino
            registry_path = home / ".local/state/homestack/setup.json"
            registry_before = registry_path.read_bytes()
            workspace = Mock()
            workspace.run.return_value = Mock(returncode=0, stdout="", stderr="")

            def route_guest(_ws, _cfg, operation, **values):
                if operation == "identity":
                    return {"ok": True}
                if operation == "snapshot":
                    raise setup.AppError("snapshot failed")
                return setup_guest.run({"operation": operation, "home": str(home), **values})

            with patch.object(setup, "guest", side_effect=route_guest), patch.object(setup, "require_tool"):
                result = setup.execute_plan(cfg, plan, workspace=workspace)

            self.assertFalse(result["ok"])
            self.assertEqual(result["results"][0]["status"], "blocked")
            self.assertFalse(result["snapshot"]["created"])
            self.assertEqual((home / ".profile").read_bytes(), profile_before)
            self.assertEqual((home / ".profile").stat().st_ino, profile_inode_before)
            self.assertEqual(registry_path.read_bytes(), registry_before)

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
