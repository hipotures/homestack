from __future__ import annotations

import json
import os
from pathlib import Path
import re
import runpy
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch


BK = Path(__file__).resolve().parents[1] / "src" / "homestack" / "assets" / "backup" / "bk.py"


class BackupCliTests(unittest.TestCase):
    """Regression coverage for the standalone guest-side ``bk`` command."""

    def run_bk(
        self,
        home: Path,
        *args: str,
        cwd: Path | None = None,
        input_text: str = "",
        extra_env: dict[str, str] | None = None,
        timeout: float = 20,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "NO_COLOR": "1",
                "TERM": "dumb",
            }
        )
        # The guest asset is required to be self-contained.  Do not let the
        # repository's source checkout hide an accidental package dependency.
        env.pop("PYTHONPATH", None)
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [sys.executable, str(BK), *args],
            cwd=str(cwd or home),
            env=env,
            input=input_text,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )

    def setUp(self) -> None:
        self._temporary_home = tempfile.TemporaryDirectory(prefix="bk-home-")
        self.home = Path(self._temporary_home.name)

    def tearDown(self) -> None:
        self._temporary_home.cleanup()

    @staticmethod
    def backup_dir(home: Path) -> Path:
        return home / "backup"

    @classmethod
    def config_path(cls, home: Path) -> Path:
        return cls.backup_dir(home) / "backup.yaml"

    @classmethod
    def write_config(
        cls,
        home: Path,
        sources: list[Path],
        retention: int = 7,
        *,
        version: int = 1,
    ) -> Path:
        backup = cls.backup_dir(home)
        backup.mkdir(parents=True, exist_ok=True)
        lines = [f"version: {version}", f"retention: {retention}", "sources:"]
        lines.extend(f"  - {json.dumps(str(source.resolve()))}" for source in sources)
        path = backup / "backup.yaml"
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return path

    @staticmethod
    def config_sources(path: Path) -> list[str]:
        """Read the tiny canonical source list without importing a YAML library."""
        result: list[str] = []
        in_sources = False
        for raw_line in path.read_text(encoding="utf-8").splitlines():
            line = raw_line.strip()
            if line == "sources:":
                in_sources = True
                continue
            if in_sources and line.startswith("-"):
                value = line[1:].strip()
                try:
                    decoded = json.loads(value)
                except json.JSONDecodeError:
                    decoded = value.strip("'\"")
                result.append(str(decoded))
            elif in_sources and line and not raw_line.startswith((" ", "\t")):
                break
        return result

    @staticmethod
    def json_output(result: subprocess.CompletedProcess[str]) -> dict[str, object]:
        if result.returncode != 0:
            raise AssertionError(
                f"bk returned {result.returncode}: stdout={result.stdout!r}; stderr={result.stderr!r}"
            )
        try:
            value = json.loads(result.stdout)
        except json.JSONDecodeError as exc:
            raise AssertionError(
                f"bk did not emit JSON only: stdout={result.stdout!r}; stderr={result.stderr!r}"
            ) from exc
        if not isinstance(value, dict):
            raise AssertionError(f"bk JSON response was not an object: {value!r}")
        return value

    @staticmethod
    def status_value(payload: dict[str, object], *names: str) -> object:
        for name in names:
            if name in payload:
                return payload[name]
        raise AssertionError(f"none of {names!r} present in status payload {payload!r}")

    @staticmethod
    def archive_files(home: Path) -> list[Path]:
        pattern = re.compile(
            r"^backup-(?P<stamp>\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2})"
            r"(?:-(?P<collision>\d+))?\.tgz$"
        )

        def sort_key(path: Path) -> tuple[str, int]:
            match = pattern.fullmatch(path.name)
            if match is None:
                return (path.name, 0)
            return (match.group("stamp"), int(match.group("collision") or 0))

        return sorted((home / "backup" / "archive").glob("backup-*.tgz"), key=sort_key)

    @staticmethod
    def log_for_archive(archive: Path) -> Path:
        return archive.with_suffix(".log")

    @staticmethod
    def regular_tar_payloads(archive: Path) -> list[tuple[str, bytes]]:
        with tarfile.open(archive, "r:gz") as tar:
            payloads: list[tuple[str, bytes]] = []
            for member in tar.getmembers():
                if member.isfile():
                    extracted = tar.extractfile(member)
                    if extracted is not None:
                        payloads.append((member.name, extracted.read()))
            return payloads

    @classmethod
    def extract_archive(cls, archive: Path, destination: Path) -> list[str]:
        with tarfile.open(archive, "r:gz") as tar:
            names = tar.getnames()
            tar.extractall(destination, filter="data")
        return names

    @staticmethod
    def make_file_classifier_wrapper(directory: Path, log_path: Path, delay: float = 0) -> dict[str, str]:
        real_file = shutil.which("file")
        if real_file is None:
            raise unittest.SkipTest("the required file classifier is not installed")
        wrapper = directory / "file"
        wrapper.write_text(
            textwrap.dedent(
                f"""\
                #!{sys.executable}
                import json
                import os
                import sys
                import time
                from pathlib import Path

                with Path({str(log_path)!r}).open("a", encoding="utf-8") as stream:
                    stream.write(json.dumps(sys.argv[1:]) + "\\n")
                if {delay!r}:
                    time.sleep({delay!r})
                os.execv({real_file!r}, [{real_file!r}, *sys.argv[1:]])
                """
            ),
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        return {"PATH": f"{directory}{os.pathsep}{os.environ.get('PATH', '')}"}

    @staticmethod
    def create_wal_database(path: Path) -> tuple[sqlite3.Connection, int]:
        connection = sqlite3.connect(path)
        mode = connection.execute("PRAGMA journal_mode=WAL").fetchone()[0]
        if mode.lower() != "wal":
            connection.close()
            raise unittest.SkipTest("SQLite WAL mode is unavailable")
        connection.execute("PRAGMA wal_autocheckpoint=0")
        connection.execute("CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
        rows = [(index, f"value-{index}") for index in range(1, 25)]
        connection.executemany("INSERT INTO records(id, value) VALUES (?, ?)", rows)
        connection.commit()
        return connection, len(rows)

    def test_bare_command_and_help_aliases_show_help(self) -> None:
        for args in ((), ("--help",), ("help",), ("h",)):
            with self.subTest(args=args):
                result = self.run_bk(self.home, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("bk", result.stdout.lower())
                self.assertIn("usage", result.stdout.lower())
                self.assertFalse((self.home / "backup" / "backup.yaml").exists())

    def test_no_config_commands_are_safe_noops_and_status_is_unconfigured(self) -> None:
        for args in (("list",), ("l",), ("del",), ("d",), ("status",), ("s",)):
            with self.subTest(args=args):
                result = self.run_bk(self.home, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
        run_result = self.run_bk(self.home, "run")
        self.assertEqual(run_result.returncode, 0, run_result.stderr)
        self.assertFalse(self.config_path(self.home).exists())
        self.assertFalse(self.archive_files(self.home))
        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(self.status_value(status, "status"), "unconfigured")
        self.assertFalse(self.config_path(self.home).exists())

    def test_first_add_creates_canonical_config_with_self_source_and_default_retention(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / "source with spaces"
        selected.mkdir()
        (selected / "note.txt").write_text("hello", encoding="utf-8")

        result = self.run_bk(self.home, "add", cwd=working, input_text="1\n")

        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.config_path(self.home)
        self.assertTrue(config.exists())
        self.assertIn("version: 1", config.read_text(encoding="utf-8"))
        self.assertIn("retention: 7", config.read_text(encoding="utf-8"))
        self.assertEqual(
            self.config_sources(config),
            [str(config.resolve()), str(selected.resolve())],
        )
        self.assertIn(str(selected), result.stdout)
        self.assertIn("~/backup/backup.yaml", result.stdout)
        self.assertNotIn("~/backup/backup.yaml.", result.stdout)

    def test_add_without_a_selection_does_not_create_configuration(self) -> None:
        working = self.home / "empty"
        working.mkdir()

        result = self.run_bk(self.home, "add", cwd=working, input_text="\n")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.config_path(self.home).exists())

    def test_add_accepts_arbitrary_selection_separators(self) -> None:
        working = self.home / "work"
        working.mkdir()
        first = working / "first"
        second = working / "second"
        first.touch()
        second.touch()

        result = self.run_bk(self.home, "add", cwd=working, input_text="1(anything)2\n")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.config_sources(self.config_path(self.home))[1:],
            [str(first.resolve()), str(second.resolve())],
        )

    def test_add_lists_hidden_immediate_child_and_exact_duplicate_is_report_only(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / ".hidden source"
        selected.mkdir()
        (selected / "payload").write_text("first", encoding="utf-8")

        first = self.run_bk(self.home, "a", cwd=working, input_text="1\n")
        self.assertEqual(first.returncode, 0, first.stderr)
        before = self.config_path(self.home).read_bytes()
        duplicate = self.run_bk(self.home, "add", cwd=working, input_text="1\n")
        self.assertEqual(duplicate.returncode, 0, duplicate.stderr)
        self.assertIn("already", duplicate.stdout.lower())
        self.assertEqual(self.config_path(self.home).read_bytes(), before)
        self.assertIn(selected.name, first.stdout)

    def test_delete_keeps_self_entry_and_aliases_run_and_status_work(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / "source"
        selected.mkdir()
        (selected / "file").write_text("data", encoding="utf-8")
        added = self.run_bk(self.home, "a", cwd=working, input_text="1\n")
        self.assertEqual(added.returncode, 0, added.stderr)

        deleted = self.run_bk(self.home, "d", cwd=working, input_text="1\n")
        sources = self.config_sources(self.config_path(self.home))
        # Implementations may show the protected self-entry as a non-selectable
        # row (making the user source number 2) or omit it from the choices.
        if str(selected.resolve()) in sources:
            deleted = self.run_bk(self.home, "del", cwd=working, input_text="2\n")
            sources = self.config_sources(self.config_path(self.home))
        self.assertEqual(deleted.returncode, 0, deleted.stderr)
        self.assertEqual(sources, [str(self.config_path(self.home).resolve())])

        run = self.run_bk(self.home, "r")
        self.assertEqual(run.returncode, 0, run.stderr)
        status = self.run_bk(self.home, "s")
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertIn("ok", status.stdout.lower())

    def test_runtime_tree_is_rejected_as_a_source(self) -> None:
        runtime = self.backup_dir(self.home)
        runtime.mkdir()
        (runtime / "archive").mkdir()
        from_home = self.run_bk(self.home, "add", cwd=self.home, input_text="1\n")
        self.assertNotEqual(from_home.returncode, 0)
        self.assertIn("backup", (from_home.stdout + from_home.stderr).lower())
        self.assertFalse(self.config_path(self.home).exists())

        another_home = self.home / "nested-home"
        another_home.mkdir()
        another_runtime = another_home / "backup"
        another_runtime.mkdir()
        (another_runtime / "archive").mkdir()
        from_runtime = self.run_bk(
            another_home,
            "add",
            cwd=another_runtime,
            input_text="1\n",
        )
        self.assertNotEqual(from_runtime.returncode, 0)
        self.assertFalse((another_runtime / "backup.yaml").exists())

        working = self.home / "work"
        working.mkdir()
        (working / "home-link").symlink_to(self.home, target_is_directory=True)
        from_runtime_parent = self.run_bk(self.home, "add", cwd=working, input_text="1\n")
        self.assertNotEqual(from_runtime_parent.returncode, 0)
        self.assertFalse(self.config_path(self.home).exists())

    def test_json_list_run_and_status_are_valid_json_only(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data").write_text("json", encoding="utf-8")
        config = self.write_config(self.home, [self.config_path(self.home), source])

        listed = self.json_output(self.run_bk(self.home, "list", "--json"))
        self.assertIsInstance(listed.get("sources"), list)
        listed_sources = listed["sources"]
        assert isinstance(listed_sources, list)
        self.assertEqual(len(listed_sources), 2)
        self.assertIn(str(config.resolve()), json.dumps(listed_sources))

        ran = self.json_output(self.run_bk(self.home, "run", "--json"))
        self.assertEqual(self.status_value(ran, "status"), "ok")
        self.assertNotIn("\x1b[", json.dumps(ran))

        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(self.status_value(status, "status"), "ok")
        self.assertNotIn("\x1b[", json.dumps(status))

    def test_overlapping_sources_are_allowed_and_independently_archived(self) -> None:
        dev = self.home / "DEV"
        parent = dev / "foo"
        parent.mkdir(parents=True)
        child = parent / "config.yaml"
        child.write_text("child content", encoding="utf-8")

        added_parent = self.run_bk(self.home, "add", cwd=dev, input_text="1\n")
        added_child = self.run_bk(self.home, "add", cwd=parent, input_text="1\n")

        self.assertEqual(added_parent.returncode, 0, added_parent.stderr)
        self.assertEqual(added_child.returncode, 0, added_child.stderr)
        config = self.config_path(self.home)

        listed = self.json_output(self.run_bk(self.home, "list", "--json"))
        listed_sources = listed["sources"]
        self.assertIsInstance(listed_sources, list)
        self.assertIn(str(parent.resolve()), json.dumps(listed_sources))
        self.assertIn(str(child.resolve()), json.dumps(listed_sources))

        result = self.run_bk(self.home, "run")
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.archive_files(self.home)[-1]
        payloads = self.regular_tar_payloads(archive)
        config_members = [
            (name, content)
            for name, content in payloads
            if Path(name).name == child.name and content == child.read_bytes()
        ]
        self.assertGreaterEqual(len(config_members), 2, payloads)
        self.assertEqual(len({name for name, _ in config_members}), len(config_members))
        self.assertEqual(self.config_sources(config), [
            str(self.config_path(self.home).resolve()),
            str(parent.resolve()),
            str(child.resolve()),
        ])

    def test_recursive_directory_snapshot_preserves_paths_with_spaces_and_sources(self) -> None:
        source = self.home / "directory with spaces"
        nested = source / "nested directory"
        nested.mkdir(parents=True)
        top_file = source / "top level.txt"
        nested_file = nested / "nested file.txt"
        top_file.write_text("top", encoding="utf-8")
        nested_file.write_text("nested", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.archive_files(self.home)[-1]
        payloads = self.regular_tar_payloads(archive)
        self.assertTrue(any(content == b"top" for _, content in payloads))
        self.assertTrue(any(content == b"nested" for _, content in payloads))
        names = [name for name, _ in payloads]
        self.assertTrue(any(Path(name).name == top_file.name for name in names))
        self.assertTrue(any(Path(name).name == nested_file.name for name in names))
        self.assertEqual(top_file.read_text(encoding="utf-8"), "top")
        self.assertEqual(nested_file.read_text(encoding="utf-8"), "nested")

    def test_empty_directory_and_symlink_are_preserved(self) -> None:
        source = self.home / "source"
        empty = source / "empty"
        empty.mkdir(parents=True)
        link = source / "empty-link"
        link.symlink_to("empty", target_is_directory=True)
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.archive_files(self.home)[-1]
        with tarfile.open(archive, "r:gz") as tar:
            members = tar.getmembers()
        empty_members = [member for member in members if Path(member.name).name == "empty"]
        link_members = [member for member in members if Path(member.name).name == "empty-link"]
        self.assertTrue(any(member.isdir() for member in empty_members), members)
        self.assertTrue(any(member.issym() and member.linkname == "empty" for member in link_members), members)

    def test_read_only_source_directory_is_archived_and_staging_is_cleaned(self) -> None:
        source = self.home / "read-only"
        source.mkdir()
        (source / "data").write_text("content", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])
        source.chmod(0o555)
        try:
            result = self.run_bk(self.home, "run")
        finally:
            source.chmod(0o755)

        self.assertEqual(result.returncode, 0, result.stderr)
        work_entries = list((self.backup_dir(self.home) / ".work").glob("run-*"))
        self.assertEqual(work_entries, [])

    def test_unsupported_special_object_fails_without_publishing_an_archive(self) -> None:
        source = self.home / "source"
        source.mkdir()
        fifo = source / "events.fifo"
        os.mkfifo(fifo)
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported", (result.stdout + result.stderr).lower())
        self.assertFalse(self.archive_files(self.home))
        self.assertTrue(fifo.exists())
        self.assertTrue((self.backup_dir(self.home) / "last-failed.log").exists())

    def test_successful_archive_and_log_are_hardlinked_current_paths(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "file.txt").write_text("content", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        archives = self.archive_files(self.home)
        self.assertEqual(len(archives), 1)
        archive = archives[0]
        log = self.log_for_archive(archive)
        current_archive = self.backup_dir(self.home) / "backup.tgz"
        current_log = self.backup_dir(self.home) / "backup.log"
        self.assertTrue(archive.exists())
        self.assertTrue(log.exists())
        self.assertTrue(current_archive.exists())
        self.assertTrue(current_log.exists())
        self.assertEqual(os.stat(current_archive).st_ino, os.stat(archive).st_ino)
        self.assertEqual(os.stat(current_log).st_ino, os.stat(log).st_ino)
        self.assertGreater(archive.stat().st_size, 0)
        self.assertIn("status", log.read_text(encoding="utf-8").lower())

    def test_retention_counts_successful_archives_and_prunes_matching_logs(self) -> None:
        source = self.home / "source"
        source.mkdir()
        data = source / "data.txt"
        data.write_text("one", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source], retention=2)

        for content in ("one", "two", "three"):
            data.write_text(content, encoding="utf-8")
            result = self.run_bk(self.home, "run")
            self.assertEqual(result.returncode, 0, result.stderr)

        archives = self.archive_files(self.home)
        logs = sorted((self.home / "backup" / "archive").glob("backup-*.log"))
        self.assertEqual(len(archives), 2)
        self.assertEqual(len(logs), 2)
        self.assertEqual({p.stem for p in archives}, {p.stem for p in logs})
        newest = archives[-1]
        self.assertEqual(os.stat(self.backup_dir(self.home) / "backup.tgz").st_ino, os.stat(newest).st_ino)
        self.assertEqual(
            os.stat(self.backup_dir(self.home) / "backup.log").st_ino,
            os.stat(self.log_for_archive(newest)).st_ino,
        )
        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        retained = self.status_value(
            status,
            "retained_successful_archives",
            "retained_archives",
            "archive_count",
        )
        self.assertEqual(retained, 2)

    def test_retention_one_handles_repeated_timestamp_collisions(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data").write_text("content", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source], retention=1)

        for _ in range(3):
            result = self.run_bk(self.home, "run")
            self.assertEqual(result.returncode, 0, result.stderr)

        archives = self.archive_files(self.home)
        logs = sorted((self.home / "backup" / "archive").glob("backup-*.log"))
        self.assertEqual(len(archives), 1)
        self.assertEqual(len(logs), 1)
        self.assertEqual(os.stat(self.backup_dir(self.home) / "backup.tgz").st_ino, os.stat(archives[0]).st_ino)

    def test_status_reflects_current_retention_and_missing_current_links(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data").write_text("content", encoding="utf-8")
        config = self.write_config(self.home, [self.config_path(self.home), source])
        result = self.run_bk(self.home, "run")
        self.assertEqual(result.returncode, 0, result.stderr)
        config.write_text(config.read_text(encoding="utf-8").replace("retention: 7", "retention: 2"), encoding="utf-8")

        current_status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(current_status["configured_retention"], 2)

        (self.backup_dir(self.home) / "backup.tgz").unlink()
        (self.backup_dir(self.home) / "backup.log").unlink()
        missing_status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertIsNone(missing_status["current_backup_path"])
        self.assertIsNone(missing_status["current_successful_log_path"])
        self.assertEqual(missing_status["retained_successful_archives"], 1)

    def test_failed_run_preserves_current_backup_and_writes_failure_status(self) -> None:
        source = self.home / "source"
        source.mkdir()
        source_file = source / "data.txt"
        source_file.write_text("stable", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source_file])
        successful = self.run_bk(self.home, "run")
        self.assertEqual(successful.returncode, 0, successful.stderr)
        current = self.backup_dir(self.home) / "backup.tgz"
        current_log = self.backup_dir(self.home) / "backup.log"
        before_bytes = current.read_bytes()
        before_archive_inode = os.stat(current).st_ino
        before_log_inode = os.stat(current_log).st_ino
        before_archives = [path.name for path in self.archive_files(self.home)]

        source_file.unlink()
        failed = self.run_bk(self.home, "run")

        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(current.read_bytes(), before_bytes)
        self.assertEqual(os.stat(current).st_ino, before_archive_inode)
        self.assertEqual(os.stat(current_log).st_ino, before_log_inode)
        self.assertEqual([path.name for path in self.archive_files(self.home)], before_archives)
        failed_log = self.backup_dir(self.home) / "last-failed.log"
        self.assertTrue(failed_log.exists())
        self.assertIn("fail", failed_log.read_text(encoding="utf-8").lower())
        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(self.status_value(status, "status"), "failed")
        error = self.status_value(status, "error", "error_summary", "last_error")
        self.assertIn("data.txt", str(error))
        human = self.run_bk(self.home, "status")
        self.assertEqual(human.returncode, 0, human.stderr)
        self.assertIn("failed", human.stdout.lower())
        self.assertIn("successful", human.stdout.lower())

    def test_concurrent_run_is_rejected_by_persistent_lock(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data.txt").write_text("slow", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])
        classifier_dir = self.home / "classifier"
        classifier_dir.mkdir()
        log_path = self.home / "classifier-argv.jsonl"
        classifier_env = self.make_file_classifier_wrapper(classifier_dir, log_path, delay=1.2)
        env = os.environ.copy()
        env.update({"HOME": str(self.home), "NO_COLOR": "1", "TERM": "dumb"})
        env.pop("PYTHONPATH", None)
        env.update(classifier_env)
        first = subprocess.Popen(
            [sys.executable, str(BK), "run"],
            cwd=str(self.home),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            time.sleep(0.25)
            second = self.run_bk(self.home, "run", extra_env=classifier_env)
            first_stdout, first_stderr = first.communicate(timeout=15)
        finally:
            if first.poll() is None:
                first.kill()
                first.communicate(timeout=5)
        self.assertEqual(first.returncode, 0, first_stderr)
        self.assertNotEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertRegex(
            (second.stdout + second.stderr).lower(),
            r"already|another|concurrent|lock|running",
        )

    def test_concurrent_malformed_config_run_cannot_overwrite_active_diagnostics(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data").write_text("slow", encoding="utf-8")
        config = self.write_config(self.home, [self.config_path(self.home), source])
        valid_config = config.read_text(encoding="utf-8")
        classifier_dir = self.home / "classifier"
        classifier_dir.mkdir()
        classifier_log = self.home / "classifier.jsonl"
        classifier_env = self.make_file_classifier_wrapper(classifier_dir, classifier_log, delay=1.2)
        env = os.environ.copy()
        env.update({"HOME": str(self.home), "NO_COLOR": "1", "TERM": "dumb", **classifier_env})
        env.pop("PYTHONPATH", None)
        first = subprocess.Popen(
            [sys.executable, str(BK), "run"],
            cwd=str(self.home),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 5
            while not classifier_log.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(classifier_log.exists(), "the first run did not reach classification")
            config.write_text("version: 2\nretention: 7\nsources:\n", encoding="utf-8")
            second = self.run_bk(self.home, "run", extra_env=classifier_env)
            self.assertEqual(second.returncode, 75, second.stdout + second.stderr)
            self.assertIn("running", (second.stdout + second.stderr).lower())
            self.assertFalse((self.backup_dir(self.home) / "last-failed.log").exists())
            self.assertFalse((self.backup_dir(self.home) / "status.json").exists())
            config.write_text(valid_config, encoding="utf-8")
            first.communicate(timeout=15)
        finally:
            if first.poll() is None:
                first.kill()
                first.communicate(timeout=5)

    def test_classifier_is_file_command_and_each_regular_file_is_classified_once(self) -> None:
        source = self.home / "tree"
        nested = source / "nested"
        nested.mkdir(parents=True)
        first = source / "one"
        second = nested / "two.bin"
        first.write_text("one", encoding="utf-8")
        second.write_bytes(b"two")
        self.write_config(self.home, [self.config_path(self.home), source])
        classifier_dir = self.home / "classifier"
        classifier_dir.mkdir()
        classifier_log = self.home / "classifier.jsonl"
        env = self.make_file_classifier_wrapper(classifier_dir, classifier_log)

        result = self.run_bk(self.home, "run", extra_env=env)

        self.assertEqual(result.returncode, 0, result.stderr)
        calls: list[list[str]] = []
        for line in classifier_log.read_text(encoding="utf-8").splitlines():
            calls.append(json.loads(line))
        expected = {
            str(self.config_path(self.home).resolve()),
            str(first.resolve()),
            str(second.resolve()),
        }
        counts = {path: 0 for path in expected}
        for argv in calls:
            for path in expected:
                counts[path] += argv.count(path)
        self.assertEqual(counts, {path: 1 for path in expected}, calls)

    def test_extensionless_wal_sqlite_is_backed_up_and_sidecars_are_not_restored(self) -> None:
        source = self.home / "database tree"
        source.mkdir()
        database = source / "state without extension"
        connection, row_count = self.create_wal_database(database)
        self.assertTrue(database.with_name(database.name + "-wal").exists())
        self.assertTrue(database.with_name(database.name + "-shm").exists())
        original_database = database.read_bytes()
        original_wal = database.with_name(database.name + "-wal").read_bytes()
        original_wal_mtime = database.with_name(database.name + "-wal").stat().st_mtime_ns
        self.write_config(self.home, [self.config_path(self.home), source])
        classifier_dir = self.home / "classifier"
        classifier_dir.mkdir()
        classifier_log = self.home / "classifier.jsonl"
        env = self.make_file_classifier_wrapper(classifier_dir, classifier_log)
        try:
            result = self.run_bk(self.home, "run", extra_env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            # Check the live database before closing the fixture connection;
            # closing the final SQLite connection may itself checkpoint WAL.
            self.assertEqual(database.read_bytes(), original_database)
            self.assertEqual(database.with_name(database.name + "-wal").read_bytes(), original_wal)
            self.assertEqual(database.with_name(database.name + "-wal").stat().st_mtime_ns, original_wal_mtime)
        finally:
            connection.close()

        archive = self.archive_files(self.home)[-1]
        names = self.extract_archive(archive, self.home / "extracted")
        sidecar_names = {
            database.name + "-wal",
            database.name + "-shm",
            database.name + "-journal",
        }
        self.assertFalse(any(Path(name).name in sidecar_names for name in names), names)
        extracted_databases = [
            path
            for path in (self.home / "extracted").rglob("*")
            if path.is_file() and path.read_bytes().startswith(b"SQLite format 3\x00")
        ]
        self.assertGreaterEqual(len(extracted_databases), 1)
        with sqlite3.connect(extracted_databases[0]) as checked:
            self.assertEqual(checked.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            self.assertEqual(checked.execute("SELECT count(*) FROM records").fetchone()[0], row_count)

    def test_closed_wal_database_does_not_create_source_sidecars(self) -> None:
        source = self.home / "database tree"
        source.mkdir()
        database = source / "closed-state"
        connection, row_count = self.create_wal_database(database)
        connection.close()
        wal = database.with_name(database.name + "-wal")
        shm = database.with_name(database.name + "-shm")
        self.assertFalse(wal.exists())
        self.assertFalse(shm.exists())
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(wal.exists())
        self.assertFalse(shm.exists())
        self.assertEqual(set(database.parent.iterdir()), {database})
        extracted = self.home / "closed-extracted"
        self.extract_archive(self.archive_files(self.home)[-1], extracted)
        copies = [path for path in extracted.rglob("closed-state") if path.is_file()]
        self.assertEqual(len(copies), 1)
        with sqlite3.connect(copies[0]) as checked:
            self.assertEqual(checked.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            self.assertEqual(checked.execute("SELECT count(*) FROM records").fetchone()[0], row_count)

    def test_closed_wal_database_change_during_backup_is_rejected(self) -> None:
        database = self.home / "closed-state"
        connection, _ = self.create_wal_database(database)
        connection.close()
        module = runpy.run_path(str(BK))
        real_connect = sqlite3.connect

        class MutatingSourceConnection:
            def __init__(self, wrapped: sqlite3.Connection) -> None:
                self.wrapped = wrapped

            def backup(self, destination: sqlite3.Connection, **kwargs: object) -> None:
                self.wrapped.backup(destination, **kwargs)
                with real_connect(database) as writer:
                    writer.execute("INSERT INTO records(value) VALUES ('changed during backup')")
                    writer.commit()

            def close(self) -> None:
                self.wrapped.close()

        def connect(target: str, *args: object, **kwargs: object) -> sqlite3.Connection | MutatingSourceConnection:
            opened = real_connect(target, *args, **kwargs)
            if kwargs.get("uri") and "immutable=1" in target:
                return MutatingSourceConnection(opened)
            return opened

        destination = self.home / "staged"
        with patch.object(module["sqlite3"], "connect", side_effect=connect):
            with self.assertRaises(module["BKError"]):
                module["copy_sqlite_file"](database, destination, [])

    def test_sidecar_named_directory_is_not_suppressed(self) -> None:
        source = self.home / "source"
        source.mkdir()
        database = source / "database"
        with sqlite3.connect(database) as connection:
            connection.execute("CREATE TABLE data (value TEXT)")
            connection.commit()
        similarly_named = source / "database-journal"
        similarly_named.mkdir()
        sidecar_target = source / "sidecar-target"
        sidecar_target.write_text("not SQLite state", encoding="utf-8")
        sidecar_symlink = source / "database-wal"
        sidecar_symlink.symlink_to(sidecar_target.name)
        module = runpy.run_path(str(BK))
        config = module["Config"](version=1, retention=7, sources=[source])

        plan = module["build_snapshot_plan"](config)

        entries = [entry for source_plan in plan.sources for entry in source_plan.entries]
        matching = [entry for entry in entries if entry.source_path == similarly_named]
        self.assertEqual(len(matching), 1)
        self.assertEqual(matching[0].kind, "directory")
        self.assertFalse(matching[0].skip_sidecar)
        symlinks = [entry for entry in entries if entry.source_path == sidecar_symlink]
        self.assertEqual(len(symlinks), 1)
        self.assertTrue(symlinks[0].skip_sidecar)

    def test_staging_cleanup_does_not_remove_preexisting_work_entries_or_escape_work_root(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "file").write_text("payload", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])
        work_root = self.backup_dir(self.home) / ".work"
        work_root.mkdir(parents=True)
        outside = self.home / "outside"
        outside.mkdir()
        sentinel = outside / "sentinel"
        sentinel.write_text("must survive", encoding="utf-8")
        escape = work_root / "preexisting-link"
        escape.symlink_to(outside, target_is_directory=True)

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "must survive")
        self.assertTrue(escape.is_symlink())
        self.assertTrue(outside.exists())

        module = runpy.run_path(str(BK))
        with self.assertRaises(module["BKError"]):
            module["cleanup_run_directory"](outside, work_root)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "must survive")

        linked_work = self.home / "linked-work"
        linked_work.symlink_to(outside, target_is_directory=True)
        victim = outside / "run-victim"
        victim.mkdir()
        (victim / "data").write_text("survive", encoding="utf-8")
        with self.assertRaises(module["BKError"]):
            module["cleanup_run_directory"](linked_work / victim.name, linked_work)
        self.assertEqual((victim / "data").read_text(encoding="utf-8"), "survive")

    def test_materialization_rejects_a_planned_directory_that_disappears(self) -> None:
        module = runpy.run_path(str(BK))
        source = self.home / "empty-source"
        source.mkdir()
        config = module["Config"](version=1, retention=7, sources=[source])
        plan = module["build_snapshot_plan"](config)
        source.rmdir()
        run_directory = self.home / "run-test"
        run_directory.mkdir()

        with self.assertRaises(module["BKError"]):
            module["materialize_plan"](plan, run_directory)

    def test_timestamp_collision_uses_a_deterministic_suffix(self) -> None:
        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(self.home)
        paths.archive.mkdir(parents=True)
        timestamp = module["datetime"](2026, 9, 15, 14, 35, 42).astimezone()
        first = module["make_archive_pair"](paths, timestamp)
        first.archive.touch()
        first.log.touch()

        second = module["make_archive_pair"](paths, timestamp)

        self.assertEqual(second.archive.name, "backup-2026-09-15_14-35-42-01.tgz")
        self.assertEqual(second.log.name, "backup-2026-09-15_14-35-42-01.log")
        first.archive.unlink()
        first.log.unlink()
        second.archive.touch()
        second.log.touch()
        third = module["make_archive_pair"](paths, timestamp)
        self.assertEqual(third.archive.name, "backup-2026-09-15_14-35-42-02.tgz")

    def test_malformed_or_unsafe_config_fails_without_repairing_it(self) -> None:
        source = self.home / "source"
        source.mkdir()
        self_source = json.dumps(str(self.config_path(self.home)))
        encoded_source = json.dumps(str(source))
        cases = {
            "unsupported version": f"version: 2\nretention: 7\nsources:\n  - {self_source}\n",
            "invalid retention": f"version: 1\nretention: no\nsources:\n  - {self_source}\n",
            "zero retention": f"version: 1\nretention: 0\nsources:\n  - {self_source}\n",
            "relative source": 'version: 1\nretention: 7\nsources:\n  - "relative/path"\n',
            "self is not first": (
                "version: 1\nretention: 7\nsources:\n"
                "  - %s\n  - %s\n"
                % (encoded_source, self_source)
            ),
            "missing sources": "version: 1\nretention: 7\nsources: []\n",
            "source contains runtime": (
                "version: 1\nretention: 7\nsources:\n"
                f"  - {self_source}\n  - {json.dumps(str(self.home))}\n"
            ),
        }
        for label, content in cases.items():
            with self.subTest(label=label):
                backup = self.backup_dir(self.home)
                backup.mkdir(parents=True, exist_ok=True)
                config = self.config_path(self.home)
                config.write_text(content, encoding="utf-8")
                before = config.read_bytes()
                result = self.run_bk(self.home, "run")
                self.assertNotEqual(result.returncode, 0)
                diagnostic = (result.stdout + result.stderr).lower()
                self.assertRegex(diagnostic, r"config|invalid|malformed|unsupported|source|retention")
                self.assertEqual(config.read_bytes(), before)
                self.assertFalse(self.archive_files(self.home))
                failed_log = backup / "last-failed.log"
                status_path = backup / "status.json"
                self.assertTrue(failed_log.exists())
                self.assertIn("failed", failed_log.read_text(encoding="utf-8").lower())
                recorded = json.loads(status_path.read_text(encoding="utf-8"))
                self.assertEqual(recorded["status"], "failed")
                self.assertTrue(recorded["error"])

    def test_source_files_and_directories_are_not_modified_by_snapshot(self) -> None:
        source = self.home / "source"
        source.mkdir()
        file_path = source / "original.txt"
        file_path.write_text("original", encoding="utf-8")
        before_stat = file_path.stat()
        before_bytes = file_path.read_bytes()
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(file_path.read_bytes(), before_bytes)
        after_stat = file_path.stat()
        self.assertEqual(after_stat.st_ino, before_stat.st_ino)
        self.assertEqual(after_stat.st_mtime_ns, before_stat.st_mtime_ns)
        self.assertTrue(source.is_dir())


if __name__ == "__main__":
    unittest.main()
