from __future__ import annotations

from dataclasses import replace
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from homestack import setup, setup_config as definitions, setup_guest
from homestack.models import AppError
from support import example_config, example_setup, test_config


TARGET = {"vmid": 200, "name": "workspace", "home": "/home/user", "user": "user"}


def codex():
    return next(e for e in example_setup().items if e.id == "codex")


def report(status="ok", overall="fail"):
    return json.dumps({"overallStatus": overall, "checks": {"config.load": {"status": status}}})


class ValidatorTests(unittest.TestCase):
    def test_guest_payload_uses_stdin_not_process_arguments(self):
        ws = Mock()
        ws.run.return_value = subprocess.CompletedProcess([], 0, '{"ok": true}', "")
        setup.guest(ws, test_config(), "structured-write", relative="config.json", content="PRIVATE" * 50000)
        self.assertNotIn("PRIVATE", ws.run.call_args.args[0])
        self.assertIn("PRIVATE", ws.run.call_args.kwargs["input_text"])

    def test_exit_code_validator(self):
        original = codex()
        validator = replace(original.params.validation, type="exit-code", path=(), accepted=())
        application = replace(original, params=replace(original.params, validation=validator))
        for code in (0, 1, 127):
            with self.subTest(code=code):
                ws = Mock()
                ws.run.return_value = subprocess.CompletedProcess([], code, "SECRET", "SECRET")
                if code:
                    with self.assertRaisesRegex(AppError, "validator exited"):
                        setup.validate_application_config(ws, test_config(), application)
                else:
                    setup.validate_application_config(ws, test_config(), application)

    def test_status_field_is_the_only_compatibility_signal(self):
        application = codex()
        for status in ("ok", "warning"):
            with self.subTest(status=status):
                ws = Mock()
                ws.run.return_value = subprocess.CompletedProcess([], 1, report(status), "SECRET")
                setup.validate_application_config(ws, test_config(), application)

    def test_invalid_reports_are_sanitized(self):
        application = codex()
        for code, output, error in ((1, report("fail"), "config.load=fail"),
                                    (0, "SECRET invalid JSON", "malformed JSON"),
                                    (0, "{}", "missing status path"),
                                    (0, report("SECRET"), "unaccepted status"),
                                    (0, report({"SECRET": "value"}), "unaccepted status"),
                                    (0, report(["SECRET"]), "unaccepted status"),
                                    (0, report(None), "unaccepted status"),
                                    (127, report(), "unavailable"),
                                    (255, report(), "unavailable")):
            with self.subTest(code=code, output=output):
                ws = Mock()
                ws.run.return_value = subprocess.CompletedProcess([], code, output, "SECRET")
                with self.assertRaisesRegex(AppError, error) as raised:
                    setup.validate_application_config(ws, test_config(), application)
                self.assertNotIn("SECRET", str(raised.exception))


class StructuredExecutionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.path = self.home / ".codex/config.toml"
        self.path.parent.mkdir()
        self.path.write_text('model = "private-workspace-model"\n[tui]\ntheme = "custom"\n')
        self.cfg = example_config()
        self.app = codex()
        self.leaves = definitions.config_entries(self.app)
        self.events = []
        self.validator_report = report()
        self.ws = Mock()
        self.ws.run.side_effect = self.command

    def command(self, command, **kwargs):
        if "doctor" in command:
            self.events.append("validator")
            return subprocess.CompletedProcess([], 1, self.validator_report, "SECRET")
        if "install.sh" in command:
            self.events.append("installer")
        return subprocess.CompletedProcess([], 0, "", "")

    def remote(self, ws, cfg, operation, **values):
        self.events.append(operation)
        if operation == "identity":
            return {"ok": True}
        try:
            return setup_guest.run({"home": str(self.home), "operation": operation, **values})
        except setup_guest.GuestError as exc:
            raise AppError(str(exc)) from None

    def execute(self, entries, *, expand=False):
        plan = setup.build_plan(self.cfg, TARGET, entries, include_configs=expand)
        with patch.object(setup, "guest", side_effect=self.remote), patch.object(setup, "require_tool"):
            return setup.execute_plan(self.cfg, plan, workspace=self.ws)

    def test_one_write_and_snapshot_for_many_leaves_and_final_validation_order(self):
        result = self.execute((self.app,), expand=True)
        self.assertTrue(result["ok"], result)
        self.assertEqual(self.events.count("structured-write"), 1)
        self.assertEqual(self.events.count("snapshot"), 1)
        self.assertEqual(self.events.count("structured-read"), 2)
        self.assertLess(self.events.index("snapshot"), self.events.index("installer"))
        self.assertLess(self.events.index("installer"), self.events.index("structured-write"))
        self.assertLess(self.events.index("structured-write"), self.events.index("validator"))
        self.assertLess(self.events.index("validator"), self.events.index("state-record"))
        self.assertNotIn("private-workspace-model", json.dumps(result))
        registry = (self.home / ".local/state/homestack/setup.json").read_text()
        self.assertNotIn("private-workspace-model", registry)
        self.assertNotIn("candidate", registry)

    def test_noop_has_no_write_or_snapshot_and_unrelated_keys_are_not_drift(self):
        self.assertTrue(self.execute(self.leaves)["ok"])
        self.events.clear()
        inode = self.path.stat().st_ino
        result = self.execute(self.leaves)
        self.assertTrue(result["ok"], result)
        self.assertTrue(all(r["status"] == "already-ready" for r in result["results"]))
        self.assertNotIn("structured-write", self.events)
        self.assertNotIn("snapshot", self.events)
        self.assertNotIn("validator", self.events)
        self.assertEqual(inode, self.path.stat().st_ino)

    def test_validator_runs_for_installer_only(self):
        before = self.path.read_bytes()
        result = self.execute((self.app,))
        self.assertTrue(result["ok"], result)
        self.assertLess(self.events.index("installer"), self.events.index("validator"))
        self.assertNotIn("structured-write", self.events)
        self.assertEqual(self.path.read_bytes(), before)

    def test_installer_only_failure_preserves_config_and_does_not_record_state(self):
        before = self.path.read_bytes()
        self.validator_report = report("fail")
        result = self.execute((self.app,))
        self.assertFalse(result["ok"])
        self.assertIn("config.load=fail", result["results"][0]["detail"])
        self.assertEqual(self.path.read_bytes(), before)
        self.assertNotIn("structured-restore", self.events)
        self.assertNotIn("state-record", self.events)

    def test_failed_validation_restores_original_bytes_and_mode(self):
        before = self.path.read_bytes()
        self.path.chmod(0o640)
        self.validator_report = report("fail")
        result = self.execute((self.app,), expand=True)
        self.assertFalse(result["ok"])
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o640)
        self.assertEqual(self.events.count("structured-restore"), 1)
        self.assertNotIn("state-record", self.events)

    def test_failed_validation_removes_only_new_generated_file(self):
        self.path.unlink()
        self.validator_report = report("fail")
        result = self.execute(self.leaves)
        self.assertFalse(result["ok"])
        self.assertFalse(self.path.exists())
        self.assertNotIn("state-record", self.events)

    def test_invalid_status_types_restore_generated_configuration(self):
        before = self.path.read_bytes()
        for status in ({"status": "fail"}, ["fail"], None):
            with self.subTest(status=status):
                self.validator_report = report(status)
                self.events.clear()
                result = self.execute(self.leaves)
                self.assertFalse(result["ok"])
                self.assertEqual(self.path.read_bytes(), before)
                self.assertIn("structured-restore", self.events)
                self.assertNotIn("state-record", self.events)

    def test_structural_conflict_blocks_installer_and_all_mutations(self):
        self.path.write_text('tui = "not a table"\n')
        before = self.path.read_bytes()
        result = self.execute((self.app,), expand=True)
        self.assertFalse(result["ok"])
        self.assertNotIn("installer", self.events)
        self.assertNotIn("snapshot", self.events)
        self.assertNotIn("structured-write", self.events)
        self.assertEqual(self.path.read_bytes(), before)

    def test_leaf_inspection_is_batched_and_exposes_only_declared_values(self):
        self.path.write_text('approval_policy = "never"\nsandbox_mode = "other"\nmodel = "SECRET"\n')
        with patch.object(setup, "guest", side_effect=self.remote), patch.object(setup, "require_tool"):
            state = setup.inspect_workspace_state(self.ws, self.cfg, TARGET, (self.app,))
        by_key = {tuple(item["key"]): item for item in state["items"].values() if "key" in item}
        self.assertEqual(by_key[("approval_policy",)]["state"], "matching")
        self.assertEqual(by_key[("sandbox_mode",)]["state"], "different")
        self.assertEqual(by_key[("approvals_reviewer",)]["state"], "missing")
        self.assertEqual(self.events.count("structured-read"), 1)
        self.assertNotIn("SECRET", json.dumps(state))

    def test_structural_inspection_keeps_independent_leaf_states(self):
        self.path.write_text('approval_policy = "never"\ntui = "incompatible"\n')
        with patch.object(setup, "guest", side_effect=self.remote), patch.object(setup, "require_tool"):
            state = setup.inspect_workspace_state(self.ws, self.cfg, TARGET, (self.app,))
        by_key = {tuple(item["key"]): item["state"] for item in state["items"].values() if "key" in item}
        self.assertEqual(by_key[("approval_policy",)], "matching")
        self.assertEqual(by_key[("approvals_reviewer",)], "missing")
        self.assertEqual(by_key[("tui", "status_line")], "unavailable")
        self.assertEqual(self.events.count("structured-read"), 1)

    def test_file_copy_conflicts_with_structured_patch_but_siblings_do_not(self):
        copy = definitions.Entry("copy", "files", "file", "Copy", "Copy whole file", definitions.FileParams("~/.codex/config.toml"))
        with patch.object(Path, "home", return_value=self.home):
            with self.assertRaisesRegex(AppError, "Overlapping"):
                setup.build_plan(self.cfg, TARGET, (copy, self.leaves[0]))
        plan = setup.build_plan(self.cfg, TARGET, self.leaves)
        self.assertEqual(len(plan.entries), len(self.leaves))
        self.assertEqual(plan.public()["actions"][0]["key"], list(self.leaves[0].params.key))

    def test_multiple_config_files_validate_once_and_restore_only_changed_files(self):
        first = self.app.params.config_files[0]
        second = replace(first, id="other", path="~/.codex/other.json", format="json", values={"enabled": True})
        unchanged = replace(first, id="unchanged", path="~/.codex/unchanged.yaml", format="yaml", values={"enabled": True})
        other_path = self.home / ".codex/other.json"
        other_path.write_text('{"unrelated": "preserve"}\n')
        unchanged_path = self.home / ".codex/unchanged.yaml"
        unchanged_path.write_text("enabled: true\ncustom: preserve\n")
        before = {p: p.read_bytes() for p in (self.path, other_path, unchanged_path)}
        self.app = replace(self.app, params=replace(self.app.params, config_files=(first, second, unchanged)))
        self.validator_report = report("fail")
        result = self.execute((self.app,), expand=True)
        self.assertFalse(result["ok"])
        self.assertEqual(self.events.count("validator"), 1)
        self.assertEqual(self.events.count("structured-write"), 2)
        self.assertEqual(self.events.count("structured-restore"), 2)
        self.assertLess(max(i for i, value in enumerate(self.events) if value == "structured-write"), self.events.index("validator"))
        self.assertEqual({p: p.read_bytes() for p in before}, before)
        self.assertNotIn("state-record", self.events)

    def test_application_without_validator_still_parses_all_formats(self):
        for format, content in (("toml", "broken = ["), ("json", '{"broken":'), ("yaml", "broken: [")):
            with self.subTest(format=format):
                config = replace(self.app.params.config_files[0], format=format, values={"enabled": True})
                application = replace(self.app, params=replace(self.app.params, validation=None, config_files=(config,)))
                self.path.write_text(content)
                self.events.clear()
                result = self.execute((application,), expand=True)
                self.assertFalse(result["ok"])
                self.assertNotIn("installer", self.events)
                self.assertNotIn("validator", self.events)
                self.assertEqual(self.path.read_text(), content)
                self.path.unlink()
                self.events.clear()
                result = self.execute((application,), expand=True)
                self.assertTrue(result["ok"], result)
                self.assertNotIn("validator", self.events)

    def test_failed_application_does_not_restore_another_applications_config(self):
        other_file = replace(self.app.params.config_files[0], id="other", path="~/other.json", format="json", values={"enabled": True})
        other = replace(self.app, id="other", label="Other", params=replace(self.app.params, validation=None, config_files=(other_file,)))
        before = self.path.read_bytes()
        self.validator_report = report("fail")
        result = self.execute((other, self.app), expand=True)
        self.assertFalse(result["ok"])
        self.assertEqual(json.loads((self.home / "other.json").read_text()), {"enabled": True})
        self.assertEqual(self.path.read_bytes(), before)
        registry = json.loads((self.home / ".local/state/homestack/setup.json").read_text())["items"]
        self.assertIn("other", registry)
        self.assertNotIn("codex", registry)

    def test_installer_changes_are_preserved_in_merged_final_document(self):
        def command(command, **kwargs):
            if "install.sh" in command:
                self.path.write_text('model = "installer-choice"\n')
            return self.command(command, **kwargs)
        self.ws.run.side_effect = command
        result = self.execute((self.app,), expand=True)
        self.assertTrue(result["ok"], result)
        self.assertIn('model = "installer-choice"', self.path.read_text())
        self.assertIn('approval_policy = "never"', self.path.read_text())

    def test_config_change_after_preflight_blocks_patch(self):
        def remote(ws, cfg, operation, **values):
            result = self.remote(ws, cfg, operation, **values)
            if operation == "snapshot":
                self.path.write_text('model = "concurrent-edit"\n')
            return result
        plan = setup.build_plan(self.cfg, TARGET, self.leaves)
        with patch.object(setup, "guest", side_effect=remote), patch.object(setup, "require_tool"):
            result = setup.execute_plan(self.cfg, plan, workspace=self.ws)
        self.assertFalse(result["ok"])
        self.assertEqual(self.path.read_text(), 'model = "concurrent-edit"\n')
        self.assertNotIn("structured-write", self.events)
