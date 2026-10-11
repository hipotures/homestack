import subprocess
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest import mock
from urllib.parse import quote

from homestack import addkey, config, workspace_ssh
from homestack.models import AppError
from support import test_config

YUBIKEY = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1 yubikey1"
TREZOR = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBHRyZXpvcg== trezor-vm"

VENDOR = """#cloud-config
runcmd:
  - |
      set -eu
      allow_format=1
      echo home step
  - |
      # HomeStack ensure-keys
      set -eu
      keys=$(cat <<'HS_KEYS'
      old-key-line
      HS_KEYS
      )
      echo old step
"""


def key_file(directory: str, line: str, name: str = "key.pub") -> Path:
    path = Path(directory) / name
    path.write_text(line + "\n", encoding="utf-8")
    return path


class KeyTests(unittest.TestCase):
    def test_public_key_must_be_one_named_key(self):
        with tempfile.TemporaryDirectory() as directory:
            record = addkey.read_public_key(key_file(directory, TREZOR))
            self.assertEqual((record["comment"], record["type"]), ("trezor-vm", "sk-ecdsa-sha2-nistp256@openssh.com"))
            self.assertEqual(record["line"], TREZOR)
            with self.assertRaisesRegex(AppError, "exactly one"):
                addkey.read_public_key(key_file(directory, TREZOR + "\n" + YUBIKEY))
            with self.assertRaisesRegex(AppError, "no comment"):
                addkey.read_public_key(key_file(directory, " ".join(TREZOR.split()[:2])))
            with self.assertRaisesRegex(AppError, "Cannot read"):
                addkey.read_public_key(Path(directory) / "missing.pub")

    def test_sshkeys_gain_the_key_once_and_keep_the_others(self):
        key = {"blob": TREZOR.split()[1], "line": TREZOR}
        current = quote(YUBIKEY + "\n", safe="")
        keys, present = addkey.merge_sshkeys(current, key)
        self.assertEqual((keys, present), ([YUBIKEY, TREZOR], False))
        keys, present = addkey.merge_sshkeys(quote("\n".join(keys) + "\n", safe=""), key)
        self.assertEqual((keys, present), ([YUBIKEY, TREZOR], True))
        self.assertEqual(addkey.merge_sshkeys("", key), ([TREZOR], False))


class VendorTests(unittest.TestCase):
    def test_ensure_step_is_valid_shell_and_adds_without_removing(self):
        step = addkey.ensure_keys_item([YUBIKEY, TREZOR], "user")
        script = "".join(line[6:] for line in step.splitlines(keepends=True)[1:])
        check = subprocess.run(["bash", "-n"], input=script, text=True, capture_output=True)
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertIn("for dir in /root /home/user; do", script)
        self.assertNotIn("rm ", script)
        self.assertNotIn(" > \"$f\"", script)

    def test_ensure_step_appends_only_missing_keys(self):
        step = addkey.ensure_keys_item([YUBIKEY, TREZOR], "user")
        script = "".join(line[6:] for line in step.splitlines(keepends=True)[1:])
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / "home"
            (home / ".ssh").mkdir(parents=True)
            target = home / ".ssh" / "authorized_keys"
            target.write_text("other-key AAAA other\n" + YUBIKEY, encoding="utf-8")  # no trailing newline
            patched = script.replace("for dir in /root /home/user; do", f"for dir in {home}; do")
            for _ in range(2):
                run = subprocess.run(["bash", "-c", patched], text=True, capture_output=True)
                self.assertEqual(run.returncode, 0, run.stderr)
            lines = target.read_text(encoding="utf-8").splitlines()
            self.assertEqual(lines, ["other-key AAAA other", YUBIKEY, TREZOR])

    def test_ensure_step_creates_a_missing_file_and_directory_with_private_modes(self):
        step = addkey.ensure_keys_item([YUBIKEY, TREZOR], "user")
        script = "".join(line[6:] for line in step.splitlines(keepends=True)[1:])
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / "home"
            home.mkdir()
            patched = script.replace("for dir in /root /home/user; do", f"for dir in {home} {Path(directory) / 'absent'}; do")
            for _ in range(2):
                run = subprocess.run(["bash", "-c", patched], text=True, capture_output=True)
                self.assertEqual(run.returncode, 0, run.stderr)
            target = home / ".ssh" / "authorized_keys"
            self.assertEqual(target.read_text(encoding="utf-8").splitlines(), [YUBIKEY, TREZOR])
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            self.assertEqual((home / ".ssh").stat().st_mode & 0o777, 0o700)
            self.assertFalse((Path(directory) / "absent").exists())

    def test_vendor_rewrite_stops_home_rewrite_and_keeps_one_ensure_step(self):
        result = addkey.update_vendor(VENDOR, [YUBIKEY, TREZOR], "user")
        self.assertIn("allow_format=0", result)
        self.assertNotIn("allow_format=1", result)
        self.assertNotIn("old-key-line", result)
        self.assertIn("home step", result)
        self.assertEqual(result.count(addkey.KEYS_HEREDOC + "'"), 1)
        self.assertEqual(result.count(addkey.ENSURE_MARKER), 1)
        self.assertTrue(result.endswith(f"echo '{addkey.ENSURE_DONE}'\n"))
        # a second run changes nothing
        self.assertEqual(addkey.update_vendor(result, [YUBIKEY, TREZOR], "user"), result)

    def test_vendor_without_the_home_step_is_refused(self):
        with self.assertRaisesRegex(AppError, "allow_format"):
            addkey.update_vendor("#cloud-config\nruncmd:\n  - echo hi\n", [YUBIKEY], "user")
        with self.assertRaisesRegex(AppError, "runcmd"):
            addkey.update_vendor("#cloud-config\nallow_format=1\n", [YUBIKEY], "user")


