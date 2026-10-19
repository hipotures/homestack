"""Automatic QEMU setup, isolated from the user's tool cache and network."""

import hashlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[2] / "crates/hs-e2e/src/host-tools.py"
SPEC = importlib.util.spec_from_file_location("e2e_host_tools", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ToolSetupTests(unittest.TestCase):
    def test_cold_setup_relocates_tools_and_firmware_and_warm_setup_is_offline(self):
        with tempfile.TemporaryDirectory(prefix="e2e-tools-test-") as directory:
            base = Path(directory)
            payload = base / "payload"
            (payload / "usr/bin").mkdir(parents=True)
            (payload / "usr/share/qemu").mkdir(parents=True)
            (payload / "usr/share/seabios").mkdir(parents=True)
            for name in ("qemu-system-x86_64", "qemu-img"):
                executable = payload / "usr/bin" / name
                executable.write_text('#!/bin/sh\nexit 0\n')
                executable.chmod(0o755)
            (payload / "usr/share/seabios/bios.bin").write_bytes(b"firmware")
            (payload / "usr/share/qemu/bios.bin").symlink_to("/usr/share/seabios/bios.bin")
            archive = base / "package.tar.zst"
            subprocess.run(["tar", "--zstd", "-cf", str(archive), "-C", str(payload), "usr"], check=True)
            checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
            cache = base / "cache with spaces"
            with patch.object(MODULE.shutil, "which", return_value="/usr/bin/pacman"), \
                    patch.object(MODULE, "packages", return_value=[("https://example.invalid/package", checksum)]), \
                    patch.object(MODULE, "download", side_effect=lambda u, d, p: shutil.copyfile(archive, p)):
                tools = MODULE.prepare(cache)
            self.assertEqual((tools.parent / "root/usr/share/qemu/bios.bin").read_bytes(), b"firmware")
            self.assertFalse(list(cache.glob(".qemu-*")))
            with patch.object(MODULE, "packages", side_effect=AssertionError("warm cache used the network")):
                self.assertEqual(MODULE.prepare(cache), tools)
            MODULE.verify(tools)

    def test_failed_download_does_not_publish_a_partial_tool_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            with patch.object(MODULE.shutil, "which", return_value="/usr/bin/pacman"), \
                    patch.object(MODULE, "packages", return_value=[("https://example.invalid/package", "0" * 64)]), \
                    patch.object(MODULE, "download", side_effect=RuntimeError("download interrupted")):
                with self.assertRaisesRegex(RuntimeError, "interrupted"):
                    MODULE.prepare(cache)
            self.assertFalse((cache / "qemu").exists())
            self.assertFalse(list(cache.glob(".qemu-*")))

    def test_download_rejects_a_checksum_mismatch(self):
        response = io.BytesIO(b"tampered package")
        response.url = "https://example.invalid/package"
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(MODULE.urllib.request, "urlopen", return_value=response):
            with self.assertRaisesRegex(RuntimeError, "SHA-256"):
                MODULE.download(response.url, "0" * 64, Path(directory) / "download")

    def test_package_metadata_requires_https_and_real_checksums(self):
        for row in ("http://example.invalid/package " + "0" * 64, "https://example.invalid/package %h"):
            result = subprocess.CompletedProcess([], 0, stdout=row)
            with patch.object(MODULE.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(RuntimeError, "HTTPS URLs and SHA-256"):
                    MODULE.packages()


if __name__ == "__main__":
    unittest.main()
