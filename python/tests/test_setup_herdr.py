from __future__ import annotations

from io import StringIO
import json
from pathlib import Path
import shlex
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch

from homestack import cloudinit, setup, setup_catalog, setup_config, setup_guest, setup_herdr as herdr
from homestack.models import AppError
from homestack.setup_tui import SetupApp
from support import example_setup, test_config

TARGET = {"vmid": 200, "name": "gpu", "ip": "192.0.2.200", "user": "user", "home": "/home/user"}
PROFILE = {"id": "profile-id", "target": "user@gpu", "label": "gpu", "session": "default", "enabled": True}


def completed(code=0, output=""):
    return subprocess.CompletedProcess([], code, output, "PRIVATE_OUTPUT")


class HerdrSetupTests(unittest.TestCase):
    def setUp(self):
        self.cfg = test_config()
        self.entry = next(e for e in self.cfg.setup.items if e.id == "herdr")
        self.ws = Mock()
        self.commands = []
        self.sessions = []
        self.active = False
        self.state = {"installed": True, "changed": True, "files": []}
        self.activity = Mock()
        self.guest = patch.object(setup, "guest", return_value={"ok": True}).start()
        patch.object(herdr, "inspect", return_value={"ready": True}).start()
        self.addCleanup(patch.stopall)

        def run(command, **kwargs):
            script = shlex.split(command)[-1]
            self.commands.append(script)
            if "session list --json" in script:
                return completed(output=json.dumps({"sessions": [{"name": s, "running": True} for s in self.sessions]}))
            if "is-active herdr.service" in script:
                return completed(0 if self.active else 3)
            if "start herdr.service" in script:
                self.active = True
            return completed()
        self.ws.run.side_effect = run

    def apply(self):
        return herdr.apply(self.ws, self.cfg, TARGET, self.state,
                           activity=self.activity)

    def test_default_and_example_catalog_put_herdr_after_claude(self):
        for cfg in (self.cfg.setup, setup_config.parse_setup({}), example_setup()):
            self.assertEqual([e.id for e in cfg.items if e.group == "app"][:3], ["codex", "claude", "herdr"])
        restored = setup_config.parse_setup(tomllib.loads(setup_config.setup_to_toml(self.cfg.setup))["setup"])
        self.assertEqual(restored, self.cfg.setup)
        row = next(r for r in setup_catalog.load_catalog(self.cfg).rows(self.cfg) if r["id"] == "herdr")
        self.assertEqual(row["index"], 3)
        self.assertEqual(row["selector"], "app=herdr")

    def test_plan_owns_service_path_and_rejects_overlap(self):
        conflicting = setup_config.Entry("unit", "files", "file", "Unit", "", setup_config.FileParams("~/" + herdr.UNIT_PATH))
        with patch.object(setup, "source_item"), self.assertRaisesRegex(AppError, "Overlapping"):
            setup.build_plan(self.cfg, TARGET, (self.entry, conflicting))
        action = setup.build_plan(self.cfg, TARGET, (self.entry,)).public()["actions"][0]
        self.assertEqual(action["desktop_label"], "gpu")
        self.assertIn("authentication", action["security_key_touch"])

    def test_new_install_starts_service_before_desktop_registration(self):
        self.state["installed"] = False
        with patch.object(herdr, "connect_desktop") as connect:
            connect.side_effect = lambda *a, **kw: self.assertTrue(self.active)
            status, detail = self.apply()
        self.assertEqual(status, "succeeded")
        self.assertIn("enabled and active", detail)
        scripts = "\n".join(self.commands)
        self.assertIn(herdr.INSTALL_COMMAND, scripts)
        self.assertIn("XDG_RUNTIME_DIR=/run/user/1000", scripts)
        self.assertIn("loginctl --no-ask-password enable-linger user", scripts)
        self.assertLess(scripts.index(herdr.INSTALL_COMMAND), scripts.index("enable-linger"))

    def test_existing_unmanaged_sessions_never_update_start_or_stop(self):
        self.sessions = ["default", "agents"]
        with patch.object(herdr, "connect_desktop"):
            _, detail = self.apply()
        self.assertIn("existing sessions preserved", detail)
        scripts = "\n".join(self.commands)
        for forbidden in (herdr.INSTALL_COMMAND, "herdr update", "start herdr.service", "restart", "server stop", "session stop"):
            self.assertNotIn(forbidden, scripts)
        self.assertIn("enable herdr.service", scripts)

    def test_active_service_is_not_restarted_or_updated(self):
        self.active = True
        self.sessions = ["default"]
        with patch.object(herdr, "connect_desktop"):
            self.apply()
        self.assertFalse(any("start herdr.service" in c or "herdr update" in c for c in self.commands))

    def test_failed_installer_does_not_configure_service_or_desktop(self):
        self.state["installed"] = False
        self.ws.run.return_value = completed(1)
        self.ws.run.side_effect = None
        with patch.object(herdr, "connect_desktop") as connect, self.assertRaisesRegex(AppError, "installer failed"):
            self.apply()
        connect.assert_not_called()
        self.guest.assert_not_called()

    def test_failed_linger_does_not_write_service_or_connect(self):
        self.ws.run.side_effect = [completed(), completed(1)]
        with patch.object(herdr, "connect_desktop") as connect, self.assertRaisesRegex(AppError, "administrator"):
            self.apply()
        self.guest.assert_not_called()
        connect.assert_not_called()

    def test_invalid_session_inspection_refuses_to_start(self):
        self.ws.run.return_value = completed(output='{"sessions":[{"name":"default"}]}')
        self.ws.run.side_effect = None
        with self.assertRaisesRegex(AppError, "refusing"):
            herdr.running_sessions(self.ws, self.cfg)

    def test_service_install_is_atomic_and_cannot_overwrite_concurrent_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            def guest(ws, cfg, operation, **values):
                return setup_guest.run({"home": directory, "operation": operation, **values})
            with patch.object(setup, "guest", side_effect=guest), patch.object(herdr, "connect_desktop"):
                self.apply()
                unit = home / herdr.UNIT_PATH
                self.assertEqual(unit.read_bytes(), herdr.unit_content())
                self.assertEqual(unit.stat().st_mode & 0o777, 0o600)
                self.assertIn("ExecStartPre=-%h/.local/bin/herdr update", unit.read_text())
                unit.write_text("concurrent edit")
                with self.assertRaisesRegex(setup_guest.GuestError, "changed since preflight"):
                    self.apply()
                self.assertEqual(unit.read_text(), "concurrent edit")

    def test_existing_desktop_profile_is_renamed_and_enabled_without_authentication(self):
        profile = dict(PROFILE, label="old", enabled=False)
        with patch.object(herdr, "desktop_profile", return_value=profile), patch.object(herdr, "run_local", return_value=completed()) as run:
            desktop = Mock()
            herdr.connect_desktop(self.cfg, TARGET, desktop=desktop, activity=self.activity)
        desktop.assert_not_called()
        self.assertEqual([c.args[0] for c in run.call_args_list], [
            ["herdr", "machine", "rename", "profile-id", "--label", "gpu"],
            ["herdr", "machine", "enable", "profile-id"],
        ])

    def test_new_desktop_profile_uses_vm_label_and_visible_security_key_notice(self):
        with patch.object(herdr, "desktop_profile", return_value=None), patch.object(herdr, "verify_desktop_alias"), patch.object(herdr.subprocess, "run", return_value=completed()) as run, patch.object(herdr.sys, "stderr", new_callable=StringIO) as output:
            herdr.connect_desktop(self.cfg, TARGET, activity=self.activity)
        run.assert_called_once_with(["herdr", "machine", "add", "user@gpu", "--label", "gpu", "--remote-session", "default"], text=True)
        self.assertIn("YubiKey", output.getvalue())

    def test_desktop_failure_is_reported_as_partial(self):
        with patch.object(herdr, "desktop_profile", return_value=None), patch.object(herdr, "verify_desktop_alias"), patch.object(herdr.subprocess, "run", return_value=completed(1)), patch.object(herdr.sys, "stderr", new_callable=StringIO):
            with self.assertRaisesRegex(AppError, "guest service is configured"):
                herdr.connect_desktop(self.cfg, TARGET, activity=self.activity)

    def test_tui_interactive_decision_failure_shows_manual_command_without_external_output(self):
        desktop = Mock(return_value=completed(1, "requires interactive approval: PRIVATE_OUTPUT"))
        with patch.object(herdr, "desktop_profile", return_value=None), patch.object(herdr, "verify_desktop_alias"):
            with self.assertRaisesRegex(AppError, "interactive decision") as raised:
                herdr.connect_desktop(self.cfg, TARGET, desktop=desktop, activity=self.activity)
        self.assertIn("herdr machine add user@gpu", str(raised.exception))
        self.assertNotIn("PRIVATE_OUTPUT", str(raised.exception))

    def test_conflicting_label_blocks_and_ip_profile_is_reused(self):
        profile = dict(PROFILE, target="user@192.0.2.200")
        with patch.object(herdr, "run_local", return_value=completed(output=json.dumps([profile]))):
            self.assertEqual(herdr.desktop_profile(self.cfg, TARGET), profile)
        profile["target"] = "user@other"
        with patch.object(herdr, "run_local", return_value=completed(output=json.dumps([profile]))):
            with self.assertRaisesRegex(AppError, "label conflict"):
                herdr.desktop_profile(self.cfg, TARGET)

    def test_unattended_missing_desktop_profile_blocks_before_installer(self):
        with patch.object(herdr.shutil, "which", return_value="/bin/tool"), patch.object(setup, "require_tool"), patch.object(herdr, "inspect", return_value={"desktop_profile_exists": False}), patch.object(herdr, "_systemctl", return_value=completed()):
            with self.assertRaisesRegex(AppError, "interactive terminal"):
                herdr.preflight(self.ws, self.cfg, TARGET, unattended=True)
        self.ws.run.assert_not_called()

    def test_alias_must_resolve_to_exact_workspace_and_keys(self):
        output = "hostname 192.0.2.200\nuser user\nidentitiesonly yes\nidentityfile ~/.ssh/example-hardware-key\n"
        with patch.object(herdr, "run_local", return_value=completed(output=output)):
            herdr.verify_desktop_alias(self.cfg, TARGET)
        with patch.object(herdr, "run_local", return_value=completed(output=output.replace("192.0.2.200", "192.0.2.201"))):
            with self.assertRaisesRegex(AppError, "alias does not match"):
                herdr.verify_desktop_alias(self.cfg, TARGET)

    def test_blocked_herdr_preflight_prevents_other_selected_actions(self):
        codex = next(e for e in self.cfg.setup.items if e.id == "codex")
        plan = setup.build_plan(self.cfg, TARGET, (codex, self.entry))
        with patch.object(setup, "require_tool"), patch.object(setup, "preflight_entry", side_effect=[{}, AppError("Desktop conflict")]), patch.object(setup, "apply_entry") as apply:
            result = setup.execute_plan(self.cfg, plan, workspace=self.ws)
        self.assertFalse(result["ok"])
        self.assertEqual([r["status"] for r in result["results"]], ["not-run", "blocked"])
        apply.assert_not_called()

    def test_refresh_restores_linger_after_mounting_persistent_home(self):
        with patch.object(cloudinit, "node_run"), patch.object(cloudinit, "remote_write_text") as write:
            cloudinit.write_snippets(Mock(), self.cfg, "node", "gpu", 200, "aa:bb:cc:dd:ee:ff", TARGET["ip"], "HS_HOME_200", replace=True, preserve_home=True)
        vendor = next(call.args[4] for call in write.call_args_list if str(call.args[3]).endswith("vendor.yaml"))
        self.assertLess(vendor.index('mount "$home_path"'), vendor.index("loginctl enable-linger user"))
        self.assertIn('if [ -f "$home_path/.config/systemd/user/herdr.service" ]', vendor)

    def test_tui_details_explain_linger_and_separate_authentication(self):
        details = SetupApp(self.cfg, TARGET).details("herdr")
        self.assertIn("without a user login", details)
        self.assertIn("YubiKey", details)


