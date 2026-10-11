import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock
from urllib.parse import quote

from homestack import addkey
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
        self.assertIn("/root/.ssh/authorized_keys /home/user/.ssh/authorized_keys", script)
        self.assertNotIn("rm ", script)
        self.assertNotIn(" > \"$f\"", script)

    def test_ensure_step_appends_only_missing_keys(self):
        step = addkey.ensure_keys_item([YUBIKEY, TREZOR], "user")
        script = "".join(line[6:] for line in step.splitlines(keepends=True)[1:])
        with tempfile.TemporaryDirectory() as directory:
            ssh = Path(directory) / "ssh"
            ssh.mkdir()
            target = ssh / "authorized_keys"
            target.write_text("other-key AAAA other\n" + YUBIKEY, encoding="utf-8")  # no trailing newline
            patched = script.replace("/root/.ssh/authorized_keys /home/user/.ssh/authorized_keys", str(target))
            for _ in range(2):
                run = subprocess.run(["bash", "-c", patched], text=True, capture_output=True)
                self.assertEqual(run.returncode, 0, run.stderr)
            lines = target.read_text(encoding="utf-8").splitlines()
            self.assertEqual(lines, ["other-key AAAA other", YUBIKEY, TREZOR])

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

    def plan(self, **overrides):
        data = info(**overrides)
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data):
            return addkey.build_addkey_plan(object(), self.cfg, 206, self.key)

    def test_plan_names_the_key_and_every_step(self):
        plan = self.plan()
        self.assertEqual((plan["name"], plan["ip"], plan["key"]["comment"]), ("cpu", "192.0.2.206", "trezor-vm"))
        self.assertEqual(plan["keys"], [YUBIKEY, TREZOR])
        self.assertFalse(plan["key_already_in_sshkeys"])
        self.assertEqual(len(plan["steps"]), 7)
        self.assertIn("allow_format=0", " ".join(plan["steps"]))
        self.assertIn("stays stopped", " ".join(self.plan(status="stopped")["steps"]))

    def test_vm_without_homestack_snippets_is_refused(self):
        data = info()
        data["vm_config"]["cicustom"] = "user=local:snippets/other.yaml"
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data), \
                self.assertRaisesRegex(AppError, "does not use the HomeStack vendor snippet"):
            addkey.build_addkey_plan(object(), self.cfg, 206, self.key)

    def _execute(self, plan, *, cloud_init="status: done", counts="/root/.ssh/authorized_keys:1\n/home/user/.ssh/authorized_keys:1",
                 fail_on=None):
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
                mock.patch.object(addkey, "forget_local_ssh_host", return_value=["192.0.2.206"]) as forget:
            try:
                result, error = addkey.add_key(object(), self.cfg, plan), None
            except AppError as exc:
                result, error = None, str(exc)
        return result, error, events, written, forget

    def test_running_vm_gets_the_key_restarts_and_is_verified(self):
        result, error, events, written, forget = self._execute(self.plan())
        self.assertIsNone(error)
        self.assertEqual(result, {"ok": True, "command": "addkey", "vmid": 206, "name": "cpu", "key": "trezor-vm",
                                  "restarted": True, "known_hosts_removed": ["192.0.2.206"], "verified": True})
        order = [next(i for i, e in enumerate(events) if marker in e)
                 for marker in ("qm set 206 --sshkeys", "write homestack-cpu-vendor.yaml", "write homestack-cpu-meta.yaml",
                                "qm cloudinit update 206", "shutdown", "qm start 206", "qga", "guest timeout", "guest grep")]
        self.assertEqual(order, sorted(order))
        self.assertIn("allow_format=0", written["homestack-cpu-vendor.yaml"][-1])
        self.assertNotIn("old-key-line", written["homestack-cpu-vendor.yaml"][-1])
        self.assertNotEqual(written["homestack-cpu-meta.yaml"][-1], "instance-id: cpu-206-old\nlocal-hostname: cpu\n")
        self.assertRegex(written["homestack-cpu-meta.yaml"][-1], r"^instance-id: cpu-206-[0-9a-f]{12}\nlocal-hostname: cpu\n$")
        forget.assert_called_once_with("192.0.2.206")

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
        _result, error, *_ = self._execute(self.plan(), counts="/root/.ssh/authorized_keys:1\n/home/user/.ssh/authorized_keys:0")
        self.assertRegex(error, "not in both authorized_keys")

    def test_key_already_in_sshkeys_skips_qm_set(self):
        data = info()
        data["vm_config"]["sshkeys"] = quote(YUBIKEY + "\n" + TREZOR + "\n", safe="")
        with mock.patch.object(addkey, "resolve_existing_workspace", return_value=data):
            plan = addkey.build_addkey_plan(object(), self.cfg, 206, self.key)
        self.assertTrue(plan["key_already_in_sshkeys"])
        _result, error, events, _written, _forget = self._execute(plan)
        self.assertIsNone(error)
        self.assertFalse([event for event in events if "--sshkeys" in event])


if __name__ == "__main__":
    unittest.main()
