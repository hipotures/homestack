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


class RecordingProgress:
    """Minimal progress recorder for deterministic byte-accounting tests."""

    def __init__(self) -> None:
        self.records: list[dict[str, object]] = []
        self.current: dict[str, object] | None = None

    def stage(
        self,
        description: str,
        *,
        total: int | None = None,
        byte_progress: bool = False,
    ) -> None:
        self.current = {
            "description": description,
            "total": total,
            "byte_progress": byte_progress,
            "completed": 0,
            "advances": [],
            "values": [0],
        }
        self.records.append(self.current)

    def advance(self, amount: int = 1) -> None:
        assert self.current is not None
        completed = int(self.current["completed"]) + amount
        self.current["completed"] = completed
        advances = self.current["advances"]
        values = self.current["values"]
        assert isinstance(advances, list)
        assert isinstance(values, list)
        advances.append(amount)
        values.append(completed)

    def complete(self) -> None:
        assert self.current is not None
        total = self.current["total"]
        assert isinstance(total, int)
        self.current["completed"] = total
        values = self.current["values"]
        assert isinstance(values, list)
        values.append(total)


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

    def run_edit(self, home: Path, cwd: Path, selected_indices: list[int], *, cancelled: bool = False) -> int:
        """Run edit_command with a deterministic selector result."""

        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(home)
        result = module["SelectorResult"](selected_indices, cancelled=cancelled)
        previous = Path.cwd()
        try:
            os.chdir(cwd)
            with patch.dict(
                module["edit_command"].__globals__,
                {
                    "selector_tty_available": lambda: True,
                    "run_edit_selector": lambda *args, **kwargs: result,
                },
            ):
                return module["edit_command"](paths)
        finally:
            os.chdir(previous)

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
        respect_gitignore: bool = True,
    ) -> Path:
        backup = cls.backup_dir(home)
        backup.mkdir(parents=True, exist_ok=True)
        lines = [
            f"version: {version}",
            f"retention: {retention}",
            f"respect_gitignore: {'true' if respect_gitignore else 'false'}",
            "sources:",
        ]
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
                self.assertIn("edit", result.stdout.lower())
                self.assertNotIn("add immediate children", result.stdout.lower())
                self.assertNotIn("remove configured", result.stdout.lower())
                self.assertFalse((self.home / "backup" / "backup.yaml").exists())

    def test_no_config_commands_are_safe_noops_and_status_is_unconfigured(self) -> None:
        for args in (("list",), ("l",), ("status",), ("s",)):
            with self.subTest(args=args):
                result = self.run_bk(self.home, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
        for args in (("edit",), ("e",)):
            with self.subTest(args=args):
                result = self.run_bk(self.home, *args)
                self.assertEqual(result.returncode, 2)
                self.assertIn("interactive TTY", (result.stdout + result.stderr))
                self.assertIn("bk list", (result.stdout + result.stderr))
        run_result = self.run_bk(self.home, "run")
        self.assertEqual(run_result.returncode, 0, run_result.stderr)
        self.assertEqual(run_result.stdout.strip().splitlines()[-1], "Status: UNCONFIGURED")
        self.assertFalse(self.config_path(self.home).exists())
        self.assertFalse(self.archive_files(self.home))
        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(self.status_value(status, "status"), "unconfigured")
        self.assertFalse(self.config_path(self.home).exists())

    def test_obsolete_add_and_delete_commands_are_unknown(self) -> None:
        for command in ("add", "a", "del", "d"):
            with self.subTest(command=command):
                result = self.run_bk(self.home, command)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unknown command", (result.stdout + result.stderr).lower())

    def test_first_edit_creates_canonical_config_with_self_source_and_default_retention(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / "source with spaces"
        selected.mkdir()
        (selected / "note.txt").write_text("hello", encoding="utf-8")

        result = self.run_edit(self.home, working, [1])

        self.assertEqual(result, 0)
        config = self.config_path(self.home)
        self.assertTrue(config.exists())
        self.assertIn("version: 1", config.read_text(encoding="utf-8"))
        self.assertIn("retention: 7", config.read_text(encoding="utf-8"))
        self.assertIn("respect_gitignore: true", config.read_text(encoding="utf-8"))
        self.assertEqual(
            self.config_sources(config),
            [str(config.resolve()), str(selected.resolve())],
        )

    def test_config_accepts_only_canonical_gitignore_booleans(self) -> None:
        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(self.home)
        self_source = json.dumps(str(paths.config))

        def config_text(value: str) -> str:
            return (
                "version: 1\n"
                "retention: 7\n"
                f"respect_gitignore: {value}\n"
                "sources:\n"
                f"  - {self_source}\n"
            )

        self.assertTrue(
            module["parse_config_text"](config_text("true"), paths.config, paths).respect_gitignore
        )
        self.assertFalse(
            module["parse_config_text"](config_text("false"), paths.config, paths).respect_gitignore
        )
        for invalid in ("True", "False", "yes", "1", '"true"'):
            with self.subTest(invalid=invalid):
                with self.assertRaises(module["ConfigError"]):
                    module["parse_config_text"](config_text(invalid), paths.config, paths)

        paths.runtime.mkdir()
        module["write_config"](paths, module["Config"](1, 7, [paths.config], False))
        self.assertIn("respect_gitignore: false\n", paths.config.read_text(encoding="utf-8"))

    def test_edit_without_a_selection_does_not_create_configuration(self) -> None:
        working = self.home / "empty"
        working.mkdir()

        result = self.run_edit(self.home, working, [])

        self.assertEqual(result, 0)
        self.assertFalse(self.config_path(self.home).exists())

    def test_edit_cancel_does_not_create_configuration(self) -> None:
        working = self.home / "empty"
        working.mkdir()

        result = self.run_edit(self.home, working, [1], cancelled=True)

        self.assertEqual(result, 1)
        self.assertFalse(self.config_path(self.home).exists())

    def test_edit_cancel_does_not_modify_existing_configuration(self) -> None:
        working = self.home / "work"
        working.mkdir()
        source = working / "source"
        source.touch()
        config = self.write_config(self.home, [self.config_path(self.home), source])
        before = config.read_bytes()

        result = self.run_edit(self.home, working, [], cancelled=True)

        self.assertEqual(result, 1)
        self.assertEqual(config.read_bytes(), before)

    def test_edit_preserves_order_and_applies_additions_and_removals(self) -> None:
        working = self.home / "work"
        working.mkdir()
        first = working / "first"
        second = working / "second"
        first.touch()
        second.touch()
        elsewhere = self.home / "elsewhere"
        elsewhere.touch()
        config = self.write_config(self.home, [self.config_path(self.home), elsewhere, first])

        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(self.home)
        items, groups, selected = module["build_edit_selector"](paths, module["immediate_children"](working), module["read_config"](paths))
        self.assertEqual([item.index for item in items if item.path == first], [1])
        self.assertEqual([item.index for item in items if item.path == second], [2])
        elsewhere_index = next(item.index for item in items if item.path == elsewhere)
        self.assertEqual(selected, {1, elsewhere_index})
        self.assertFalse(groups[1].expanded)

        # Select second and unselect the existing first source.  The existing
        # elsewhere source stays first; the new current-directory source is
        # appended in displayed order.
        result = self.run_edit(self.home, working, [elsewhere_index, 2])

        self.assertEqual(result, 0)
        self.assertEqual(
            self.config_sources(self.config_path(self.home))[1:],
            [str(elsewhere.resolve()), str(second.resolve())],
        )

    def test_edit_applies_additions_and_removals_with_one_atomic_config_write(self) -> None:
        working = self.home / "work"
        working.mkdir()
        first = working / "first"
        second = working / "second"
        first.touch()
        second.touch()
        elsewhere = self.home / "elsewhere"
        elsewhere.touch()
        self.write_config(self.home, [self.config_path(self.home), elsewhere, first])

        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(self.home)
        config = module["read_config"](paths)
        items, groups, selected = module["build_edit_selector"](
            paths, module["immediate_children"](working), config
        )
        elsewhere_index = next(item.index for item in items if item.path == elsewhere)
        state = module["SelectorState"](
            items, groups=groups, selected_indices={2, elsewhere_index}
        )
        writes = []

        with patch.dict(
            module["apply_edit_selection"].__globals__,
            {"write_config": lambda write_paths, value: writes.append((write_paths, value))},
        ):
            changed = module["apply_edit_selection"](paths, config, state)

        self.assertTrue(changed)
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0][0], paths)
        self.assertEqual(writes[0][1].sources, [paths.config.resolve(), elsewhere.resolve(), second.resolve()])

    def test_edit_lists_hidden_children_and_exact_configured_path_starts_checked(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / ".hidden source"
        selected.mkdir()
        (selected / "payload").write_text("first", encoding="utf-8")

        self.assertEqual(self.run_edit(self.home, working, [1]), 0)
        module = runpy.run_path(str(BK))
        paths = module["Paths"].from_home(self.home)
        config = module["read_config"](paths)
        items, _groups, selected_indices = module["build_edit_selector"](
            paths, module["immediate_children"](working), config
        )
        item = next(item for item in items if item.path == selected)
        self.assertTrue(item.label.endswith("/"))
        self.assertIn(item.index, selected_indices)
        self.assertEqual(self.config_sources(self.config_path(self.home)).count(str(selected.resolve())), 1)

    def test_edit_removing_last_user_source_keeps_self_entry(self) -> None:
        working = self.home / "work"
        working.mkdir()
        selected = working / "source"
        selected.mkdir()
        (selected / "file").write_text("data", encoding="utf-8")
        self.assertEqual(self.run_edit(self.home, working, [1]), 0)
        self.assertEqual(self.run_edit(self.home, working, [], cancelled=False), 0)
        sources = self.config_sources(self.config_path(self.home))
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
        # The non-TTY editor fails before it can mutate anything.
        from_home = self.run_bk(self.home, "edit", cwd=self.home)
        self.assertNotEqual(from_home.returncode, 0)
        self.assertIn("TTY", (from_home.stdout + from_home.stderr))
        self.assertFalse(self.config_path(self.home).exists())

        another_home = self.home / "nested-home"
        another_home.mkdir()
        another_runtime = another_home / "backup"
        another_runtime.mkdir()
        (another_runtime / "archive").mkdir()
        from_runtime = self.run_bk(another_home, "edit", cwd=another_runtime)
        self.assertNotEqual(from_runtime.returncode, 0)
        self.assertFalse((another_runtime / "backup.yaml").exists())

        working = self.home / "work"
        working.mkdir()
        (working / "home-link").symlink_to(self.home, target_is_directory=True)
        from_runtime_parent = self.run_bk(self.home, "edit", cwd=working)
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

    def test_non_tty_human_run_omits_backup_progress(self) -> None:
        source = self.home / "source"
        source.mkdir()
        (source / "data").write_text("progress", encoding="utf-8")
        self.write_config(self.home, [self.config_path(self.home), source])

        result = self.run_bk(self.home, "run")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("Backup ready", result.stdout)
        self.assertNotIn("Backup completed", result.stdout)
        self.assertRegex(result.stdout, r"Created: \d{4}-\d{2}-\d{2}T.*[+-]\d{2}:\d{2}")
        archive = self.archive_files(self.home)[0]
        log = self.log_for_archive(archive)
        self.assertIn(f"Archive: {archive}", result.stdout)
        self.assertIn(f"Log: {log}", result.stdout)
        self.assertNotIn("Current archive:", result.stdout)
        self.assertNotIn("Current log:", result.stdout)
        self.assertEqual(result.stdout.strip().splitlines()[-1], "Status: OK")

    def test_staging_progress_uses_recursive_plan_bytes_and_finishes_exactly(self) -> None:
        module = runpy.run_path(str(BK))
        source = self.home / "source"
        nested = source / "nested"
        nested.mkdir(parents=True)
        (source / "first.bin").write_bytes(b"a" * 19)
        (nested / "second.bin").write_bytes(b"b" * 23)
        (nested / "empty").touch()
        (source / "link").symlink_to("first.bin")
        config = module["Config"](version=1, retention=7, sources=[source])
        plan = module["build_snapshot_plan"](config)
        progress = RecordingProgress()
        paths = module["Paths"].from_home(self.home)
        paths.runtime.mkdir()

        with patch.dict(
            module["run_backup"].__globals__,
            {"build_snapshot_plan": lambda *_args, **_kwargs: plan},
        ):
            with module["backup_lock"](paths):
                code, _status = module["run_backup"](
                    paths,
                    config,
                    already_locked=True,
                    progress=progress,
                )

        self.assertEqual(code, 0)
        self.assertEqual(plan.input_bytes, 42)
        staged = next(
            record
            for record in progress.records
            if record["description"] == "Creating staged snapshot"
        )
        self.assertEqual(staged["total"], plan.input_bytes)
        self.assertTrue(staged["byte_progress"])
        self.assertEqual(staged["completed"], plan.input_bytes)
        values = staged["values"]
        assert isinstance(values, list)
        self.assertTrue(all(0 <= int(value) <= plan.input_bytes for value in values))

    def test_regular_file_copy_reports_incremental_bytes(self) -> None:
        module = runpy.run_path(str(BK))
        source = self.home / "large.bin"
        source.write_bytes(b"x" * (2 * 1024 * 1024 + 17))
        destination = self.home / "copied.bin"
        reported: list[int] = []

        module["copy_regular_file"](source, destination, reported.append)

        self.assertGreater(len(reported), 1)
        self.assertEqual(reported[-1], source.stat().st_size)
        self.assertEqual(reported, sorted(reported))
        self.assertEqual(destination.read_bytes(), source.read_bytes())

    def test_ordinary_file_replacement_and_retry_policy(self) -> None:
        for scenario in ("before", "after", "mutate", "recursive", "explicit", "missing", "sqlite"):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary)
                source = base / "source"
                source.mkdir()
                path = source / "session.json"
                original = b"a" * (1024 * 1024 + 1)
                path.write_bytes(original)
                module = runpy.run_path(str(BK))
                explicit = scenario == "explicit"
                config = module["Config"](1, 7, [path if explicit else source], False)
                plan = module["build_snapshot_plan"](config)
                if scenario in {"before", "sqlite"}:
                    replacement = base / "replacement"
                    if scenario == "sqlite":
                        with sqlite3.connect(replacement) as connection:
                            connection.execute("CREATE TABLE sample (value)")
                            connection.execute("INSERT INTO sample VALUES (42)")
                    else:
                        replacement.write_bytes(b"new version")
                    replacement.replace(path)
                if scenario == "missing":
                    path.unlink()
                run = base / "run"
                run.mkdir()
                progress = RecordingProgress()
                progress.stage("staging", total=plan.input_bytes)
                advance = progress.advance
                mutations = 0

                def update(amount: int) -> None:
                    nonlocal mutations
                    advance(amount)
                    if amount <= 0:
                        return
                    if scenario == "after" and mutations == 0:
                        replacement = base / "replacement"
                        replacement.write_bytes(b"new pathname")
                        replacement.replace(path)
                        mutations += 1
                    elif scenario in {"mutate", "recursive", "explicit"} and (
                        mutations == 0 or scenario != "mutate"
                    ):
                        metadata = path.stat()
                        with path.open("r+b") as stream:
                            stream.write(b"b")
                        os.utime(path, ns=(metadata.st_atime_ns, metadata.st_mtime_ns + 1000000))
                        mutations += 1

                progress.advance = update
                logs: list[str] = []
                with patch.object(module["time"], "sleep"):
                    if explicit:
                        with self.assertRaisesRegex(module["BKError"], "after 5 attempts"):
                            module["materialize_plan"](plan, run, logs, progress)
                        continue
                    payload, _ = module["materialize_plan"](plan, run, logs, progress)
                staged = payload / "0001" / "source" / path.name
                if scenario in {"recursive", "missing"}:
                    self.assertFalse(staged.exists())
                    self.assertTrue(any("Skipped unstable file after 5 attempts" in line for line in logs))
                elif scenario == "sqlite":
                    with sqlite3.connect(staged) as connection:
                        self.assertEqual(connection.execute("SELECT value FROM sample").fetchone(), (42,))
                        self.assertEqual(connection.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                    self.assertTrue(any("Classified SQLite" in line for line in logs))
                else:
                    expected = b"new version" if scenario == "before" else original
                    if scenario == "mutate":
                        expected = b"b" + original[1:]
                    self.assertEqual(staged.read_bytes(), expected)
                self.assertEqual(progress.current["completed"], plan.input_bytes)
                self.assertTrue(all(0 <= value <= plan.input_bytes for value in progress.current["values"]))
                if scenario == "mutate":
                    self.assertTrue(any(value < 0 for value in progress.current["advances"]))

    def test_overlapping_sources_are_allowed_and_independently_archived(self) -> None:
        dev = self.home / "DEV"
        parent = dev / "foo"
        parent.mkdir(parents=True)
        child = parent / "config.yaml"
        child.write_text("child content", encoding="utf-8")

        self.assertEqual(self.run_edit(self.home, dev, [1]), 0)
        self.assertEqual(self.run_edit(self.home, parent, [1, 2]), 0)
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
        self.assertFalse((self.backup_dir(self.home) / "last-failed.log").exists())
        self.assertEqual(result.stderr.strip().splitlines()[-1], "Status: FAILED")

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
        status = self.json_output(self.run_bk(self.home, "status", "--json"))
        self.assertEqual(self.status_value(status, "status"), "failed")
        self.assertIsNone(status["failed_log_path"])
        error = self.status_value(status, "error", "error_summary", "last_error")
        self.assertIn("data.txt", str(error))
        human = self.run_bk(self.home, "status")
        self.assertEqual(human.returncode, 0, human.stderr)
        self.assertIn("failed", human.stdout.lower())
        self.assertIn("successful", human.stdout.lower())
        run_output = failed.stdout + failed.stderr
        self.assertNotIn("Backup ready", run_output)
        self.assertNotIn("Backup completed", run_output)
        self.assertIn("Status: FAILED", run_output)

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

    def test_gitignore_false_preserves_unfiltered_traversal_without_git(self) -> None:
        repository = self.home / "repository"
        source = repository / "source"
        source.mkdir(parents=True)
        subprocess.run(["git", "-C", str(repository), "init", "-q"], check=True)
        (repository / ".gitignore").write_text("source/ignored.txt\n", encoding="utf-8")
        ignored = source / "ignored.txt"
        ignored.write_text("included", encoding="utf-8")
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source], False)

        def unexpected_git_call(_source: Path) -> Path | None:
            self.fail("respect_gitignore=false must not invoke Git")

        with patch.dict(
            module["build_snapshot_plan"].__globals__,
            {"git_worktree_root": unexpected_git_call},
        ):
            plan = module["build_snapshot_plan"](config)

        self.assertIn(ignored, {entry.source_path for entry in plan.sources[0].entries})

    def test_gitignore_filters_directory_plan_before_classification_and_byte_counting(self) -> None:
        repository = self.home / "repository"
        source = repository / "source"
        nested = source / "nested"
        ignored_directory = source / "ignored-directory"
        nested.mkdir(parents=True)
        ignored_directory.mkdir()
        subprocess.run(["git", "-C", str(repository), "init", "-q"], check=True)
        (repository / ".gitignore").write_text(
            "source/ignored.txt\nsource/ignored-directory/\nsource/tracked.txt\n",
            encoding="utf-8",
        )
        (nested / ".gitignore").write_text("*.tmp\n", encoding="utf-8")
        tracked = source / "tracked.txt"
        untracked = source / "untracked.txt"
        nested_kept = nested / "kept.txt"
        ignored = source / "ignored.txt"
        nested_ignored = nested / "ignored.tmp"
        ignored_child = ignored_directory / "secret.bin"
        tracked.write_bytes(b"tracked")
        untracked.write_bytes(b"untracked")
        nested_kept.write_bytes(b"nested")
        ignored.write_bytes(b"ignored" * 100)
        nested_ignored.write_bytes(b"nested ignored" * 100)
        ignored_child.write_bytes(b"directory ignored" * 100)
        subprocess.run(
            ["git", "-C", str(repository), "add", "-f", "source/tracked.txt"],
            check=True,
        )
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source], True)
        classified: list[Path] = []
        classify = module["classify_regular_file"]

        def record_classification(path: Path) -> object:
            classified.append(path)
            return classify(path)

        with patch.dict(
            module["build_snapshot_plan"].__globals__,
            {"classify_regular_file": record_classification},
        ):
            plan = module["build_snapshot_plan"](config)

        planned = {entry.source_path for entry in plan.sources[0].entries}
        included_files = {tracked, untracked, nested_kept, nested / ".gitignore"}
        self.assertTrue(included_files <= planned)
        self.assertTrue({ignored, nested_ignored, ignored_child}.isdisjoint(planned))
        self.assertTrue({ignored, nested_ignored, ignored_child}.isdisjoint(classified))
        self.assertEqual(
            plan.input_bytes,
            sum(path.stat().st_size for path in included_files),
        )

    def test_explicit_ignored_file_remains_an_independent_source(self) -> None:
        repository = self.home / "repository"
        source = repository / "source"
        source.mkdir(parents=True)
        subprocess.run(["git", "-C", str(repository), "init", "-q"], check=True)
        (repository / ".gitignore").write_text("source/ignored.txt\n", encoding="utf-8")
        ignored = source / "ignored.txt"
        ignored.write_text("explicit", encoding="utf-8")
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source, ignored], True)

        plan = module["build_snapshot_plan"](config)

        self.assertNotIn(ignored, {entry.source_path for entry in plan.sources[0].entries})
        self.assertEqual([entry.source_path for entry in plan.sources[1].entries], [ignored])
        run_directory = self.home / "run"
        run_directory.mkdir()
        payload, _log = module["materialize_plan"](plan, run_directory)
        self.assertEqual((payload / "0002" / "ignored.txt").read_text(encoding="utf-8"), "explicit")

    def test_git_plan_never_descends_through_a_replaced_directory_symlink(self) -> None:
        repository = self.home / "repository"
        source = repository / "source"
        tracked_directory = source / "tracked"
        tracked_directory.mkdir(parents=True)
        tracked_file = tracked_directory / "secret.txt"
        tracked_file.write_text("indexed", encoding="utf-8")
        subprocess.run(["git", "-C", str(repository), "init", "-q"], check=True)
        subprocess.run(
            ["git", "-C", str(repository), "add", "source/tracked/secret.txt"],
            check=True,
        )
        tracked_file.unlink()
        tracked_directory.rmdir()
        outside = self.home / "outside"
        outside.mkdir()
        sentinel = outside / "secret.txt"
        sentinel.write_text("must survive", encoding="utf-8")
        tracked_directory.symlink_to(outside, target_is_directory=True)
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source], True)

        plan = module["build_snapshot_plan"](config)
        run_directory = self.home / "run"
        run_directory.mkdir()
        module["materialize_plan"](plan, run_directory)

        planned = {entry.source_path for entry in plan.sources[0].entries}
        self.assertIn(tracked_directory, planned)
        self.assertNotIn(tracked_directory / "secret.txt", planned)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "must survive")

    def test_git_plan_rejects_source_changed_to_symlink_during_detection(self) -> None:
        repository = self.home / "repository"
        source = repository / "source"
        source.mkdir(parents=True)
        subprocess.run(["git", "-C", str(repository), "init", "-q"], check=True)
        outside = self.home / "outside"
        outside.mkdir()
        sentinel = outside / "sentinel"
        sentinel.write_text("must survive", encoding="utf-8")
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source], True)

        def replace_source(_source: Path) -> Path:
            source.rmdir()
            source.symlink_to(outside, target_is_directory=True)
            return repository

        with patch.dict(
            module["build_snapshot_plan"].__globals__,
            {"git_worktree_root": replace_source},
        ):
            with self.assertRaises(module["BKError"]):
                module["build_snapshot_plan"](config)

        self.assertEqual(sentinel.read_text(encoding="utf-8"), "must survive")

    def test_non_git_directory_uses_normal_traversal_when_gitignore_is_enabled(self) -> None:
        source = self.home / "source"
        nested = source / "nested"
        nested.mkdir(parents=True)
        included = nested / "data.txt"
        included.write_text("included", encoding="utf-8")
        module = runpy.run_path(str(BK))
        config = module["Config"](1, 7, [source], True)

        plan = module["build_snapshot_plan"](config)

        self.assertIn(included, {entry.source_path for entry in plan.sources[0].entries})

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

    def test_sqlite_progress_reaches_exact_planned_byte_weight(self) -> None:
        database = self.home / "database"
        with sqlite3.connect(database) as connection:
            connection.execute("CREATE TABLE data (value BLOB)")
            connection.execute("INSERT INTO data VALUES (zeroblob(2097152))")
            connection.commit()
        module = runpy.run_path(str(BK))
        destination = self.home / "staged"
        planned_size = database.stat().st_size
        reported: list[int] = []

        module["copy_sqlite_file"](
            database,
            destination,
            [],
            planned_size,
            reported.append,
        )

        self.assertGreater(len(reported), 1)
        self.assertEqual(reported[-1], planned_size)
        self.assertTrue(all(0 <= value <= planned_size for value in reported))

    def test_sqlite_retry_rolls_back_progress_before_counting_retry(self) -> None:
        database = self.home / "database"
        with sqlite3.connect(database) as connection:
            connection.execute("CREATE TABLE data (value TEXT)")
            connection.commit()
        module = runpy.run_path(str(BK))
        config = module["Config"](version=1, retention=7, sources=[database])
        plan = module["build_snapshot_plan"](config)
        run_directory = self.home / "run"
        run_directory.mkdir()
        progress = RecordingProgress()
        progress.stage("Creating staged snapshot", total=plan.input_bytes, byte_progress=True)
        attempts = 0

        def copy_attempt(
            _source: Path,
            destination: Path,
            _log_lines: list[str],
            planned_size: int,
            report_bytes: object,
        ) -> None:
            nonlocal attempts
            attempts += 1
            destination.write_bytes(b"staged")
            assert callable(report_bytes)
            report_bytes(planned_size)
            if attempts == 1:
                raise module["_RetryableSQLiteSnapshot"]("retry once")

        with patch.dict(
            module["copy_sqlite_file"].__globals__,
            {"_copy_sqlite_attempt": copy_attempt},
        ):
            module["materialize_plan"](plan, run_directory, progress=progress)

        self.assertEqual(attempts, 2)
        assert progress.current is not None
        self.assertEqual(progress.current["completed"], plan.input_bytes)
        advances = progress.current["advances"]
        values = progress.current["values"]
        assert isinstance(advances, list)
        assert isinstance(values, list)
        self.assertIn(-plan.input_bytes, advances)
        self.assertTrue(all(0 <= int(value) <= plan.input_bytes for value in values))

    def test_closed_wal_database_change_during_backup_retries_and_succeeds(self) -> None:
        database = self.home / "closed-state"
        connection, row_count = self.create_wal_database(database)
        connection.close()
        module = runpy.run_path(str(BK))
        real_connect = sqlite3.connect
        mutated = False

        class MutatingSourceConnection:
            def __init__(self, wrapped: sqlite3.Connection) -> None:
                self.wrapped = wrapped

            def backup(self, destination: sqlite3.Connection, **kwargs: object) -> None:
                nonlocal mutated
                self.wrapped.backup(destination, **kwargs)
                if not mutated:
                    mutated = True
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
        log_lines: list[str] = []
        with patch.object(module["sqlite3"], "connect", side_effect=connect):
            module["copy_sqlite_file"](
                database,
                destination,
                log_lines,
                database.stat().st_size,
            )

        self.assertTrue(mutated)
        self.assertIn("retry", "\n".join(log_lines).lower())
        with sqlite3.connect(destination) as checked:
            self.assertEqual(checked.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            self.assertEqual(checked.execute("SELECT count(*) FROM records").fetchone()[0], row_count + 1)

    def test_closed_wal_backup_error_after_source_change_is_retried(self) -> None:
        database = self.home / "closed-state"
        connection, row_count = self.create_wal_database(database)
        connection.close()
        module = runpy.run_path(str(BK))
        real_connect = sqlite3.connect
        changed = False

        class FailingChangedSourceConnection:
            def __init__(self, wrapped: sqlite3.Connection) -> None:
                self.wrapped = wrapped

            def backup(self, destination: sqlite3.Connection, **kwargs: object) -> None:
                nonlocal changed
                if not changed:
                    changed = True
                    with real_connect(database) as writer:
                        writer.execute("INSERT INTO records(value) VALUES ('changed before failure')")
                        writer.commit()
                    raise sqlite3.DatabaseError("source changed during immutable backup")
                self.wrapped.backup(destination, **kwargs)

            def close(self) -> None:
                self.wrapped.close()

        def connect(target: str, *args: object, **kwargs: object) -> sqlite3.Connection | FailingChangedSourceConnection:
            opened = real_connect(target, *args, **kwargs)
            if kwargs.get("uri") and "immutable=1" in target:
                return FailingChangedSourceConnection(opened)
            return opened

        destination = self.home / "staged"
        with patch.object(module["sqlite3"], "connect", side_effect=connect):
            module["copy_sqlite_file"](
                database,
                destination,
                [],
                database.stat().st_size,
            )

        with sqlite3.connect(destination) as checked:
            self.assertEqual(checked.execute("PRAGMA integrity_check").fetchone()[0], "ok")
            self.assertEqual(checked.execute("SELECT count(*) FROM records").fetchone()[0], row_count + 1)

    def test_stable_sqlite_backup_error_is_not_retried(self) -> None:
        database = self.home / "closed-state"
        connection, _ = self.create_wal_database(database)
        connection.close()
        module = runpy.run_path(str(BK))
        destination = self.home / "staged"

        with patch.object(module["time"], "sleep") as sleep:
            with patch.object(
                module["sqlite3"],
                "connect",
                side_effect=sqlite3.DatabaseError("stable read failure"),
            ):
                with self.assertRaises(module["BKError"]) as raised:
                    module["copy_sqlite_file"](
                        database,
                        destination,
                        [],
                        database.stat().st_size,
                    )

        self.assertIn("stable read failure", str(raised.exception))
        sleep.assert_not_called()

    def test_closed_wal_database_retry_exhaustion_is_bounded_and_cleans_staging(self) -> None:
        database = self.home / "closed-state"
        connection, _ = self.create_wal_database(database)
        connection.close()
        module = runpy.run_path(str(BK))
        retry = module["_RetryableSQLiteSnapshot"]("transient WAL state")
        destination = self.home / "staged"
        log_lines: list[str] = []
        policy_calls = 0

        def reject_policy(*_args: object, **_kwargs: object) -> object:
            nonlocal policy_calls
            policy_calls += 1
            raise retry

        with patch.object(module["time"], "sleep") as sleep:
            with patch.dict(module["copy_sqlite_file"].__globals__, {"sqlite_open_policy": reject_policy}):
                with self.assertRaises(module["BKError"]) as raised:
                    module["copy_sqlite_file"](
                        database,
                        destination,
                        log_lines,
                        database.stat().st_size,
                    )

        self.assertIn("5 attempts", str(raised.exception))
        self.assertEqual(policy_calls, 5)
        self.assertEqual(sleep.call_count, 4)
        self.assertEqual(len([line for line in log_lines if "retry" in line.lower()]), 5)
        self.assertFalse(destination.exists())

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
            "relative source": (
                'version: 1\nretention: 7\nrespect_gitignore: true\nsources:\n  - "relative/path"\n'
            ),
            "self is not first": (
                "version: 1\nretention: 7\nrespect_gitignore: true\nsources:\n"
                "  - %s\n  - %s\n"
                % (encoded_source, self_source)
            ),
            "missing sources": (
                "version: 1\nretention: 7\nrespect_gitignore: true\nsources: []\n"
            ),
            "source contains runtime": (
                "version: 1\nretention: 7\nrespect_gitignore: true\nsources:\n"
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
                status_path = backup / "status.json"
                self.assertFalse((backup / "last-failed.log").exists())
                recorded = json.loads(status_path.read_text(encoding="utf-8"))
                self.assertEqual(recorded["status"], "failed")
                self.assertIsNone(recorded["failed_log_path"])
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


class SelectorTests(unittest.TestCase):
    """Deterministic tests for the unified source editor state machine."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.module = runpy.run_path(str(BK))
        cls.item = cls.module["SelectorItem"]
        cls.group = cls.module["SelectorGroup"]

    def items(self):
        return [
            self.item(1, ".agents/", "directory", path=Path("/work/.agents")),
            self.item(2, ".codex/", "directory", path=Path("/work/.codex")),
            self.item(3, "config.yaml", "file", path=Path("/work/config.yaml")),
            self.item(4, "backup/", "directory", False, "BK-managed; not selectable", Path("/work/backup")),
            self.item(5, "~/elsewhere/", "directory", path=Path("/home/user/elsewhere"), group="elsewhere"),
        ]

    def groups(self):
        return [
            self.group("current", "Current directory", True),
            self.group("elsewhere", "Configured elsewhere", False),
        ]

    def state(self, selected=None):
        return self.module["SelectorState"](
            self.items(), groups=self.groups(), selected_indices=set(selected or ())
        )

    def run_selector(self, keys, selected=None):
        return self.module["run_edit_selector"](
            self.items(), self.groups(), set(selected or ()), context=Path("/work"), curses_module=_FakeCurses(keys=keys)
        )

    def test_edit_commands_are_represented_by_one_state(self) -> None:
        state = self.state()
        self.assertEqual([group.expanded for group in state.groups], [True, False])
        self.assertEqual(state.focus_index, 1)
        state.move_focus(1)
        self.assertTrue(state.toggle())
        self.assertEqual(state.selected_indices, {2})
        self.assertEqual(state.selection_text(), "2")
        self.assertEqual(state.selected_count, 1)

    def test_typed_numeric_forms_update_checkboxes_and_canonical_text(self) -> None:
        for raw in ("1,2,3", "1 2 3", "1, 2, 3", "1 2,3"):
            with self.subTest(raw=raw):
                state = self.state()
                self.assertTrue(state.edit_selection_text(raw))
                self.assertEqual(state.selected_indices, {1, 2, 3})
                self.assertEqual(state.selection_text(), "1,2,3")

    def test_numeric_parser_rejects_non_separator_text_without_changing_selection(self) -> None:
        for raw in ("1x2", "-1", "1.2", "abc", "1,,2", "1,"):
            with self.subTest(raw=raw):
                state = self.state(selected={2, 3})
                self.assertFalse(state.edit_selection_text(raw))
                self.assertEqual(state.selected_indices, {2, 3})
                self.assertEqual(state.selection_text(), "2,3")
                self.assertIn("commas or spaces", state.error or "")

    def test_protected_and_runtime_rows_cannot_be_selected(self) -> None:
        state = self.state()
        self.assertFalse(state.edit_selection_text("4"))
        self.assertEqual(state.selected_indices, set())
        self.assertIn("non-selectable", state.error or "")
        self.assertFalse(state.toggle(4))

        protected = self.item(
            None,
            "backup.yaml",
            "file",
            False,
            "protected self-entry",
            Path("/home/user/backup/backup.yaml"),
        )
        protected_state = self.module["SelectorState"](
            [protected], groups=self.groups(), selected_indices=set()
        )
        fake_curses = _FakeCurses()
        self.module["_selector_render"](
            fake_curses.screen, protected_state, Path("/home/user/backup"), 0, fake_curses
        )
        rendered = "\n".join(value for _row, value, _width, _attribute in fake_curses.screen.lines)
        self.assertIn("[x]", rendered)
        self.assertIn("protected self-entry", rendered)
        self.assertNotIn(". backup.yaml", rendered)

    def test_configured_elsewhere_stays_selected_when_collapsed_and_numbered_stably(self) -> None:
        state = self.state(selected={5})
        self.assertEqual(state.selection_text(), "5")
        before = [item.index for item in state.items]
        state.toggle_group("elsewhere")
        self.assertEqual([item.index for item in state.items], before)
        self.assertEqual(state.selection_text(), "5")
        self.assertIn(("item", 5), state.visible_nodes())
        state.toggle_group("elsewhere")
        self.assertEqual(state.selection_text(), "5")

    def test_filter_matches_path_kind_and_state_without_changing_editor_state(self) -> None:
        state = self.state(selected={5})
        before_numbers = [item.index for item in state.items]
        before_expanded = [group.expanded for group in state.groups]

        state.set_filter_text("CONFIG.YAML")
        self.assertEqual(state.filtered_item_indices(), [3])
        state.set_filter_text("DIRECTORY")
        self.assertEqual(state.filtered_item_indices(), [1, 2, 4, 5])
        state.set_filter_text("BK-MANAGED")
        self.assertEqual(state.filtered_item_indices(), [4])

        self.assertEqual(state.selected_indices, {5})
        self.assertEqual([item.index for item in state.items], before_numbers)
        self.assertEqual([group.expanded for group in state.groups], before_expanded)
        self.assertIn(("item", 4), state.visible_nodes())

    def test_filter_preserves_collapsed_groups_until_the_user_expands_them(self) -> None:
        state = self.state(selected={5})
        self.assertFalse(state.groups[1].expanded)
        state.set_filter_text("elsewhere")
        self.assertNotIn(("item", 5), state.visible_nodes())
        self.assertFalse(state.groups[1].expanded)
        state.expand_group("elsewhere")
        self.assertIn(("item", 5), state.visible_nodes())
        state.clear_filter()
        self.assertIn(("item", 5), state.visible_nodes())
        self.assertTrue(state.groups[1].expanded)

    def test_filter_no_matches_are_reported_and_clearing_restores_visibility(self) -> None:
        state = self.state(selected={2})
        state.set_filter_text("does-not-exist")
        self.assertEqual(state.filtered_item_indices(), [])
        fake_curses = _FakeCurses(width=100, height=16)
        self.module["_selector_render"](
            fake_curses.screen, state, Path("/work"), 0, fake_curses
        )
        rendered = "\n".join(value for _row, value, _width, _attribute in fake_curses.screen.lines)
        self.assertIn("No matches for filter: does-not-exist", rendered)
        state.clear_filter()
        self.assertIn(("item", 1), state.visible_nodes())
        self.assertEqual(state.selected_indices, {2})

    def test_parent_and_child_entries_remain_independent(self) -> None:
        state = self.state()
        state.set_selection({1, 3})
        self.assertEqual(state.selection_text(), "1,3")
        state.toggle(1)
        self.assertEqual(state.selected_indices, {3})

    def test_mouse_row_and_group_toggles_use_the_same_selection_state(self) -> None:
        state = self.state()
        fake_curses = _FakeCurses()
        scroll = self.module["_selector_mouse_event"](
            fake_curses.screen, state, 1, 6, fake_curses.BUTTON1_PRESSED, 0, 6, fake_curses
        )
        self.assertEqual(scroll, 0)
        self.assertEqual(state.selected_indices, {2})
        self.assertEqual(state.input_buffer, "2")
        self.module["_selector_mouse_event"](
            fake_curses.screen, state, 1, 4, fake_curses.BUTTON1_PRESSED, 0, 6, fake_curses
        )
        self.assertFalse(state.groups[0].expanded)
        self.assertEqual(state.selected_indices, {2})

    def test_mouse_filter_click_activates_filter_and_row_math_uses_framed_layout(self) -> None:
        state = self.state()
        fake_curses = _FakeCurses(height=16)
        list_top, input_row, visible = self.module["_selector_visible_window"](state, 16)
        self.assertEqual(list_top, 4)
        self.assertEqual(input_row, 14)
        self.assertEqual(visible, 10)
        self.module["_selector_mouse_event"](
            fake_curses.screen, state, 1, 2, fake_curses.BUTTON1_PRESSED, 0, visible, fake_curses
        )
        self.assertTrue(state.filter_active)
        self.module["_selector_mouse_event"](
            fake_curses.screen, state, 1, list_top + 1, fake_curses.BUTTON1_PRESSED, 0, visible, fake_curses
        )
        self.assertFalse(state.filter_active)
        self.assertEqual(state.selected_indices, {1})

    def test_mouse_filter_hitbox_matches_the_frame_and_short_layout(self) -> None:
        state = self.state()
        normal = _FakeCurses(height=16, width=120)
        frame_width = self.module["_selector_filter_width"](state, 120)
        self.module["_selector_mouse_event"](
            normal.screen,
            state,
            frame_width + 1,
            2,
            normal.BUTTON1_PRESSED,
            0,
            10,
            normal,
        )
        self.assertFalse(state.filter_active)

        short_state = self.state()
        short = _FakeCurses(height=6, width=80)
        self.module["_selector_mouse_event"](
            short.screen,
            short_state,
            1,
            2,
            short.BUTTON1_PRESSED,
            0,
            3,
            short,
        )
        self.assertFalse(short_state.filter_active)
        self.assertEqual(short_state.selected_indices, {1})

    def test_typed_key_sequences_and_space_toggle_remain_synchronized(self) -> None:
        cases = (
            ([ord("1"), ord(","), ord("2"), 10], [1, 2]),
            ([ord("1"), ord(" "), ord("2"), 10], [1, 2]),
            ([ord("1"), ord(","), ord(" "), ord("3"), 10], [1, 3]),
            ([ord("j"), ord(" "), 10], [2]),
        )
        for keys, expected in cases:
            with self.subTest(keys=keys):
                result = self.run_selector(keys)
                self.assertEqual(result.selected_indices, expected)

    def test_invalid_typed_numbers_do_not_corrupt_the_current_selection(self) -> None:
        invalid_first = self.run_selector(
            [ord("9"), _FakeCurses.KEY_DOWN, 10], selected={2, 3}
        )
        self.assertEqual(invalid_first.selected_indices, [2, 3])

        invalid_continuation = self.run_selector(
            [ord("1"), ord("9"), _FakeCurses.KEY_DOWN, 10]
        )
        self.assertEqual(invalid_continuation.selected_indices, [1])

    def test_stable_multi_digit_numbers_include_collapsed_entries(self) -> None:
        items = [
            self.item(index, f"item-{index}", path=Path(f"/work/item-{index}"))
            for index in range(1, 41)
        ]
        groups = [
            self.group("current", "Current directory", True),
            self.group("elsewhere", "Configured elsewhere", False),
        ]
        result = self.module["run_edit_selector"](
            items,
            groups,
            set(),
            context=Path("/work"),
            curses_module=_FakeCurses(keys=[ord("3"), ord(","), ord("1"), ord("2"), 10]),
        )
        self.assertEqual(result.selected_indices, [3, 12])

    def test_header_rows_render_synchronized_count_and_directory_slashes(self) -> None:
        protected = self.item(
            None,
            "backup.yaml",
            "file",
            False,
            "protected self-entry",
            Path("/home/user/backup/backup.yaml"),
        )
        state = self.module["SelectorState"](
            [*self.items(), protected],
            groups=self.groups(),
            selected_indices={1, 5},
        )
        fake_curses = _FakeCurses(width=80, height=16)
        self.module["_selector_render"](fake_curses.screen, state, Path("/work"), 0, fake_curses)
        rendered = "\n".join(value for _row, value, _width, _attribute in fake_curses.screen.lines)
        self.assertIn("Configured 2", rendered)
        self.assertIn("Current directory  1/3", rendered)
        self.assertIn(".agents/", rendered)
        self.assertIn("▶ Configured elsewhere  1/1", rendered)
        self.assertIn("Selection: 1,5", rendered)
        self.assertIn("Enter Apply", rendered)
        self.assertIn("Esc/Ctrl-Q Cancel", rendered)

    def test_filtered_connectors_mark_the_last_visible_child(self) -> None:
        state = self.state()
        state.set_filter_text("directory")
        fake_curses = _FakeCurses(width=100, height=16)
        self.module["_selector_render"](
            fake_curses.screen, state, Path("/work"), 0, fake_curses
        )
        rendered = "\n".join(value for _row, value, _width, _attribute in fake_curses.screen.lines)
        self.assertIn("├── [ ]  1. .agents/", rendered)
        self.assertIn("└── [ ]  4. backup/", rendered)
        self.assertNotIn("├── [ ]  3. config.yaml", rendered)
        self.assertNotIn("│  [", rendered)

    def test_filter_frame_is_compact_and_footer_keys_are_styled_separately(self) -> None:
        state = self.state(selected={1, 5})
        fake_curses = _FakeCurses(width=120, height=16)
        frame_width = self.module["_selector_filter_width"](state, 120)
        self.assertLess(frame_width, 120)
        self.module["_selector_render"](
            fake_curses.screen, state, Path("/work"), 0, fake_curses
        )
        frame_rows = [
            (row, value, width, attribute)
            for row, value, width, attribute in fake_curses.screen.lines
            if row in {1, 2, 3}
        ]
        self.assertTrue(any(value.startswith("╭") and width == frame_width for _row, value, width, _attribute in frame_rows))
        self.assertTrue(any(value.startswith("╰") and width == frame_width for _row, value, width, _attribute in frame_rows))
        self.assertTrue(any(value.startswith("↑↓ Move") and attribute == fake_curses.A_DIM for _row, value, _width, attribute in fake_curses.screen.lines))
        self.assertTrue(any(value == "↑↓" and attribute == fake_curses.A_BOLD for _row, value, _width, attribute in fake_curses.screen.lines))
        self.assertTrue(any(value == "Selection:" and attribute == fake_curses.A_BOLD for _row, value, _width, attribute in fake_curses.screen.lines))

    def test_filter_keyboard_editing_and_escape_clear_before_cancel(self) -> None:
        state = self.state(selected={2})
        state.activate_filter()
        state.append_filter("Data")
        state.backspace_filter()
        self.assertEqual(state.filter_text, "Dat")
        state.clear_filter()
        self.assertEqual(state.filter_text, "")
        self.assertEqual(state.selected_indices, {2})

        result = self.run_selector([ord("/"), ord("x"), 27, 10], selected=[2])
        self.assertFalse(result.cancelled)
        self.assertEqual(result.selected_indices, [2])
        cancelled = self.run_selector([ord("/"), ord("x"), 27, 27], selected=[2])
        self.assertTrue(cancelled.cancelled)

    def test_filter_accepts_unicode_terminal_input(self) -> None:
        fake_curses = _FakeCurses(keys=["/", "Ą", "ć", "\x1b", "\n"])
        result = self.module["run_edit_selector"](
            self.items(),
            self.groups(),
            {2},
            context=Path("/work"),
            curses_module=fake_curses,
        )
        self.assertFalse(result.cancelled)
        self.assertEqual(result.selected_indices, [2])
        rendered = "\n".join(
            value for _row, value, _width, _attribute in fake_curses.screen.lines
        )
        self.assertIn("Filter (/): Ąć", rendered)

    def test_enter_applies_and_cancel_keys_restore_terminal_state(self) -> None:
        applied = self.run_selector([ord("j"), ord(" "), 10])
        self.assertEqual(applied.selected_indices, [2])
        self.assertFalse(applied.cancelled)
        for key in (27, 17, 3, KeyboardInterrupt()):
            with self.subTest(key=key):
                fake_curses = _FakeCurses(keys=[key])
                result = self.module["run_edit_selector"](
                    self.items(), self.groups(), set(), context=Path("/work"), curses_module=fake_curses
                )
                self.assertTrue(result.cancelled)
                self.assertEqual(fake_curses.mouse_masks[-1], 0)
                self.assertIn(False, fake_curses.screen.keypad_values)
                self.assertGreaterEqual(fake_curses.endwin_calls, 1)
                self.assertEqual(fake_curses.cursor_visibility, 2)
                self.assertEqual(fake_curses.raw_calls, 1)
                self.assertEqual(fake_curses.noraw_calls, 1)

    def test_horizontal_group_controls_preserve_selection(self) -> None:
        state = self.state(selected={5})
        state.focus_node = ("group", "elsewhere")
        state.horizontal(1)
        self.assertTrue(state.groups[1].expanded)
        state.horizontal(-1)
        self.assertFalse(state.groups[1].expanded)
        self.assertEqual(state.selected_indices, {5})

    def test_exception_restores_terminal_state(self) -> None:
        fake_curses = _FakeCurses(keys=[RuntimeError("selector failure")])
        with self.assertRaises(RuntimeError):
            self.module["run_edit_selector"](
                self.items(), self.groups(), set(), context=Path("/work"), curses_module=fake_curses
            )
        self.assertEqual(fake_curses.mouse_masks[-1], 0)
        self.assertIn(False, fake_curses.screen.keypad_values)
        self.assertGreaterEqual(fake_curses.endwin_calls, 1)
        self.assertEqual(fake_curses.cursor_visibility, 2)
        self.assertEqual(fake_curses.raw_calls, 1)
        self.assertEqual(fake_curses.noraw_calls, 1)


class _FakeScreen:
    def __init__(self, keys: list[object] | None = None, *, height: int = 12, width: int = 100) -> None:
        self.keys = list(keys or [])
        self.height = height
        self.width = width
        self.lines: list[tuple[int, str, int, int]] = []
        self.keypad_values: list[bool] = []
        self.moves: list[tuple[int, int]] = []

    def getmaxyx(self) -> tuple[int, int]:
        return (self.height, self.width)

    def erase(self) -> None:
        pass

    def addnstr(self, row: int, _column: int, value: str, width: int, attribute: int = 0) -> None:
        self.lines.append((row, value, width, attribute))

    def refresh(self) -> None:
        pass

    def getch(self) -> object:
        if not self.keys:
            raise AssertionError("fake selector input was exhausted")
        key = self.keys.pop(0)
        if isinstance(key, BaseException):
            raise key
        return key

    def get_wch(self) -> object:
        return self.getch()

    def keypad(self, enabled: bool) -> None:
        self.keypad_values.append(enabled)

    def move(self, row: int, column: int) -> None:
        self.moves.append((row, column))


class _FakeCurses:
    KEY_UP = 1001
    KEY_DOWN = 1002
    KEY_LEFT = 260
    KEY_RIGHT = 261
    KEY_HOME = 1003
    KEY_END = 1004
    KEY_PPAGE = 1005
    KEY_NPAGE = 1006
    KEY_BACKSPACE = 263
    KEY_DC = 1008
    KEY_MOUSE = 1009
    KEY_BTAB = 1010
    BUTTON1_CLICKED = 1
    BUTTON1_PRESSED = 2
    BUTTON4_PRESSED = 4
    BUTTON5_PRESSED = 8
    A_BOLD = 1
    A_REVERSE = 2
    A_DIM = 4

    def __init__(self, keys: list[object] | None = None, *, height: int = 12, width: int = 100) -> None:
        self.screen = _FakeScreen(keys, height=height, width=width)
        self.mouse_masks: list[int] = []
        self.endwin_calls = 0
        self.cursor_visibility = 2
        self.raw_calls = 0
        self.noraw_calls = 0

    def initscr(self) -> _FakeScreen:
        return self.screen

    def noecho(self) -> None:
        pass

    def echo(self) -> None:
        pass

    def raw(self) -> None:
        self.raw_calls += 1

    def noraw(self) -> None:
        self.noraw_calls += 1

    def mousemask(self, mask: int) -> None:
        self.mouse_masks.append(mask)

    def mouseinterval(self, _interval: int) -> None:
        pass

    def curs_set(self, visible: int) -> int:
        previous = self.cursor_visibility
        self.cursor_visibility = visible
        return previous

    def endwin(self) -> None:
        self.endwin_calls += 1


if __name__ == "__main__":
    unittest.main()
