"""Protect applications and E2E artifacts from build cleanup."""

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "cargo.py"
SPEC = importlib.util.spec_from_file_location("cargo_cleanup", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CleanupTests(unittest.TestCase):
    def test_removes_only_disposable_output_and_preserves_linked_applications(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary)
            deps = target / "debug/deps"
            deps.mkdir(parents=True)
            binary = deps / "homestack-0123456789abcdef"
            binary.write_bytes(b"application")
            binary.chmod(0o755)
            os.link(binary, target / "debug/homestack")
            test = deps / "test_suite-fedcba9876543210"
            test.write_bytes(b"test executable")
            test.chmod(0o755)
            retained = ["debug/deps/libmacro-0123456789abcdef.so", "debug/deps/libcore.rlib",
                        "debug/deps/test_suite-fedcba9876543210.d", "e2e-work/lab/disk.qcow2",
                        "e2e/run/report.json", "release/homestack", "debug/examples/preview"]
            for name in retained:
                path = target / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"retain")
                path.chmod(0o755)
            incremental = target / "debug/incremental/state"
            incremental.mkdir(parents=True)
            (incremental / "cache.bin").write_bytes(b"cache")
            foreign = target / "foreign"
            foreign.mkdir()
            (foreign / "keep").write_bytes(b"foreign")
            (incremental / "symlink").symlink_to(foreign, target_is_directory=True)
            count, _ = MODULE.cleanup(target)
            self.assertEqual(count, 2)
            self.assertFalse(binary.exists())
            self.assertFalse(test.exists())
            self.assertFalse(incremental.parent.exists())
            self.assertEqual((target / "debug/homestack").read_bytes(), b"application")
            self.assertEqual((foreign / "keep").read_bytes(), b"foreign")
            for name in retained:
                self.assertEqual((target / name).read_bytes(), b"retain")
            self.assertEqual(MODULE.cleanup(target), (0, 0))

    def test_does_not_follow_directory_or_executable_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary)
            (target / "debug/deps").mkdir(parents=True)
            foreign = target / "foreign"
            foreign.mkdir()
            executable = foreign / "test-0123456789abcdef"
            executable.write_bytes(b"keep")
            executable.chmod(0o755)
            (target / "debug/examples").symlink_to(foreign, target_is_directory=True)
            (target / "debug/incremental").symlink_to(foreign, target_is_directory=True)
            (target / "debug/deps/test-0123456789abcdef").symlink_to(executable)
            self.assertEqual(MODULE.cleanup(target), (0, 0))
            self.assertEqual(executable.read_bytes(), b"keep")


if __name__ == "__main__":
    unittest.main()