def info(status="running"):
    cicustom = ",".join(f"{kind}=local:snippets/homestack-cpu-{kind}.yaml" for kind in ("user", "vendor", "network", "meta"))
    return {"vmid": 206, "name": "cpu", "node": "pve2", "status": status, "ip": "192.0.2.206",
            "vm_config": {"cicustom": cicustom, "sshkeys": quote(YUBIKEY + "\n", safe="")}}


class FlowTests(unittest.TestCase):
    def setUp(self):
        self.cfg = test_config()
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.key = key_file(self.directory.name, TREZOR)
        Path(self.directory.name, "key").write_text("private\n", encoding="utf-8")

    def plan(self, **overrides):
        data = info(**overrides)
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data):
            return addkey.build_addkey_plan(object(), self.cfg, 206, self.key)

    def test_plan_names_the_key_and_every_step(self):
        plan = self.plan()
        self.assertEqual((plan["name"], plan["ip"], plan["key"]["comment"]), ("cpu", "192.0.2.206", "trezor-vm"))
        self.assertEqual(plan["keys"], [YUBIKEY, TREZOR])
        self.assertFalse(plan["key_already_in_sshkeys"])
        self.assertEqual(len(plan["steps"]), 9)
        self.assertIn("allow_format=0", " ".join(plan["steps"]))
        self.assertIn("stays stopped", " ".join(self.plan(status="stopped")["steps"]))

    def test_vm_without_homestack_snippets_is_refused(self):
        data = info()
        data["vm_config"]["cicustom"] = "user=local:snippets/other.yaml"
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data), \
                self.assertRaisesRegex(AppError, "does not use the HomeStack vendor snippet"):
            addkey.build_addkey_plan(object(), self.cfg, 206, self.key)

    def _execute(self, plan, *, cloud_init="status: done", counts="CHECKED",
                 fail_on=None, restart=True):
        events = []
        written = {}

        def node_run(_session, _cfg, _node, command, **_kwargs):
            events.append(command)
            if command.startswith("cat "):
                path = command.split(None, 1)[1].strip("'")
                return SimpleNamespace(output=VENDOR.replace("allow_format=1", "allow_format=1")
                                       if path.endswith("vendor.yaml") else "instance-id: cpu-206-old\nlocal-hostname: cpu\n")
            if fail_on and fail_on in command:
                raise AppError("boom")
            return SimpleNamespace(output="")

        def remote_write(_session, _cfg, _node, path, content, mode):
            events.append(f"write {path.name}")
            written.setdefault(path.name, []).append(content)

        def guest_out(_session, _cfg, _node, _vmid, command, **_kwargs):
            events.append("guest " + command.split()[0])
            return cloud_init if "cloud-init" in command else counts

        with mock.patch.object(addkey, "node_run", node_run), \
                mock.patch.object(addkey, "remote_write_text", remote_write), \
                mock.patch.object(addkey, "shutdown_vm_on_node", lambda *a, **k: events.append("shutdown")), \
                mock.patch.object(addkey, "wait_for_qga_on_node", lambda *a, **k: events.append("qga")), \
                mock.patch.object(addkey, "guest_out_on_node", guest_out), \
                mock.patch.object(addkey, "sync_local_ssh", return_value={"ssh_config": "x", "identity_files_changed": True}), \
                mock.patch.object(addkey, "forget_local_ssh_host", return_value=["192.0.2.206"]) as forget:
            try:
                asked = restart(events) if callable(restart) else restart
                result, error = addkey.add_key(object(), self.cfg, plan, asked), None
            except AppError as exc:
                result, error = None, str(exc)
        return result, error, events, written, forget

    def test_running_vm_gets_the_key_restarts_and_is_verified(self):
        result, error, events, written, forget = self._execute(self.plan())
        self.assertIsNone(error)
        self.assertEqual(result, {"ok": True, "command": "addkey", "vmid": 206, "name": "cpu", "key": "trezor-vm",
                                  "restarted": True, "pending_restart": False,
                                  "known_hosts_removed": ["192.0.2.206"], "verified": True,
                                  "local": {"ssh_config": "x", "identity_files_changed": True}})
        order = [next(i for i, e in enumerate(events) if marker in e)
                 for marker in ("qm set 206 --sshkeys", "write homestack-cpu-vendor.yaml", "write homestack-cpu-meta.yaml",
                                "qm cloudinit update 206", "shutdown", "qm start 206", "qga", "guest timeout", "guest missing=0;")]
        self.assertEqual(order, sorted(order))
        self.assertIn("allow_format=0", written["homestack-cpu-vendor.yaml"][-1])
        self.assertNotIn("old-key-line", written["homestack-cpu-vendor.yaml"][-1])
        self.assertNotEqual(written["homestack-cpu-meta.yaml"][-1], "instance-id: cpu-206-old\nlocal-hostname: cpu\n")
        self.assertRegex(written["homestack-cpu-meta.yaml"][-1], r"^instance-id: cpu-206-[0-9a-f]{12}\nlocal-hostname: cpu\n$")
        forget.assert_called_once_with("192.0.2.206")

    def test_running_vm_is_not_restarted_unless_asked(self):
        result, error, events, _written, forget = self._execute(self.plan(), restart=False)
        self.assertIsNone(error)
        self.assertEqual((result["restarted"], result["pending_restart"], result["verified"]), (False, True, False))
        self.assertTrue([event for event in events if event.startswith("qm cloudinit update")])
        self.assertNotIn("shutdown", events)
        self.assertFalse([event for event in events if event.startswith("qm start") or event.startswith("guest")])
        forget.assert_not_called()

    def test_restart_question_comes_last_and_a_no_leaves_the_vm_running(self):
        for answer in (True, False):
            def ask(events, answer=answer):
                def question():
                    events.append("ASK")
                    return answer
                return question

            _result, error, events, _written, _forget = self._execute(self.plan(), restart=ask)
            self.assertIsNone(error)
            self.assertEqual(events.count("ASK"), 1)
            self.assertLess(next(i for i, e in enumerate(events) if e.startswith("qm cloudinit update")),
                            events.index("ASK"))
            self.assertEqual("shutdown" in events, answer)
            if answer:
                self.assertLess(events.index("ASK"), events.index("shutdown"))

    def test_stopped_vm_is_not_started_and_known_hosts_stay(self):
        result, error, events, _written, forget = self._execute(self.plan(status="stopped"))
        self.assertIsNone(error)
        self.assertEqual((result["restarted"], result["verified"], result["known_hosts_removed"]), (False, False, []))
        self.assertNotIn("shutdown", events)
        self.assertFalse([event for event in events if event.startswith("qm start")])
        forget.assert_not_called()

    def test_failure_before_the_restart_puts_the_snippets_back(self):
        result, error, events, written, _forget = self._execute(self.plan(), fail_on="qm cloudinit update")
        self.assertEqual(error, "boom")
        self.assertEqual(written["homestack-cpu-vendor.yaml"][-1], VENDOR)
        self.assertEqual(written["homestack-cpu-meta.yaml"][-1], "instance-id: cpu-206-old\nlocal-hostname: cpu\n")
        self.assertNotIn("shutdown", events)

    def test_cloud_init_error_and_missing_key_are_reported(self):
        _result, error, *_ = self._execute(self.plan(), cloud_init="status: error")
        self.assertRegex(error, "did not report 'done'")
        _result, error, *_ = self._execute(
            self.plan(), counts=f"MISSING /home/user/.ssh/authorized_keys {TREZOR.split()[1]}")
        self.assertRegex(error, "Not all keys are in root and user authorized_keys of VM 206: trezor-vm in /home/user")

    def test_without_a_key_the_existing_sshkeys_are_applied(self):
        data = info()
        data["vm_config"]["sshkeys"] = quote(YUBIKEY + "\n" + TREZOR + "\n", safe="")
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data):
            plan = addkey.build_addkey_plan(object(), self.cfg, 206)
        self.assertIsNone(plan["key"])
        self.assertIsNone(plan["private_key"])
        self.assertEqual((plan["label"], plan["keys"]), ("yubikey1, trezor-vm", [YUBIKEY, TREZOR]))
        self.assertEqual(len(plan["steps"]), 7)
        self.assertIn("Keep the Proxmox sshkeys as they are", plan["steps"][0])
        result, error, events, written, _forget = self._execute(plan)
        self.assertIsNone(error)
        self.assertFalse([event for event in events if "--sshkeys" in event])
        self.assertNotIn("local", result)
        self.assertEqual(result["key"], "yubikey1, trezor-vm")
        self.assertIn(TREZOR.split()[1], written["homestack-cpu-vendor.yaml"][-1])
        checks = [event for event in events if event.startswith("guest")]
        self.assertTrue(checks)

    def test_without_a_key_and_without_sshkeys_is_refused(self):
        data = info()
        data["vm_config"]["sshkeys"] = ""
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data), \
                self.assertRaisesRegex(AppError, "no keys in its Proxmox sshkeys"):
            addkey.build_addkey_plan(object(), self.cfg, 206)

    def test_key_already_in_sshkeys_skips_qm_set(self):
        data = info()
        data["vm_config"]["sshkeys"] = quote(YUBIKEY + "\n" + TREZOR + "\n", safe="")
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data):
            plan = addkey.build_addkey_plan(object(), self.cfg, 206, self.key)
        self.assertTrue(plan["key_already_in_sshkeys"])
        _result, error, events, _written, _forget = self._execute(plan)
        self.assertIsNone(error)
        self.assertFalse([event for event in events if "--sshkeys" in event])