class HerdrExecutionTests(unittest.TestCase):
    def execute(self, home, *, running=False, desktop_exit=0):
        cfg = test_config()
        entry = next(e for e in cfg.setup.items if e.id == "herdr")
        runtime = {"installed": running, "running": running, "active": False, "enabled": False, "linger": False, "profile": None}
        commands = []

        def run(command, **kwargs):
            command = shlex.split(command)[-1]
            commands.append(command)
            if '"$HOME/.local/bin/herdr" --version' in command:
                return completed(0 if runtime["installed"] else 127)
            if "session list --json" in command:
                return completed(output=json.dumps({"sessions": [{"name": "default", "running": runtime["running"]}]}))
            if herdr.INSTALL_COMMAND in command:
                runtime["installed"] = True
            if "show-user user" in command:
                return completed(output="yes\n" if runtime["linger"] else "no\n")
            if "enable-linger user" in command:
                runtime["linger"] = True
            if "is-enabled herdr.service" in command:
                return completed(0 if runtime["enabled"] else 1)
            if "is-active herdr.service" in command:
                return completed(0 if runtime["active"] else 3)
            if "enable herdr.service" in command:
                runtime["enabled"] = True
            if "start herdr.service" in command:
                runtime.update(active=True, running=True)
            return completed()

        def guest(ws, cfg, operation, **values):
            if operation == "identity":
                return {"ok": True}
            return setup_guest.run({"operation": operation, "home": str(home), **values})

        def register(command, **kwargs):
            if desktop_exit == 0:
                runtime["profile"] = PROFILE
            return completed(desktop_exit)

        workspace = Mock()
        workspace.run.side_effect = run
        with patch.object(setup, "guest", side_effect=guest), patch.object(herdr.shutil, "which", return_value="/bin/tool"), patch.object(herdr, "desktop_profile", side_effect=lambda *a: runtime["profile"]), patch.object(herdr, "verify_desktop_alias"), patch.object(herdr.subprocess, "run", side_effect=register), patch.object(herdr.sys, "stderr", new_callable=StringIO):
            result = setup.execute_plan(cfg, setup.build_plan(cfg, TARGET, (entry,)), workspace=workspace)
        return result, runtime, commands

    def test_full_fresh_setup_records_verified_service_and_install_time(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            result, runtime, commands = self.execute(home)
            self.assertTrue(result["ok"], result)
            self.assertTrue(runtime["active"])
            self.assertTrue(runtime["linger"])
            self.assertEqual((home / herdr.UNIT_PATH).read_bytes(), herdr.unit_content())
            state = json.loads((home / ".local/state/homestack/setup.json").read_text())
            record = state["items"]["herdr"]
            self.assertTrue(record["installed_at"])
            self.assertEqual(record["files"][0]["path"], herdr.UNIT_PATH)

    def test_full_setup_snapshots_previous_unit_and_preserves_unmanaged_server(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            unit = home / herdr.UNIT_PATH
            unit.parent.mkdir(parents=True)
            unit.write_text("old service")
            result, runtime, commands = self.execute(home, running=True)
            self.assertTrue(result["ok"], result)
            self.assertTrue(result["snapshot"]["created"])
            snapshot = home / ".local/state/homestack" / result["snapshot"]["id"]
            self.assertEqual((snapshot / "home" / herdr.UNIT_PATH).read_text(), "old service")
            self.assertTrue(runtime["running"])
            self.assertFalse(runtime["active"])
            self.assertFalse(any("start herdr.service" in c or herdr.INSTALL_COMMAND in c for c in commands))

    def test_failed_desktop_registration_keeps_guest_service_without_recording_success(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            result, runtime, commands = self.execute(home, desktop_exit=1)
            self.assertFalse(result["ok"])
            self.assertEqual(result["results"][0]["status"], "failed")
            self.assertIn("guest service is configured", result["results"][0]["detail"])
            self.assertTrue(runtime["active"])
            self.assertTrue((home / herdr.UNIT_PATH).exists())
            self.assertFalse((home / ".local/state/homestack/setup.json").exists())


if __name__ == "__main__":
    unittest.main()
