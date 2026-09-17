from __future__ import annotations

from contextlib import ExitStack
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from homestack import resize, resize_auth
from homestack.models import AppError, RemoteResult
from support import test_config


class ResizeLuksTests(unittest.TestCase):
    def setUp(self) -> None:
        cfg = test_config()
        self.cfg = cfg
        self.plan = {
            "vmid": 207,
            "name": "gpu",
            "node": "pve3",
            "role": "root",
            "disk": cfg.root_disk,
            "volume": "local-lvm:root",
            "current_size_gib": 16.0,
            "size": "32G",
            "target_size_gib": 32.0,
            "digest": "a" * 40,
            "ip": "192.0.2.207",
            "guest": {
                "parent": "/dev/sda",
                "device": "/dev/vg/root",
                "partition": 1,
                "size_bytes": 16 * 1024**3,
                "filesystem": "ext4",
                "mount": "/",
                "btrfs_devid": None,
                "crypt": {
                    "name": "cryptroot",
                    "device": "/dev/sda1",
                    "key_file": "/run/homestack/root.key",
                    "auth": "keyfile",
                },
                "lvm": {
                    "lv_path": "/dev/vg/root",
                    "pv_path": "/dev/mapper/cryptroot",
                    "vg_name": "vg",
                },
            },
        }

    def _common_patches(self, plan: dict, events: list[tuple[str, object]], *, qga_result=None):
        stack = ExitStack()
        stack.enter_context(patch.object(resize, "build_resize_plan", return_value=plan))

        def run_node(_session, _cfg, _node, command, **kwargs):
            events.append(("node", (command, kwargs)))
            return RemoteResult(0, "")

        def run_guest(_session, _cfg, _node, _vmid, command, **kwargs):
            events.append(("guest", (command, kwargs)))
            return ""

        node = stack.enter_context(patch.object(resize, "node_run", side_effect=run_node))
        guest = stack.enter_context(patch.object(resize, "guest_out_on_node", side_effect=run_guest))
        config = stack.enter_context(
            patch.object(
                resize,
                "qm_config_on_node",
                return_value={self.cfg.root_disk: f"local-lvm:root,size={plan['size']}"},
            )
        )

        def run_qga(_session, _cfg, _node, _vmid, command, **kwargs):
            events.append(("qga", (command, kwargs)))
            return {"exited": 1, "exitcode": 0, "out-data": "", "err-data": ""} if qga_result is None else qga_result

        qga = stack.enter_context(patch.object(resize_auth, "guest_exec_on_node", side_effect=run_qga))
        return stack, node, guest, config, qga

    def test_keyfile_is_validated_in_guest_before_host_disk_growth(self) -> None:
        events: list[tuple[str, object]] = []
        stack, node, guest, config, qga = self._common_patches(self.plan, events)
        ssh_factory = Mock(side_effect=AssertionError("keyfile authentication must stay in the guest"))
        getpass = Mock(side_effect=AssertionError("keyfile authentication must not prompt"))
        with stack, patch.object(resize_auth.WorkspaceSSH, "configured", ssh_factory), patch.object(
            resize_auth.getpass, "getpass", getpass
        ):
            result = resize.resize_workspace(object(), self.cfg, self.plan)

        self.assertTrue(result["ok"])
        self.assertEqual(qga.call_count, 2)
        validation = qga.call_args_list[0].args[4]
        growth = qga.call_args_list[1].args[4]
        self.assertIn("cryptsetup", validation)
        self.assertIn("--test-passphrase", validation)
        self.assertIn("--key-file /run/homestack/root.key", validation)
        self.assertIn("/dev/sda1", validation)
        self.assertIn("cryptsetup", growth)
        self.assertIn("resize cryptroot", growth)
        self.assertLess(next(i for i, event in enumerate(events) if event[0] == "qga"),
                        next(i for i, event in enumerate(events) if event[0] == "node"))
        self.assertEqual(node.call_count, 1)
        self.assertEqual(config.call_count, 1)
        self.assertEqual(guest.call_count, 2)
        filesystem_script = guest.call_args_list[-1].args[4]
        self.assertIn("pvresize /dev/mapper/cryptroot", filesystem_script)
        self.assertIn("vgs --noheadings -o vg_free_count vg", filesystem_script)
        self.assertIn("lvextend --extents +100%FREE /dev/vg/root", filesystem_script)
        self.assertIn("resize2fs /dev/vg/root", filesystem_script)

    def test_wrong_keyfile_fails_before_any_host_or_guest_growth_mutation(self) -> None:
        events: list[tuple[str, object]] = []
        wrong_key = {"exited": 1, "exitcode": 1, "out-data": "", "err-data": "wrong key"}
        stack, node, guest, config, qga = self._common_patches(
            self.plan, events, qga_result=wrong_key
        )
        with stack, self.assertRaisesRegex(AppError, "LUKS authentication or resize failed"):
            resize.resize_workspace(object(), self.cfg, self.plan)

        qga.assert_called_once()
        node.assert_not_called()
        guest.assert_not_called()
        config.assert_not_called()
        self.assertEqual([kind for kind, _ in events], ["qga"])

    def test_encrypted_equal_size_retry_revalidates_and_grows_guest_layers(self) -> None:
        plan = {**self.plan, "size": "16G", "target_size_gib": 16.0}
        events: list[tuple[str, object]] = []
        stack, node, guest, _config, qga = self._common_patches(plan, events)
        with stack:
            result = resize.resize_workspace(object(), self.cfg, plan)

        self.assertTrue(result["ok"])
        node.assert_not_called()
        self.assertEqual(qga.call_count, 2)
        self.assertEqual(guest.call_count, 2)
        self.assertIn("cryptsetup", qga.call_args_list[-1].args[4])
        self.assertIn("resize cryptroot", qga.call_args_list[-1].args[4])

    def test_passphrase_is_sent_as_workspace_ssh_input_after_guest_identity_check(self) -> None:
        plan = {
            **self.plan,
            "guest": {
                **self.plan["guest"],
                "crypt": {
                    **self.plan["guest"]["crypt"],
                    "key_file": None,
                    "auth": "passphrase",
                },
            },
        }
        events: list[tuple[str, object]] = []

        class FakeSSH:
            def __enter__(self):
                events.append(("ssh-enter", None))
                return self

            def __exit__(self, *_args):
                events.append(("ssh-exit", None))

            def run(self, command, **kwargs):
                events.append(("ssh", (command, kwargs)))
                if command == "cat /proc/sys/kernel/random/boot_id":
                    return SimpleNamespace(returncode=0, stdout="boot-207\n")
                return SimpleNamespace(returncode=0, stdout="", stderr="")

        ssh = FakeSSH()
        stack, node, guest, _config, qga = self._common_patches(plan, events)

        def boot_id_guest(_session, _cfg, _node, _vmid, command, **kwargs):
            events.append(("guest", (command, kwargs)))
            return "boot-207" if command == "cat /proc/sys/kernel/random/boot_id" else ""

        guest.side_effect = boot_id_guest
        stack.enter_context(patch.object(resize_auth, "guest_out_on_node", side_effect=boot_id_guest))
        with (
            stack,
            patch.object(resize_auth.WorkspaceSSH, "configured", return_value=ssh) as configured,
            patch.object(resize_auth.sys.stdin, "isatty", return_value=True),
            patch.object(resize_auth.getpass, "getpass", return_value="correct horse battery staple"),
        ):
            result = resize.resize_workspace(object(), self.cfg, plan, interactive=True)

        self.assertTrue(result["ok"])
        qga.assert_not_called()
        configured.assert_called_once()
        configured_cfg, configured_target = configured.call_args.args
        self.assertEqual(configured_cfg.user_name, "root")
        self.assertEqual(configured_target, {"vmid": 207, "ip": "192.0.2.207"})
        ssh_calls = [payload for kind, payload in events if kind == "ssh"]
        self.assertEqual(len(ssh_calls), 3)
        boot_call, test_call, grow_call = ssh_calls
        self.assertEqual(boot_call[0], "cat /proc/sys/kernel/random/boot_id")
        self.assertEqual(test_call[1]["input_text"], "correct horse battery staple")
        self.assertEqual(grow_call[1]["input_text"], "correct horse battery staple")
        self.assertFalse(test_call[1]["check"])
        self.assertFalse(grow_call[1]["check"])
        for command, _kwargs in ssh_calls:
            self.assertNotIn("correct horse battery staple", command)
        self.assertIn("--test-passphrase", test_call[0])
        self.assertIn("resize cryptroot", grow_call[0])
        self.assertLess(
            next(i for i, event in enumerate(events) if event == ("ssh", test_call)),
            next(i for i, event in enumerate(events) if event[0] == "node"),
        )
        self.assertEqual(node.call_count, 1)
        self.assertEqual(guest.call_count, 2)

    def test_wrong_passphrase_fails_before_host_disk_resize(self) -> None:
        plan = {
            **self.plan,
            "guest": {
                **self.plan["guest"],
                "crypt": {
                    **self.plan["guest"]["crypt"],
                    "key_file": None,
                    "auth": "passphrase",
                },
            },
        }
        events: list[tuple[str, object]] = []

        class FakeSSH:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return None

            def run(self, command, **kwargs):
                events.append(("ssh", (command, kwargs)))
                if command == "cat /proc/sys/kernel/random/boot_id":
                    return SimpleNamespace(returncode=0, stdout="boot-207\n")
                return SimpleNamespace(returncode=1, stdout="", stderr="")

        stack, node, guest, config, qga = self._common_patches(plan, events)
        guest.side_effect = lambda _session, _cfg, _node, _vmid, command, **kwargs: "boot-207"
        stack.enter_context(
            patch.object(
                resize_auth,
                "guest_out_on_node",
                side_effect=lambda _session, _cfg, _node, _vmid, command, **kwargs: "boot-207",
            )
        )
        with (
            stack,
            patch.object(resize_auth.WorkspaceSSH, "configured", return_value=FakeSSH()),
            patch.object(resize_auth.sys.stdin, "isatty", return_value=True),
            patch.object(resize_auth.getpass, "getpass", return_value="wrong passphrase"),
            self.assertRaisesRegex(AppError, "passphrase verification failed"),
        ):
            resize.resize_workspace(object(), self.cfg, plan, interactive=True)

        qga.assert_not_called()
        node.assert_not_called()
        config.assert_not_called()
        guest.assert_not_called()
        self.assertEqual(len([event for event in events if event[0] == "ssh"]), 2)

    def test_json_or_non_tty_passphrase_refuses_before_prompt_or_mutation(self) -> None:
        for interactive, tty in ((False, True), (True, False)):
            with self.subTest(interactive=interactive, tty=tty):
                plan = {
                    **self.plan,
                    "guest": {
                        **self.plan["guest"],
                        "crypt": {
                            **self.plan["guest"]["crypt"],
                            "key_file": None,
                            "auth": "passphrase",
                        },
                    },
                }
                events: list[tuple[str, object]] = []
                stack, node, guest, config, qga = self._common_patches(plan, events)
                ssh_factory = Mock(side_effect=AssertionError("must not open SSH"))
                getpass = Mock(side_effect=AssertionError("must not prompt"))
                with (
                    stack,
                    patch.object(resize_auth.WorkspaceSSH, "configured", ssh_factory),
                    patch.object(resize_auth.sys.stdin, "isatty", return_value=tty),
                    patch.object(resize_auth.getpass, "getpass", getpass),
                    self.assertRaisesRegex(AppError, "requires a passphrase"),
                ):
                    resize.resize_workspace(object(), self.cfg, plan, interactive=interactive)
                qga.assert_not_called()
                node.assert_not_called()
                guest.assert_not_called()
                config.assert_not_called()
                ssh_factory.assert_not_called()
                getpass.assert_not_called()