class LocalTests(unittest.TestCase):
    def cfg(self, path, files=("~/.ssh/id_ed25519_sk_yubikey1",)):
        base = test_config()
        return replace(base, path=Path(path), workspace_ssh=config.WorkspaceSSHConfig(
            "user", tuple(files), True, "FATAL", ("yubikey", "trezor")))

    def test_private_key_must_exist_next_to_the_public_one(self):
        with tempfile.TemporaryDirectory() as directory:
            pub = key_file(directory, TREZOR, "id_x.pub")
            with self.assertRaisesRegex(AppError, "was not found"):
                addkey.private_key_for(pub)
            (Path(directory) / "id_x").write_text("k", encoding="utf-8")
            self.assertEqual(addkey.private_key_for(pub)[0], Path(directory) / "id_x")

    def test_identity_is_appended_once_and_the_rest_of_the_config_is_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            text = ('# my notes\n[node]\nname = "n"\n\n[workspace_ssh]\nuser = "user"\n'
                    'identity_files = ["~/.ssh/a", "~/.ssh/b"]\nlog_level = "FATAL"\n\n[repo]\nowner = "o"\n')
            path.write_text(text, encoding="utf-8")
            path.chmod(0o644)
            self.assertTrue(addkey.register_identity(path, "~/.ssh/c"))
            self.assertEqual(path.read_text(encoding="utf-8"),
                             text.replace('["~/.ssh/a", "~/.ssh/b"]', '["~/.ssh/a", "~/.ssh/b", "~/.ssh/c"]'))
            self.assertEqual(path.stat().st_mode & 0o777, 0o644)
            self.assertFalse(addkey.register_identity(path, "~/.ssh/c"))
            self.assertFalse(addkey.register_identity(path, "~/.ssh/a"))

    def test_identity_in_another_section_or_multiline_array_is_not_touched(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            for text in ('[workspace_ssh]\nuser = "u"\n[other]\nidentity_files = ["x"]\n',
                         '[workspace_ssh]\nidentity_files = [\n  "x",\n]\n', '[node]\nname = "n"\n'):
                path.write_text(text, encoding="utf-8")
                with self.assertRaisesRegex(AppError, "one-line array"):
                    addkey.register_identity(path, "~/.ssh/c")
                self.assertEqual(path.read_text(encoding="utf-8"), text)

    def test_ssh_config_lists_plain_keys_unchanged_and_picks_by_connected_key_otherwise(self):
        plain = workspace_ssh.local_ssh_lines(self.cfg("/x", ("~/.ssh/k1", "~/.ssh/k2")), 5, "n", "192.0.2.5")
        self.assertEqual(plain, ["Host n vm5", "    HostName 192.0.2.5", "    User user", "    IdentityFile ~/.ssh/k1",
                                 "    IdentityFile ~/.ssh/k2", "    IdentitiesOnly yes", "    LogLevel FATAL"])
        mixed = workspace_ssh.local_ssh_lines(self.cfg("/x", (
            "~/.ssh/id_ed25519_sk_yubikey1", "~/.ssh/id_ed25519_sk_yubikey2", "~/.ssh/id_ecdsa_sk_trezor_vm")),
            200, "test1", "192.0.2.200")
        text = "\n".join(mixed)
        self.assertIn('Match originalhost test1,vm200 exec "fido2-token -L 2>/dev/null | grep -qE \'vendor=0x1050\'"', text)
        self.assertIn('Match originalhost test1,vm200 exec "! fido2-token -L 2>/dev/null | grep -qE \'vendor=0x1050\'"', text)
        self.assertLess(text.index("yubikey1"), text.index("id_ecdsa_sk_trezor_vm"))
        self.assertEqual(text.count("IdentityFile"), 3)

    def test_sync_updates_the_config_and_writes_this_vms_ssh_file(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / ".config").mkdir()
            path = home / ".config" / "config.toml"
            path.write_text('[workspace_ssh]\nidentity_files = ["~/.ssh/id_ed25519_sk_yubikey1"]\n', encoding="utf-8")
            plan = {"private_key": "~/.ssh/id_ecdsa_sk_trezor_vm", "vmid": 206, "name": "cpu", "ip": "192.0.2.206"}
            with mock.patch.object(workspace_ssh.Path, "home", return_value=home):
                result = addkey.sync_local_ssh(self.cfg(path), plan)
            self.assertTrue(result["identity_files_changed"])
            self.assertIn("id_ecdsa_sk_trezor_vm", path.read_text(encoding="utf-8"))
            written = (home / ".ssh/config.d/homestack/vm206-cpu.conf").read_text(encoding="utf-8")
            self.assertIn("IdentityFile ~/.ssh/id_ecdsa_sk_trezor_vm", written)
            self.assertIn("Match originalhost cpu,vm206", written)

    def test_local_failure_is_reported_without_failing_the_key_addition(self):
        flow = FlowTests("test_running_vm_gets_the_key_restarts_and_is_verified")
        flow.setUp()
        self.addCleanup(flow.directory.cleanup)
        plan = flow.plan()
        with mock.patch.object(addkey, "node_run", lambda *a, **k: SimpleNamespace(output=VENDOR)), \
                mock.patch.object(addkey, "remote_write_text", lambda *a, **k: None), \
                mock.patch.object(addkey, "shutdown_vm_on_node", lambda *a, **k: None), \
                mock.patch.object(addkey, "wait_for_qga_on_node", lambda *a, **k: None), \
                mock.patch.object(addkey, "guest_out_on_node", lambda _s, _c, _n, _v, command, **k: "status: done"
                                  if "cloud-init" in command else "a:1\nb:1"), \
                mock.patch.object(addkey, "forget_local_ssh_host", return_value=[]), \
                mock.patch.object(addkey, "sync_local_ssh", side_effect=AppError("not a one-line array")):
            result = addkey.add_key(object(), flow.cfg, plan)
        self.assertTrue(result["ok"])
        self.assertEqual(result["local_error"], "not a one-line array")
        self.assertNotIn("local", result)


if __name__ == "__main__":
    unittest.main()
