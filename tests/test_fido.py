import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from homestack import config as cfgmod, fido, touch, workspace_ssh
from homestack.models import AppError

LISTING = (
    "/dev/hidraw8: vendor=0x1050, product=0x0407 (Yubico YubiKey OTP+FIDO+CCID)\n"
    "/dev/hidraw9: vendor=0x1209, product=0x53c1 (SatoshiLabs TREZOR)\n"
    "/dev/hidraw11: vendor=0x1050, product=0x0407 (Yubico YubiKey OTP+FIDO+CCID)\n"
)


class SelectionTests(unittest.TestCase):
    def test_listing_is_parsed_with_normalized_ids(self):
        devices = fido.parse_listing(LISTING + "/dev/hidraw3: vendor=0x20a0, product=0x42b2 (Nitrokey)\n")
        self.assertEqual([(d.path, d.vendor, d.product) for d in devices][:3],
                         [("/dev/hidraw8", "1050", "0407"), ("/dev/hidraw9", "1209", "53c1"),
                          ("/dev/hidraw11", "1050", "0407")])
        self.assertEqual(devices[3].vendor, "20a0")
        self.assertEqual(fido.parse_listing(""), [])

    def test_preference_order_wins_and_all_devices_of_a_kind_are_kept(self):
        devices = fido.parse_listing(LISTING)
        entry, found = fido.select(("yubikey", "trezor"), devices)
        self.assertEqual((entry, [d.path for d in found]), ("yubikey", ["/dev/hidraw8", "/dev/hidraw11"]))
        entry, found = fido.select(("trezor", "yubikey"), devices)
        self.assertEqual((entry, [d.path for d in found]), ("trezor", ["/dev/hidraw9"]))
        # the preferred kind is absent: fall back to the next one
        entry, found = fido.select(("yubikey", "trezor"), [d for d in devices if d.vendor == "1209"])
        self.assertEqual(entry, "trezor")
        self.assertEqual(fido.select(("1050:0407",), devices)[0], "1050:0407")
        self.assertEqual([d.path for d in fido.select(("1209",), devices)[1]], ["/dev/hidraw9"])

    def test_nothing_connected_gives_a_plain_message(self):
        with self.assertRaisesRegex(AppError, r"No configured security key is connected \(YubiKey, Trezor\)"):
            fido.select(("yubikey", "trezor"), [])

    def test_entries_are_validated_and_normalized(self):
        self.assertEqual(fido.validate_entry(" YubiKey "), "yubikey")
        self.assertEqual(fido.validate_entry("1050:0407"), "1050:0407")
        self.assertEqual(fido.validate_entry("1209"), "1209")
        for bad in ("usb", "12345", "1050:", 5, ""):
            with self.subTest(bad=bad), self.assertRaises(AppError):
                fido.validate_entry(bad)

    def test_sandbox_exposes_only_the_selected_devices(self):
        devices = fido.parse_listing(LISTING)
        with mock.patch.object(fido.shutil, "which", return_value="/usr/bin/bwrap"):
            prefix = fido.sandbox_prefix([d for d in devices if d.vendor == "1050"])
        self.assertEqual(prefix[:6], ["/usr/bin/bwrap", "--bind", "/", "/", "--dev", "/dev"])
        binds = [prefix[i + 1] for i, part in enumerate(prefix) if part == "--dev-bind"]
        self.assertEqual(binds, ["/dev/hidraw8", "/dev/hidraw11"])
        self.assertNotIn("/dev/hidraw9", prefix)
        with mock.patch.object(fido.shutil, "which", return_value=None), self.assertRaisesRegex(AppError, "bwrap"):
            fido.sandbox_prefix(devices)

    def test_advice_only_when_several_kinds_are_attached(self):
        devices = fido.parse_listing(LISTING)
        self.assertIn("Trezor, YubiKey", fido.advice(devices))
        self.assertIsNone(fido.advice([d for d in devices if d.vendor == "1050"]))
        self.assertIsNone(fido.advice([]))

    def test_listing_uses_fido2_token(self):
        done = subprocess.CompletedProcess([], 0, stdout=LISTING)
        with mock.patch.object(fido.shutil, "which", return_value="/usr/bin/fido2-token"):
            self.assertEqual(len(fido.list_devices(run=lambda *a, **k: done)), 3)
        with mock.patch.object(fido.shutil, "which", return_value=None), self.assertRaisesRegex(AppError, "fido2-token"):
            fido.list_devices()


