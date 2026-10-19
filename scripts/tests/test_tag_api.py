"""API release numbering and Git publication, isolated from the real repository."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "tag-api.py"
SPEC = importlib.util.spec_from_file_location("tag_api", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class NumberingTests(unittest.TestCase):
    def test_first_release_ignores_old_hashes_and_phase_tags(self):
        self.assertEqual(MODULE.next_version([]), "0.2.0")
        self.assertEqual(MODULE.next_version([
            "rust-phase-13", "0.1.238-e4284ff477d1", "v0.2.7", "0.02.9", "0.1.999"
        ]), "0.2.0")

    def test_patch_increments_the_numerically_highest_release(self):
        self.assertEqual(MODULE.next_version(["0.2.9", "0.2.10", "0.2.1"]), "0.2.11")
        self.assertEqual(MODULE.next_version(["0.2.100", "0.3.0"]), "0.3.1")


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.remote = root / "remote.git"
        self.repo = root / "work"
        subprocess.run(["git", "init", "--bare", str(self.remote)], check=True, capture_output=True)
        self.repo.mkdir()
        self.git("init", "-b", "release-test")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("commit", "--allow-empty", "-m", "Initial state")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "-u", "origin", "release-test")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True,
                              text=True, capture_output=True).stdout.strip()

    def release(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), *args], cwd=self.repo,
                              text=True, capture_output=True)

    def test_preview_has_no_tag_and_publication_uses_an_annotated_numeric_tag(self):
        preview = self.release("--dry-run")
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertEqual(preview.stdout.strip(), "0.2.0")
        self.assertEqual(self.git("tag", "--list"), "")
        head = self.git("rev-parse", "HEAD")
        published = self.release()
        self.assertEqual(published.returncode, 0, published.stderr)
        self.assertEqual(published.stdout.strip(), "0.2.0")
        self.assertEqual(self.git("cat-file", "-t", "0.2.0"), "tag")
        self.assertEqual(self.git("rev-parse", "0.2.0^{}"), head)
        self.assertIn("refs/tags/0.2.0", self.git("ls-remote", "--tags", "origin"))
        self.assertEqual(self.git("rev-parse", "HEAD"), head)
        self.git("commit", "--allow-empty", "-m", "API change")
        self.git("push")
        published = self.release()
        self.assertEqual(published.returncode, 0, published.stderr)
        self.assertEqual(published.stdout.strip(), "0.2.1")

    def test_dirty_or_unpushed_changes_cannot_be_released(self):
        pending = self.repo / "pending.txt"
        pending.write_text("pending\n")
        rejected = self.release()
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("Commit all changes", rejected.stderr)
        self.assertEqual(self.git("tag", "--list"), "")
        self.git("add", "pending.txt")
        self.git("commit", "-m", "Not pushed yet")
        rejected = self.release()
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("Push the branch", rejected.stderr)
        self.assertEqual(self.git("tag", "--list"), "")

    def test_remote_release_tags_are_fetched_before_numbering(self):
        self.git("tag", "-a", "0.2.10", "-m", "Remote release")
        self.git("push", "origin", "refs/tags/0.2.10")
        self.git("tag", "-d", "0.2.10")
        preview = self.release("--dry-run")
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertEqual(preview.stdout.strip(), "0.2.11")

    def test_a_rejected_push_stops_without_rewriting_or_retrying_tags(self):
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)
        rejected = self.release()
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("rejected", rejected.stderr)
        self.assertEqual(self.git("tag", "--list"), "0.2.0")
        self.assertEqual(self.git("ls-remote", "--tags", "origin"), "")


class BuildMetadataTests(unittest.TestCase):
    def test_client_keeps_the_latest_reachable_api_release_across_tui_commits(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "api-version"
            source = SCRIPT.parents[1] / "crates/hs-cli/build.rs"
            subprocess.run(["rustc", "--edition=2024", str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            repo = root / "repo"
            repo.mkdir()

            def git(*args):
                return subprocess.run(["git", *args], cwd=repo, check=True,
                                      text=True, capture_output=True).stdout.strip()

            def metadata():
                return subprocess.run([str(binary)], cwd=repo, check=True,
                                      text=True, capture_output=True).stdout

            git("init", "-b", "source")
            git("config", "user.name", "Release test")
            git("config", "user.email", "release-test@example.invalid")
            git("commit", "--allow-empty", "-m", "Initial state")
            git("tag", "0.1.238-e4284ff477d1")
            self.assertNotIn("HOMESTACK_API_VERSION=", metadata())
            git("tag", "0.2.9")
            git("tag", "0.2.10")
            git("tag", "0.2.99-invalid")
            git("commit", "--allow-empty", "-m", "TUI change")
            current = git("rev-parse", "HEAD")
            git("checkout", "-b", "future-api")
            git("commit", "--allow-empty", "-m", "Unmerged API change")
            git("tag", "0.3.0")
            git("checkout", "source")
            self.assertEqual(git("rev-parse", "HEAD"), current)
            self.assertIn("cargo:rustc-env=HOMESTACK_API_VERSION=0.2.10\n", metadata())


if __name__ == "__main__":
    unittest.main()