@unittest.skipUnless(shutil.which("cryptsetup"), "cryptsetup is not installed")
class CryptsetupTempImageTests(unittest.TestCase):
    def test_luks2_keyfile_test_passphrase_uses_only_a_temp_regular_file(self) -> None:
        with tempfile.TemporaryDirectory(prefix="homestack-luks-test-") as directory:
            image = Path(directory) / "image"
            key = Path(directory) / "key"
            image.write_bytes(b"\0" * (32 * 1024 * 1024))
            key.write_text("test-only-key-material\n", encoding="utf-8")
            format_result = subprocess.run(
                ["cryptsetup", "luksFormat", "--batch-mode", "--type", "luks2", str(image), str(key)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.assertEqual(format_result.returncode, 0, format_result.stderr)

            valid = subprocess.run(
                [
                    "cryptsetup",
                    "open",
                    "--batch-mode",
                    "--type",
                    "luks",
                    "--test-passphrase",
                    "--key-file",
                    str(key),
                    str(image),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.assertEqual(valid.returncode, 0, valid.stderr)

            wrong_key = Path(directory) / "wrong-key"
            wrong_key.write_text("wrong-key-material\n", encoding="utf-8")
            invalid = subprocess.run(
                [
                    "cryptsetup",
                    "open",
                    "--batch-mode",
                    "--type",
                    "luks",
                    "--test-passphrase",
                    "--key-file",
                    str(wrong_key),
                    str(image),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.assertNotEqual(invalid.returncode, 0)


if __name__ == "__main__":
    unittest.main()