class ConfigTests(unittest.TestCase):
    def test_fido_devices_are_parsed_deduplicated_and_round_trip(self):
        from support import EXAMPLE_CONFIG_PATH
        text = EXAMPLE_CONFIG_PATH.read_text(encoding="utf-8")
        marker = "log_level ="
        index = text.index(marker, text.index("[workspace_ssh]"))
        end = text.index("\n", index) + 1
        patched = text[:end] + 'fido_devices = ["YubiKey", "trezor", "yubikey"]\n' + text[end:]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            path.write_text(patched, encoding="utf-8")
            loaded = cfgmod.load_config(path)
        self.assertEqual(loaded.workspace_ssh.fido_devices, ("yubikey", "trezor"))
        again = cfgmod.validate_config_text(cfgmod.config_to_toml(loaded))
        self.assertEqual(again.workspace_ssh.fido_devices, ("yubikey", "trezor"))
        self.assertNotIn("fido_devices", cfgmod.config_to_toml(cfgmod.load_config(EXAMPLE_CONFIG_PATH)))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            path.write_text(text[:end] + 'fido_devices = "yubikey"\n' + text[end:], encoding="utf-8")
            with self.assertRaises(AppError):
                cfgmod.load_config(path)


class MasterTests(unittest.TestCase):
    def _enter(self, ws, *, stderr_text="", returncode=255, clock=(0.0, 0.1)):
        calls = []

        def run(argv, **kwargs):
            calls.append(argv)
            if stderr_text:
                kwargs["stderr"].write(stderr_text)
            return subprocess.CompletedProcess(argv, returncode)

        with mock.patch("subprocess.run", side_effect=run), \
                mock.patch.object(workspace_ssh.time, "monotonic", side_effect=list(clock)), \
                mock.patch.object(touch, "play", lambda *a, **k: None):
            try:
                ws.__enter__()
                error = None
            except AppError as exc:
                error = str(exc)
        return calls, error

    def test_master_runs_in_a_sandbox_that_shows_only_the_preferred_key(self):
        devices = fido.parse_listing(LISTING)
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, fido_devices=("yubikey", "trezor"))
        with mock.patch.object(fido, "list_devices", return_value=devices), \
                mock.patch.object(fido.shutil, "which", return_value="/usr/bin/bwrap"), \
                mock.patch.object(workspace_ssh, "run_local", return_value=subprocess.CompletedProcess([], 0)):
            calls, error = self._enter(ws, returncode=0)
            ws.opened = False
            ws.directory.cleanup()
        self.assertIsNone(error)
        argv = calls[0]
        self.assertEqual(argv[0], "/usr/bin/bwrap")
        self.assertIn("/dev/hidraw8", argv)
        self.assertNotIn("/dev/hidraw9", argv)
        self.assertIn("ssh", argv)
        self.assertLess(argv.index("--dev-bind"), argv.index("ssh"))
        # the system-wide ssh config is unreadable in the sandbox (root files look like "nobody")
        self.assertEqual(argv[argv.index("ssh") + 1], "-F")

    def test_sandboxed_master_does_not_hand_signing_to_an_agent(self):
        devices = fido.parse_listing(LISTING)
        for options, expected in (((), 1), (("-o", "IdentityAgent=/tmp/own.sock"), 0)):
            ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, options=options, fido_devices=("yubikey",))
            with mock.patch.object(fido, "list_devices", return_value=devices), \
                    mock.patch.object(fido.shutil, "which", return_value="/usr/bin/bwrap"), \
                    mock.patch.object(workspace_ssh, "run_local", return_value=subprocess.CompletedProcess([], 0)):
                calls, error = self._enter(ws, returncode=0)
                ws.opened = False
                ws.directory.cleanup()
            self.assertIsNone(error)
            self.assertEqual(calls[0].count("IdentityAgent=none"), expected)

    def test_unsandboxed_master_keeps_the_agent(self):
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1)
        with mock.patch.object(workspace_ssh, "run_local", return_value=subprocess.CompletedProcess([], 0)):
            calls, error = self._enter(ws, returncode=0)
            ws.opened = False
            ws.directory.cleanup()
        self.assertIsNone(error)
        self.assertNotIn("IdentityAgent=none", calls[0])

    def test_sandboxed_master_reads_the_users_config_or_none(self):
        devices = fido.parse_listing(LISTING)
        with tempfile.TemporaryDirectory() as home:
            (Path(home) / ".ssh").mkdir()
            (Path(home) / ".ssh" / "config").write_text("Host x\n")
            with mock.patch.object(workspace_ssh.Path, "home", return_value=Path(home)):
                self.assertEqual(workspace_ssh.WorkspaceSSH._user_ssh_config(), str(Path(home) / ".ssh" / "config"))
            (Path(home) / ".ssh" / "config").unlink()
            with mock.patch.object(workspace_ssh.Path, "home", return_value=Path(home)):
                self.assertEqual(workspace_ssh.WorkspaceSSH._user_ssh_config(), "/dev/null")
        # an explicit -F in the options is never overridden
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, options=("-F", "/tmp/own"), fido_devices=("yubikey",))
        with mock.patch.object(fido, "list_devices", return_value=devices), \
                mock.patch.object(fido.shutil, "which", return_value="/usr/bin/bwrap"), \
                mock.patch.object(workspace_ssh, "run_local", return_value=subprocess.CompletedProcess([], 0)):
            calls, error = self._enter(ws, returncode=0)
            ws.opened = False
            ws.directory.cleanup()
        self.assertEqual(calls[0].count("-F"), 1)
        self.assertIn("/tmp/own", calls[0])

    def test_no_connected_key_stops_before_ssh_with_a_plain_message(self):
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1, fido_devices=("yubikey",))
        with mock.patch.object(fido, "list_devices", return_value=[]), mock.patch("subprocess.run") as run:
            with self.assertRaisesRegex(AppError, "No configured security key is connected"):
                ws.__enter__()
        run.assert_not_called()

    def test_connected_key_unknown_to_the_host_stops_before_ssh_asks_the_device(self):
        trezor_only = [d for d in fido.parse_listing(LISTING) if d.vendor == "1209"]
        listing = "host gpu\nidentitiesonly yes\n" + "".join(
            f"identityfile ~/.ssh/id_ed25519_sk_yubikey{n}\n" for n in (1, 2, 3))
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.7", 1, fido_devices=("yubikey", "trezor"),
                                        name="gpu", address="192.0.2.7")
        with mock.patch.object(fido, "list_devices", return_value=trezor_only), \
                mock.patch.object(workspace_ssh, "run_local",
                                  return_value=subprocess.CompletedProcess([], 0, stdout=listing)), \
                mock.patch("subprocess.run") as run:
            with self.assertRaises(AppError) as caught:
                ws.__enter__()
        run.assert_not_called()
        # the host is named the way the user knows it, with the address only as information
        self.assertEqual(str(caught.exception).splitlines(), [
            "gpu (192.0.2.7) has no Trezor key configured.",
            "Only YubiKey keys are set up there.",
            "Connect a YubiKey or add a Trezor key to the host."])
        self.assertNotIn("user@", str(caught.exception))

    def test_host_key_check_passes_when_the_kind_is_listed_or_names_are_unknown(self):
        listed = ["~/.ssh/id_ed25519_sk_yubikey1", "~/.ssh/id_ecdsa_sk_trezor_pve"]
        self.assertIsNone(fido.missing_key_message("trezor", listed, "pve1"))
        self.assertIsNone(fido.missing_key_message("yubikey", listed, "pve1"))
        self.assertIsNone(fido.missing_key_message("trezor", ["~/.ssh/id_work"], "x"))
        self.assertIsNone(fido.missing_key_message("trezor", [], "x"))
        self.assertIsNone(fido.missing_key_message("1050:0407", listed[:1], "x"))
        self.assertIn("Connect a YubiKey", fido.missing_key_message("trezor", listed[:1], "gpu"))

    def test_failures_are_reported_as_short_messages_not_raw_ssh_output(self):
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1)
        _, error = self._enter(ws, stderr_text='sign_and_send_pubkey: signing failed for ECDSA-SK "k": device not found\n'
                               "user@192.0.2.1: Permission denied (publickey).\n", clock=(0.0, 0.1))
        self.assertEqual(error, touch.MESSAGES[touch.ABSENT])
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1)
        _, error = self._enter(ws, stderr_text='signing failed for ED25519-SK "k": invalid format\n', clock=(0.0, 35.8))
        self.assertEqual(error, touch.MESSAGES[touch.NOT_CONFIRMED])
        ws = workspace_ssh.WorkspaceSSH("user@192.0.2.1", 1)
        _, error = self._enter(ws, stderr_text="ssh: connect to host 192.0.2.1 port 22: Connection refused\n", clock=(0.0, 0.2))
        self.assertTrue(error.startswith(touch.MESSAGES[touch.FAILED]))
        self.assertIn("Connection refused", error)


if __name__ == "__main__":
    unittest.main()
