#!/usr/bin/env python3
"""BK: a small, workspace-local backup utility.

The file is deliberately self-contained.  It is copied to a workspace as the
``bk`` executable and therefore must not import HomeStack modules or rely on
the source checkout.
"""

from __future__ import annotations

import errno
import fcntl
import json
import os
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import traceback
from contextlib import contextmanager, nullcontext
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator
from urllib.parse import quote

try:
    from rich.console import Console
    from rich.progress import (
        BarColumn,
        Progress,
        SpinnerColumn,
        TaskProgressColumn,
        TextColumn,
        TimeElapsedColumn,
    )
    from rich.table import Table
except ImportError as exc:  # pragma: no cover - the guest contract provides Rich
    raise SystemExit("BK requires the Rich Python package") from exc


CONFIG_VERSION = 1
DEFAULT_RETENTION = 7
STATUS_VERSION = 1
SQLITE_SIDECAR_SUFFIXES = ("-wal", "-shm", "-journal")
ARCHIVE_RE = re.compile(
    r"^backup-(?P<stamp>\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2})"
    r"(?:-(?P<collision>\d+))?\.tgz$"
)

console = Console()
error_console = Console(stderr=True)


class BKError(Exception):
    """An expected user-facing BK failure."""


class ConfigError(BKError):
    """The user's backup configuration is malformed or unusable."""


class BackupAlreadyRunning(BKError):
    """Another BK process currently owns the run lock."""


@dataclass(frozen=True)
class Paths:
    """All BK-managed paths for one home directory."""

    home: Path
    runtime: Path
    config: Path
    current_archive: Path
    current_log: Path
    status: Path
    failed_log: Path
    archive: Path
    work: Path
    lock: Path

    @classmethod
    def from_home(cls, home: Path | None = None) -> "Paths":
        selected_home = home or Path.home()
        selected_home = Path(os.path.abspath(os.path.normpath(str(selected_home))))
        runtime = selected_home / "backup"
        return cls(
            home=selected_home,
            runtime=runtime,
            config=runtime / "backup.yaml",
            current_archive=runtime / "backup.tgz",
            current_log=runtime / "backup.log",
            status=runtime / "status.json",
            failed_log=runtime / "last-failed.log",
            archive=runtime / "archive",
            work=runtime / ".work",
            lock=runtime / ".bk.lock",
        )


@dataclass
class Config:
    version: int
    retention: int
    sources: list[Path]


@dataclass
class ClassResult:
    """The one classifier result associated with a regular source file."""

    handler: str
    output: str


@dataclass
class PlanEntry:
    source_index: int
    source_path: Path
    relative: Path
    kind: str
    device: int
    inode: int
    size: int
    modified_ns: int
    classification: ClassResult | None = None
    skip_sidecar: bool = False

    @property
    def is_regular(self) -> bool:
        return self.kind == "file"


@dataclass
class SourcePlan:
    index: int
    source: Path
    root_kind: str
    entries: list[PlanEntry] = field(default_factory=list)


@dataclass
class PlanSummary:
    sources: list[SourcePlan]
    file_count: int
    directory_count: int
    symlink_count: int
    input_bytes: int
    skipped_sidecars: int
    classifications: int
    sqlite_count: int


@dataclass
class ArchivePair:
    archive: Path
    log: Path
    stamp: str


class BackupProgress:
    """Render live backup phases for a human-facing run."""

    def __init__(self) -> None:
        self._progress = Progress(
            SpinnerColumn(),
            TextColumn("{task.description}"),
            BarColumn(),
            TaskProgressColumn(),
            TimeElapsedColumn(),
            console=console,
        )
        self._task_id: int | None = None

    def __enter__(self) -> BackupProgress:
        self._progress.start()
        return self

    def __exit__(self, _exc_type: object, _exc: object, _traceback: object) -> None:
        self._progress.stop()

    def stage(self, description: str, *, total: int | None = None) -> None:
        if self._task_id is not None:
            self._progress.remove_task(self._task_id)
        self._task_id = self._progress.add_task(description, total=total)

    def advance(self) -> None:
        if self._task_id is not None:
            self._progress.advance(self._task_id)


def normalize_path(path: str | os.PathLike[str]) -> Path:
    """Return an absolute lexical path without resolving symlinks."""

    raw = os.fspath(path)
    if "\x00" in raw:
        raise BKError("path contains a NUL byte")
    return Path(os.path.abspath(os.path.normpath(raw)))


def path_is_within(path: Path, parent: Path) -> bool:
    """Whether *path* is *parent* or one of its descendants."""

    try:
        return os.path.commonpath((str(path), str(parent))) == str(parent)
    except ValueError:
        return False


def lexically_real(path: Path) -> Path:
    return normalize_path(os.path.realpath(path))


def path_is_runtime(path: Path, paths: Paths) -> bool:
    """Reject sources that are within, or would contain, BK's runtime tree."""

    candidate = normalize_path(path)
    runtime = normalize_path(paths.runtime)
    if path_is_within(candidate, runtime) or path_is_within(runtime, candidate):
        return True
    real_candidate = lexically_real(candidate)
    real_runtime = lexically_real(runtime)
    return path_is_within(real_candidate, real_runtime) or path_is_within(real_runtime, real_candidate)


def ensure_runtime_directory(paths: Paths) -> None:
    """Create the runtime directory, refusing symlinked or non-directory roots."""

    if os.path.lexists(paths.runtime):
        if paths.runtime.is_symlink():
            raise BKError(f"BK runtime path is a symlink, refusing to use it: {paths.runtime}")
        if not paths.runtime.is_dir():
            raise BKError(f"BK runtime path is not a directory: {paths.runtime}")
    else:
        try:
            paths.runtime.mkdir(mode=0o700)
        except OSError as exc:
            raise BKError(f"cannot create BK runtime directory {paths.runtime}: {exc}") from exc


def ensure_archive_and_work_directories(paths: Paths) -> None:
    ensure_runtime_directory(paths)
    for directory, label in ((paths.archive, "archive"), (paths.work, "work")):
        if os.path.lexists(directory):
            if directory.is_symlink() or not directory.is_dir():
                raise BKError(f"BK {label} path is not a real directory: {directory}")
        else:
            try:
                directory.mkdir(mode=0o700)
            except OSError as exc:
                raise BKError(f"cannot create BK {label} directory {directory}: {exc}") from exc


def atomic_write_bytes(path: Path, data: bytes, mode: int | None = None) -> None:
    """Write one managed file using a same-directory temporary and replace."""

    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        raise BKError(f"managed file parent is not a real directory: {parent}")
    temporary: Path | None = None
    try:
        descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=parent)
        temporary = Path(temporary_name)
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        if mode is not None:
            os.chmod(temporary, mode)
        os.replace(temporary, path)
        temporary = None
        try:
            directory_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        except OSError:
            directory_fd = -1
        if directory_fd >= 0:
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    except OSError as exc:
        raise BKError(f"cannot atomically write {path}: {exc}") from exc
    finally:
        if temporary is not None:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def parse_config_text(text: str, config_path: Path, paths: Paths) -> Config:
    """Parse the deliberately small canonical BK YAML subset.

    Supported syntax is exactly the form emitted by :func:`write_config`.
    JSON string quoting is used for list values, which handles spaces and
    unusual path characters without needing a general YAML implementation.
    """

    lines = text.splitlines()
    if text and not text.endswith(("\n", "\r")):
        # A final newline is part of the canonical writer format.  Refusing it
        # also prevents accidentally accepting a truncated manually edited file.
        raise ConfigError("backup.yaml must end with a newline")
    if len(lines) < 3:
        raise ConfigError("backup.yaml must contain version, retention, and sources")
    if lines[0] != "version: 1":
        match = re.fullmatch(r"version: ([0-9]+)", lines[0])
        if not match:
            raise ConfigError("backup.yaml has an invalid version line")
        raise ConfigError(f"unsupported backup.yaml version: {match.group(1)}")
    retention_match = re.fullmatch(r"retention: ([0-9]+)", lines[1])
    if not retention_match:
        raise ConfigError("backup.yaml has an invalid retention line")
    retention = int(retention_match.group(1))
    if retention < 1:
        raise ConfigError("backup.yaml retention must be an integer greater than or equal to 1")
    if lines[2] != "sources:":
        raise ConfigError("backup.yaml must contain a sources section")

    source_values: list[Path] = []
    for line_number, line in enumerate(lines[3:], start=4):
        if not line.startswith("  - "):
            raise ConfigError(f"backup.yaml has invalid source syntax on line {line_number}")
        encoded = line[4:]
        try:
            value = json.loads(encoded)
        except json.JSONDecodeError as exc:
            raise ConfigError(f"backup.yaml has invalid source quoting on line {line_number}") from exc
        if not isinstance(value, str) or not value:
            raise ConfigError(f"backup.yaml source on line {line_number} must be a non-empty string")
        if not os.path.isabs(value):
            raise ConfigError(f"backup.yaml source on line {line_number} must be absolute: {value}")
        try:
            source = normalize_path(value)
        except BKError as exc:
            raise ConfigError(f"backup.yaml source on line {line_number} is invalid: {exc}") from exc
        if value != str(source):
            raise ConfigError(f"backup.yaml source on line {line_number} is not canonical: {value}")
        if source in source_values:
            # Exact duplicates are not produced by BK and make source order
            # needlessly ambiguous.  Parent/child overlap remains legal.
            raise ConfigError(f"backup.yaml contains duplicate source: {source}")
        source_values.append(source)

    if not source_values:
        raise ConfigError("backup.yaml must contain the protected self source first")
    expected_self = normalize_path(config_path)
    if source_values[0] != expected_self:
        raise ConfigError(f"backup.yaml source number 1 must be {expected_self}")
    for index, source in enumerate(source_values):
        if index == 0:
            continue
        if path_is_runtime(source, paths):
            raise ConfigError(f"backup.yaml source {source} is inside the protected BK runtime")
    return Config(version=CONFIG_VERSION, retention=retention, sources=source_values)


def read_config(paths: Paths) -> Config | None:
    if not os.path.lexists(paths.config):
        return None
    if paths.config.is_symlink() or not paths.config.is_file():
        raise ConfigError(f"backup configuration is not a regular file: {paths.config}")
    try:
        text = paths.config.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise ConfigError(f"cannot read backup configuration {paths.config}: {exc}") from exc
    try:
        return parse_config_text(text, paths.config, paths)
    except ConfigError:
        raise
    except Exception as exc:  # pragma: no cover - defensive parser boundary
        raise ConfigError(f"cannot parse backup configuration {paths.config}: {exc}") from exc


def write_config(paths: Paths, config: Config) -> None:
    lines = ["version: 1", f"retention: {config.retention}", "sources:"]
    lines.extend(f"  - {json.dumps(str(source), ensure_ascii=False)}" for source in config.sources)
    atomic_write_bytes(paths.config, ("\n".join(lines) + "\n").encode("utf-8"), mode=0o600)


def parse_selection(raw: str, count: int) -> list[int]:
    """Parse the numeric selector's natural comma/space-separated forms."""

    values = re.findall(r"[0-9]+", raw)
    if not values:
        return []
    selected: list[int] = []
    errors: list[str] = []
    for value in values:
        index = int(value)
        if index < 1 or index > count:
            errors.append(value)
            continue
        if index not in selected:
            selected.append(index)
    if errors:
        raise BKError(f"selection contains invalid number(s): {', '.join(errors)}")
    return selected


@dataclass(frozen=True)
class SelectorItem:
    """One numbered leaf in the unified source editor."""

    index: int | None
    label: str
    kind: str = ""
    selectable: bool = True
    state: str = ""
    path: Path | None = None
    group: str = "current"


@dataclass
class SelectorGroup:
    """One of the two logical groups shown by the source editor."""

    key: str
    label: str
    expanded: bool
    item_indices: list[int | None] = field(default_factory=list)


@dataclass
class SelectorState:
    """The one selection state shared by rows, mouse, keyboard, and input."""

    items: list[SelectorItem]
    groups: list[SelectorGroup] = field(default_factory=list)
    selected_indices: set[int] = field(default_factory=set)
    focus_node: tuple[str, str | int | None] | None = None
    input_buffer: str = ""
    input_cursor: int = 0
    input_active: bool = False
    input_editing: bool = False
    input_token: str = ""
    input_token_index: int | None = None
    input_token_base_indices: set[int] = field(default_factory=set)
    error: str | None = None

    def __post_init__(self) -> None:
        if not self.groups:
            self.groups = [
                SelectorGroup("current", "Current directory", True),
                SelectorGroup("elsewhere", "Configured elsewhere", False),
            ]
        known_groups = {group.key for group in self.groups}
        for item in self.items:
            if item.group not in known_groups:
                raise BKError(f"selector item has unknown group: {item.group}")
            group = next(group for group in self.groups if group.key == item.group)
            if item.index not in group.item_indices:
                group.item_indices.append(item.index)
        selectable = set(self.selectable_indices)
        if not self.selected_indices <= selectable:
            raise BKError("selector selection contains a non-selectable entry")
        if self.focus_node not in self.visible_nodes():
            self.focus_node = self._first_focusable_node()
        self.input_buffer = self.selection_text()
        self.input_cursor = len(self.input_buffer)

    @property
    def focus_index(self) -> int | None:
        """Return the focused leaf number, if the focus is on a leaf."""

        if self.focus_node is not None and self.focus_node[0] == "item":
            value = self.focus_node[1]
            return value if isinstance(value, int) else None
        return None

    @focus_index.setter
    def focus_index(self, value: int | None) -> None:
        self.focus_node = ("item", value) if value is not None else self._first_focusable_node()

    @property
    def selectable_count(self) -> int:
        return len(self.selectable_indices)

    @property
    def selected_count(self) -> int:
        return len(self.selected_indices)

    @property
    def selectable_indices(self) -> list[int]:
        return [item.index for item in self.items if item.index is not None and item.selectable]

    def item_for_index(self, index: int | None) -> SelectorItem | None:
        return next((item for item in self.items if item.index == index), None)

    def group_for_key(self, key: str) -> SelectorGroup | None:
        return next((group for group in self.groups if group.key == key), None)

    def group_for_item(self, index: int | None) -> SelectorGroup | None:
        item = self.item_for_index(index)
        return self.group_for_key(item.group) if item is not None else None

    def group_selected_count(self, group: SelectorGroup) -> int:
        return sum(index in self.selected_indices for index in group.item_indices if index is not None)

    def visible_nodes(self) -> list[tuple[str, str | int | None]]:
        nodes: list[tuple[str, str | int | None]] = []
        for group in self.groups:
            nodes.append(("group", group.key))
            if group.expanded:
                nodes.extend(("item", index) for index in group.item_indices)
        return nodes

    def _first_focusable_node(self) -> tuple[str, str | int | None] | None:
        for node in self.visible_nodes():
            if node[0] == "item":
                item = self.item_for_index(node[1] if isinstance(node[1], int) else None)
                if item is not None and item.selectable:
                    return node
        return self.visible_nodes()[0] if self.visible_nodes() else None

    def selection_text(self) -> str:
        """Return selected numbers in stable global list order."""

        return ",".join(
            str(item.index)
            for item in self.items
            if item.index is not None and item.index in self.selected_indices
        )

    def set_selection(self, indices: list[int] | set[int]) -> None:
        selected = set(indices)
        selectable = set(self.selectable_indices)
        invalid = sorted(selected - selectable)
        if invalid:
            raise BKError(
                "selection contains non-selectable number(s): "
                + ", ".join(str(index) for index in invalid)
            )
        self.selected_indices = selected
        self.input_buffer = self.selection_text()
        self.input_cursor = len(self.input_buffer)
        self.input_token = ""
        self.input_token_index = None
        self.input_token_base_indices.clear()
        self.input_editing = False
        self.error = None

    def set_selection_text(self, raw: str) -> None:
        """Parse and apply one numeric selector value atomically."""

        self.set_selection(parse_selection(raw, max((item.index or 0 for item in self.items), default=0)))

    def edit_selection_text(self, raw: str, cursor: int | None = None) -> bool:
        """Apply a composed numeric value without creating a second state."""

        try:
            self.set_selection(parse_selection(raw, max((item.index or 0 for item in self.items), default=0)))
        except BKError as exc:
            self.error = str(exc)
            self.input_buffer = self.selection_text()
            self.input_cursor = len(self.input_buffer)
            self.input_editing = True
            return False
        self.input_buffer = self.selection_text()
        requested_cursor = cursor if cursor is not None else len(self.input_buffer)
        self.input_cursor = max(0, min(len(self.input_buffer), requested_cursor))
        self.input_editing = True
        self.error = None
        return True

    def toggle(self, index: int | None = None) -> bool:
        """Toggle one selectable leaf and synchronize the numeric field."""

        target = self.focus_index if index is None else index
        item = self.item_for_index(target)
        if item is None or target is None or not item.selectable:
            return False
        self.focus_node = ("item", target)
        if target in self.selected_indices:
            self.selected_indices.remove(target)
        else:
            self.selected_indices.add(target)
        self.input_buffer = self.selection_text()
        self.input_cursor = len(self.input_buffer)
        self.input_editing = False
        self.input_active = False
        self.input_token = ""
        self.input_token_index = None
        self.input_token_base_indices.clear()
        self.error = None
        return True

    def move_focus(self, offset: int) -> None:
        nodes = self.visible_nodes()
        if not nodes:
            self.focus_node = None
            return
        try:
            position = nodes.index(self.focus_node) if self.focus_node is not None else 0
        except ValueError:
            position = 0
        self.focus_node = nodes[max(0, min(len(nodes) - 1, position + offset))]

    def focus_home(self) -> None:
        nodes = self.visible_nodes()
        for node in nodes:
            if node[0] == "item":
                item = self.item_for_index(node[1] if isinstance(node[1], int) else None)
                if item is not None and item.selectable:
                    self.focus_node = node
                    return
        self.focus_node = nodes[0] if nodes else None

    def focus_end(self) -> None:
        nodes = self.visible_nodes()
        for node in reversed(nodes):
            if node[0] == "item":
                item = self.item_for_index(node[1] if isinstance(node[1], int) else None)
                if item is not None and item.selectable:
                    self.focus_node = node
                    return
        self.focus_node = nodes[-1] if nodes else None

    def toggle_group(self, key: str) -> bool:
        group = self.group_for_key(key)
        if group is None:
            return False
        group.expanded = not group.expanded
        if self.focus_node == ("group", key):
            return True
        if self.focus_node not in self.visible_nodes():
            self.focus_node = ("group", key)
        return True

    def expand_group(self, key: str, expanded: bool = True) -> bool:
        group = self.group_for_key(key)
        if group is None:
            return False
        group.expanded = expanded
        if self.focus_node not in self.visible_nodes():
            self.focus_node = ("group", key)
        return True

    def horizontal(self, direction: int) -> None:
        if self.focus_node is None:
            return
        if self.focus_node[0] == "group":
            key = self.focus_node[1]
            if not isinstance(key, str):
                return
            group = self.group_for_key(key)
            if group is not None and direction > 0:
                group.expanded = True
            elif group is not None and direction < 0:
                group.expanded = False
            return
        index = self.focus_node[1] if isinstance(self.focus_node[1], int) else None
        group = self.group_for_item(index)
        if group is not None and direction < 0:
            group.expanded = False
            self.focus_node = ("group", group.key)
        elif group is not None and direction > 0:
            group.expanded = True


@dataclass(frozen=True)
class SelectorResult:
    """Result returned by the unified source editor."""

    selected_indices: list[int]
    cancelled: bool = False


class SelectorUnavailable(Exception):
    """The terminal cannot support the interactive source editor."""


def item_kind(path: Path) -> tuple[bool, str]:
    try:
        mode = os.lstat(path).st_mode
    except FileNotFoundError:
        return False, "missing"
    except OSError:
        return False, "unknown"
    if stat.S_ISLNK(mode):
        return True, "symlink"
    if stat.S_ISREG(mode):
        return True, "file"
    if stat.S_ISDIR(mode):
        return True, "directory"
    return True, "special"


def display_path(path: Path) -> str:
    """Use a concise home-relative spelling in human-only output."""

    try:
        relative = path.relative_to(Path.home())
    except ValueError:
        return str(path)
    return "~/" + str(relative)


def print_help() -> None:
    console.print("[bold]bk[/bold] - workspace backup tool")
    console.print("Usage: bk <command> [--json]")
    table = Table(show_header=True, header_style="bold")
    table.add_column("Command")
    table.add_column("Alias")
    table.add_column("Description")
    for command, alias, description in (
        ("list", "l", "show configured sources"),
        ("edit", "e", "edit configured backup sources"),
        ("run", "r", "create a backup snapshot"),
        ("status", "s", "show backup status and history"),
    ):
        table.add_row(command, alias, description)
    console.print(table)
    console.print("Aliases: h/help for this help, l/list, e/edit, r/run, s/status")


def command_json(args: list[str]) -> tuple[list[str], bool]:
    json_mode = False
    remaining: list[str] = []
    for arg in args:
        if arg == "--json":
            if json_mode:
                raise BKError("--json was specified more than once")
            json_mode = True
        elif arg in ("--help", "-h"):
            raise BKError("help is available as bk --help or bk help")
        else:
            remaining.append(arg)
    if remaining:
        raise BKError(f"unexpected argument: {remaining[0]}")
    return remaining, json_mode


def list_command(paths: Paths, json_mode: bool) -> int:
    config = read_config(paths)
    if config is None:
        result = {"configured": False, "version": CONFIG_VERSION, "retention": None, "sources": []}
        if json_mode:
            emit_json(result)
        else:
            console.print("No backup configuration exists; no sources are configured.")
        return 0

    sources: list[dict[str, Any]] = []
    for index, source in enumerate(config.sources, start=1):
        exists, kind = item_kind(source)
        sources.append(
            {
                "index": index,
                "path": str(source),
                "self": index == 1,
                "exists": exists,
                "kind": kind,
            }
        )
    result = {
        "configured": True,
        "version": config.version,
        "retention": config.retention,
        "sources": sources,
    }
    if json_mode:
        emit_json(result)
        return 0

    table = Table(title="Configured backup sources", show_header=True, header_style="bold")
    table.add_column("#", justify="right")
    table.add_column("Path")
    table.add_column("State")
    table.add_column("Kind")
    for source in sources:
        state = "protected self-entry" if source["self"] else ("present" if source["exists"] else "missing")
        table.add_row(str(source["index"]), source["path"], state, source["kind"])
    console.print(table)
    console.print(f"Retention: {config.retention} successful backup(s)")
    return 0


def immediate_children(directory: Path) -> list[Path]:
    try:
        with os.scandir(directory) as iterator:
            children = [Path(entry.path) for entry in iterator]
    except OSError as exc:
        raise BKError(f"cannot list current directory {directory}: {exc}") from exc
    return sorted(children, key=lambda path: path.name)


def selector_display_label(path: Path, kind: str, *, relative_name: bool = False) -> str:
    """Format a leaf label, marking directories with a trailing slash."""

    label = path.name if relative_name else display_path(path)
    return f"{label}/" if kind == "directory" and not label.endswith("/") else label


def build_edit_selector(
    paths: Paths,
    children: list[Path],
    config: Config | None,
) -> tuple[list[SelectorItem], list[SelectorGroup], set[int]]:
    """Build the two editor groups and one initial selection set.

    Numbering is assigned before rendering, so collapsing either group never
    changes a leaf's number.  The self-entry is rendered but intentionally
    has no number because it is not editable.
    """

    config_path = normalize_path(paths.config)
    configured = list(config.sources[1:]) if config is not None else []
    configured_set = set(configured)
    items: list[SelectorItem] = []
    selected: set[int] = set()
    next_index = 1

    for child in children:
        candidate = normalize_path(child)
        exists, kind = item_kind(candidate)
        if candidate == config_path:
            item = SelectorItem(
                index=None,
                label=selector_display_label(candidate, kind, relative_name=True),
                kind=kind,
                selectable=False,
                state="protected self-entry",
                path=candidate,
                group="current",
            )
        else:
            selectable = not path_is_runtime(candidate, paths)
            item = SelectorItem(
                index=next_index,
                label=selector_display_label(candidate, kind, relative_name=True),
                kind=kind,
                selectable=selectable,
                state=("BK-managed; not selectable" if not selectable else ("present" if exists else "missing")),
                path=candidate,
                group="current",
            )
            next_index += 1
            if selectable and candidate in configured_set:
                selected.add(item.index)
        items.append(item)

    current_paths = {item.path for item in items if item.path is not None}
    for source in configured:
        if source in current_paths:
            continue
        exists, kind = item_kind(source)
        item = SelectorItem(
            index=next_index,
            label=selector_display_label(source, kind),
            kind=kind,
            selectable=True,
            state="present" if exists else "missing",
            path=source,
            group="elsewhere",
        )
        items.append(item)
        selected.add(next_index)
        next_index += 1

    groups = [
        SelectorGroup(
            key="current",
            label="Current directory",
            expanded=True,
            item_indices=[item.index for item in items if item.group == "current"],
        ),
        SelectorGroup(
            key="elsewhere",
            label="Configured elsewhere",
            expanded=False,
            item_indices=[item.index for item in items if item.group == "elsewhere"],
        ),
    ]
    return items, groups, selected


def selector_tty_available() -> bool:
    """Whether the full-screen editor is safe for the current terminal."""

    try:
        return bool(
            sys.stdin.isatty()
            and sys.stdout.isatty()
            and os.environ.get("TERM") not in {None, "", "dumb", "unknown"}
        )
    except (AttributeError, OSError):
        return False


def _selector_key(curses_module: Any, name: str, default: int) -> int:
    return int(getattr(curses_module, name, default))


def _selector_addnstr(screen: Any, row: int, column: int, value: str, width: int, attribute: int = 0) -> None:
    if width <= 0:
        return
    try:
        screen.addnstr(row, column, value, width, attribute)
    except (AttributeError, TypeError):
        # Small fake screens used by tests may expose only the four-argument
        # form.  Real curses screens accept the attribute argument.
        screen.addnstr(row, column, value, width)


def _selector_mouse_mask(curses_module: Any) -> int:
    mask = 0
    for name in (
        "BUTTON1_PRESSED",
        "BUTTON1_CLICKED",
        "BUTTON4_PRESSED",
        "BUTTON5_PRESSED",
    ):
        mask |= int(getattr(curses_module, name, 0))
    return mask


class SelectorTerminal:
    """Own curses terminal state and restore it on every exit path."""

    def __init__(self, curses_module: Any) -> None:
        self.curses = curses_module
        self.screen: Any | None = None
        self.started = False
        self.keypad_enabled = False
        self.raw_enabled = False
        self.mouse_enabled = False
        self.mouse_previous_mask = 0
        self.cursor_visibility: int | None = None

    def start(self) -> Any:
        try:
            self.screen = self.curses.initscr()
            self.started = True
            self.curses.noecho()
            # Raw mode disables terminal flow control, so Ctrl-Q reaches BK
            # instead of being consumed as XON by the line discipline.
            self.curses.raw()
            self.raw_enabled = True
            self.screen.keypad(True)
            self.keypad_enabled = True
            # Mark this before the call so cleanup is attempted if the
            # terminal reports an error while enabling mouse events.
            self.mouse_enabled = True
            mouse_result = self.curses.mousemask(_selector_mouse_mask(self.curses))
            if isinstance(mouse_result, tuple) and len(mouse_result) >= 2:
                self.mouse_previous_mask = int(mouse_result[1])
            try:
                self.cursor_visibility = self.curses.curs_set(0)
            except Exception:
                pass
            return self.screen
        except Exception as exc:
            self.close()
            raise SelectorUnavailable(f"cannot initialize the interactive selector: {exc}") from exc

    def close(self) -> None:
        """Restore mouse reporting, keypad, raw mode, echo, and the screen."""

        if not self.started:
            return
        # Every cleanup operation is attempted even if an earlier curses call
        # fails.  This matters when a terminal is detached during selection.
        if self.mouse_enabled:
            try:
                self.curses.mousemask(self.mouse_previous_mask)
            except Exception:
                pass
            self.mouse_enabled = False
        if self.keypad_enabled and self.screen is not None:
            try:
                self.screen.keypad(False)
            except Exception:
                pass
            self.keypad_enabled = False
        try:
            self.curses.echo()
        except Exception:
            pass
        if self.raw_enabled:
            try:
                self.curses.noraw()
            except Exception:
                pass
            self.raw_enabled = False
        if self.cursor_visibility is not None:
            try:
                self.curses.curs_set(self.cursor_visibility)
            except Exception:
                pass
        try:
            self.curses.endwin()
        except Exception:
            pass
        self.started = False


def _selector_visible_window(state: SelectorState, height: int) -> tuple[int, int, int]:
    del state
    list_top = 2
    input_row = max(list_top + 1, height - 2)
    visible = max(1, input_row - list_top)
    return list_top, input_row, visible


def _selector_keep_focus_visible(state: SelectorState, scroll: int, visible: int) -> int:
    nodes = state.visible_nodes()
    if state.focus_node is None:
        return max(0, scroll)
    try:
        position = nodes.index(state.focus_node)
    except ValueError:
        return max(0, scroll)
    if position < scroll:
        return position
    if position >= scroll + visible:
        return position - visible + 1
    return scroll


def _selector_group_text(group: SelectorGroup, state: SelectorState, width: int) -> str:
    marker = "▼" if group.expanded else "▶"
    return f"{marker} {group.label}  {state.group_selected_count(group)}/{len(group.item_indices)}"[: max(0, width)]


def _selector_row_text(item: SelectorItem, selected: bool, width: int) -> str:
    marker = "x" if selected else " "
    number = f"{item.index}. " if item.index is not None else ""
    value = f"│  [{marker}]  {number}{item.label}"
    if item.kind:
        value += f"  {item.kind}"
    if not item.selectable:
        value += f"  ({item.state})"
    return value[: max(0, width)]


def _selector_header(context: Path, state: SelectorState, width: int) -> str:
    """Keep the synchronized configured count visible by truncating context."""

    available = max(0, width - 1)
    left = " BK · Edit"
    count = f"Configured {state.selected_count}"
    if available <= len(count):
        return count[:available]
    fixed = len(left) + len(count) + 6
    if available < fixed:
        left_width = max(0, available - len(count) - 2)
        return f"{left[:left_width]}  {count}"[-available:]
    context_width = available - fixed
    context_text = display_path(context)
    if len(context_text) > context_width:
        context_text = context_text[: max(0, context_width - 1)] + "…"
    return f"{left}   {context_text:<{context_width}}   {count}"


def _selector_render(
    screen: Any,
    state: SelectorState,
    context: Path,
    scroll: int,
    curses_module: Any,
) -> int:
    height, width = screen.getmaxyx()
    screen.erase()
    list_top, input_row, visible = _selector_visible_window(state, height)
    scroll = _selector_keep_focus_visible(state, scroll, visible)
    header = _selector_header(context, state, width)
    _selector_addnstr(screen, 0, 0, header, max(0, width - 1), getattr(curses_module, "A_BOLD", 0))

    nodes = state.visible_nodes()
    for row_offset, node in enumerate(nodes[scroll : scroll + visible]):
        row = list_top + row_offset
        focused = node == state.focus_node
        if node[0] == "group":
            group = state.group_for_key(str(node[1]))
            if group is None:
                continue
            text = _selector_group_text(group, state, width - 1)
            attribute = getattr(curses_module, "A_REVERSE", 0) if focused else getattr(curses_module, "A_BOLD", 0)
        else:
            index = node[1] if isinstance(node[1], int) else None
            item = state.item_for_index(index)
            if item is None:
                item = next((candidate for candidate in state.items if candidate.index is None), None)
            if item is None:
                continue
            checked = item.index in state.selected_indices or item.state == "protected self-entry"
            text = _selector_row_text(item, checked, width - 1)
            attribute = getattr(curses_module, "A_REVERSE", 0) if focused else 0
            if not item.selectable:
                attribute |= getattr(curses_module, "A_DIM", 0)
        _selector_addnstr(screen, row, 0, text, max(0, width - 1), attribute)

    selector_text = state.selection_text()
    if state.error:
        selector_text = f"{selector_text}  ({state.error})"
    _selector_addnstr(screen, input_row, 0, f"Selection: {selector_text}", max(0, width - 1), 0)
    help_row = min(max(0, height - 1), input_row + 1)
    help_text = "↑↓ Move   ←→ Expand   Space Toggle   Enter Apply   Esc/Ctrl-Q Cancel"
    if width > len(help_text) + len("   Click Toggle") + 1:
        help_text = help_text.replace("   Enter Apply", "   Click Toggle   Enter Apply")
    _selector_addnstr(screen, help_row, 0, help_text, max(0, width - 1), getattr(curses_module, "A_DIM", 0))
    try:
        self_curs_set = getattr(curses_module, "curs_set", None)
        if state.input_active:
            screen.move(input_row, min(max(0, width - 1), len("Selection: ") + state.input_cursor))
            if self_curs_set is not None:
                self_curs_set(1)
        elif self_curs_set is not None:
            self_curs_set(0)
    except Exception:
        pass
    screen.refresh()
    return scroll


def _selector_insert_input(state: SelectorState, value: str) -> None:
    started_editing = not state.input_editing
    if not state.input_editing:
        state.input_editing = True
        state.input_token = ""
        state.input_token_index = None
        state.input_token_base_indices.clear()
    if value in {",", " "}:
        if started_editing:
            state.selected_indices.clear()
        state.input_token = ""
        state.input_token_index = None
        state.input_token_base_indices = set(state.selected_indices)
        state.input_buffer = state.selection_text()
        state.input_cursor = len(state.input_buffer)
        state.error = None
        return
    _selector_update_input_token(state, state.input_token + value)


def _selector_update_input_token(state: SelectorState, token: str) -> None:
    """Replace the in-progress numeric token without losing earlier tokens."""

    state.input_token = token
    state.input_token_index = None
    state.error = None
    selected = set(state.input_token_base_indices)
    if token:
        index = int(token)
        item = state.item_for_index(index)
        if item is None:
            state.error = f"selection contains invalid number(s): {index}"
            return
        elif not item.selectable:
            state.error = f"selection contains non-selectable number(s): {index}"
            return
        else:
            state.input_token_index = index
            selected.add(index)

    state.selected_indices = selected
    state.input_buffer = state.selection_text()
    state.input_cursor = len(state.input_buffer)
    if state.input_token_index is not None:
        cursor = 0
        for item in state.items:
            if item.index not in state.selected_indices:
                continue
            cursor += len(str(item.index))
            if item.index == state.input_token_index:
                state.input_cursor = cursor
                break
            cursor += 1


def _selector_backspace(state: SelectorState) -> None:
    if not state.input_editing or not state.input_token:
        return
    _selector_update_input_token(state, state.input_token[:-1])


def _selector_focus_list(state: SelectorState) -> None:
    """Finish numeric composition so list navigation and Space work together."""

    state.input_active = False
    state.input_editing = False
    state.input_token = ""
    state.input_token_index = None
    state.input_token_base_indices.clear()
    state.input_buffer = state.selection_text()
    state.input_cursor = len(state.input_buffer)
    state.error = None


def _selector_mouse_event(
    screen: Any,
    state: SelectorState,
    y: int,
    bstate: int,
    scroll: int,
    visible: int,
    curses_module: Any,
) -> int:
    list_top, input_row, _ = _selector_visible_window(state, screen.getmaxyx()[0])
    button4 = int(getattr(curses_module, "BUTTON4_PRESSED", 0))
    button5 = int(getattr(curses_module, "BUTTON5_PRESSED", 0))
    if bstate & button4:
        state.move_focus(-1)
        return _selector_keep_focus_visible(state, scroll, visible)
    if bstate & button5:
        state.move_focus(1)
        return _selector_keep_focus_visible(state, scroll, visible)
    left_event = int(getattr(curses_module, "BUTTON1_CLICKED", 0))
    left_event |= int(getattr(curses_module, "BUTTON1_PRESSED", 0))
    if not bstate & left_event:
        return scroll
    if list_top <= y < list_top + visible:
        position = scroll + y - list_top
        nodes = state.visible_nodes()
        if 0 <= position < len(nodes):
            node = nodes[position]
            if node[0] == "group":
                state.focus_node = node
                state.toggle_group(str(node[1]))
            else:
                index = node[1] if isinstance(node[1], int) else None
                item = state.item_for_index(index)
                if item is not None and item.selectable:
                    state.toggle(index)
    elif y == input_row:
        state.input_active = True
        state.input_editing = False
        state.input_token = ""
        state.input_token_index = None
        state.input_token_base_indices.clear()
        state.input_cursor = len(state.input_buffer)
    return _selector_keep_focus_visible(state, scroll, visible)


def run_edit_selector(
    items: list[SelectorItem],
    groups: list[SelectorGroup],
    selected_indices: set[int],
    *,
    context: Path,
    curses_module: Any | None = None,
) -> SelectorResult:
    """Run the unified source editor, returning the one selection state."""

    if curses_module is None:
        import curses as curses_module

    terminal = SelectorTerminal(curses_module)
    try:
        screen = terminal.start()
        state = SelectorState(items, groups=groups, selected_indices=set(selected_indices))
        scroll = 0
        while True:
            height, _ = screen.getmaxyx()
            _list_top, _input_row, visible = _selector_visible_window(state, height)
            scroll = _selector_keep_focus_visible(state, scroll, visible)
            scroll = _selector_render(screen, state, context, scroll, curses_module)
            key = screen.getch()
            if key in (27, 17, 3):  # Escape, Ctrl-Q, Ctrl-C
                return SelectorResult([], cancelled=True)
            if key in (
                _selector_key(curses_module, "KEY_ENTER", 10),
                10,
                13,
            ):
                if state.error:
                    continue
                return SelectorResult(
                    [item.index for item in state.items if item.index is not None and item.index in state.selected_indices]
                )

            key_up = _selector_key(curses_module, "KEY_UP", -101)
            key_down = _selector_key(curses_module, "KEY_DOWN", -102)
            key_left = _selector_key(curses_module, "KEY_LEFT", -103)
            key_right = _selector_key(curses_module, "KEY_RIGHT", -104)
            key_home = _selector_key(curses_module, "KEY_HOME", -105)
            key_end = _selector_key(curses_module, "KEY_END", -106)
            key_page_up = _selector_key(curses_module, "KEY_PPAGE", -107)
            key_page_down = _selector_key(curses_module, "KEY_NPAGE", -108)
            key_backspace = _selector_key(curses_module, "KEY_BACKSPACE", 263)
            key_delete = _selector_key(curses_module, "KEY_DC", 330)
            key_mouse = _selector_key(curses_module, "KEY_MOUSE", -111)

            if key == key_mouse:
                try:
                    _mouse_id, _mouse_x, mouse_y, _mouse_z, mouse_state = curses_module.getmouse()
                except Exception:
                    continue
                scroll = _selector_mouse_event(screen, state, mouse_y, mouse_state, scroll, visible, curses_module)
                continue
            if key in (key_up, ord("k")):
                _selector_focus_list(state)
                state.move_focus(-1)
                continue
            if key in (key_down, ord("j")):
                _selector_focus_list(state)
                state.move_focus(1)
                continue
            if key == key_left:
                _selector_focus_list(state)
                state.horizontal(-1)
                continue
            if key == key_right:
                _selector_focus_list(state)
                state.horizontal(1)
                continue
            if key == key_home:
                _selector_focus_list(state)
                state.focus_home()
                continue
            if key == key_end:
                _selector_focus_list(state)
                state.focus_end()
                continue
            if key == key_page_up:
                _selector_focus_list(state)
                state.move_focus(-visible)
                continue
            if key == key_page_down:
                _selector_focus_list(state)
                state.move_focus(visible)
                continue
            if key == 9:
                if state.input_active:
                    _selector_focus_list(state)
                else:
                    state.input_active = True
                    state.input_editing = False
                    state.input_token = ""
                    state.input_token_index = None
                    state.input_token_base_indices.clear()
                    state.input_cursor = len(state.input_buffer)
                continue

            if state.input_active:
                if key in (key_backspace, key_delete, 8, 127):
                    _selector_backspace(state)
                elif key == 21:  # Ctrl-U clears the numeric editor.
                    state.selected_indices.clear()
                    state.input_buffer = ""
                    state.input_cursor = 0
                    state.input_token = ""
                    state.input_token_index = None
                    state.input_token_base_indices.clear()
                    state.error = None
                elif isinstance(key, int) and (key in (ord(","), ord(" ")) or 48 <= key <= 57):
                    _selector_insert_input(state, chr(key))
                continue

            if key == ord(" "):
                state.toggle()
            elif isinstance(key, int) and 48 <= key <= 57:
                state.input_active = True
                state.input_editing = False
                _selector_insert_input(state, chr(key))
            elif key == ord(","):
                state.input_active = True
                state.input_editing = False
                _selector_insert_input(state, ",")
    except KeyboardInterrupt:
        return SelectorResult([], cancelled=True)
    finally:
        terminal.close()


def apply_edit_selection(paths: Paths, config: Config | None, state: SelectorState) -> bool:
    """Apply one confirmed editor state while preserving source ordering."""

    original = list(config.sources[1:]) if config is not None else []
    original_set = set(original)
    selected_paths = {
        item.path
        for item in state.items
        if item.index is not None and item.index in state.selected_indices and item.path is not None
    }
    final_sources = [source for source in original if source in selected_paths]
    additions: list[Path] = []
    addition_set: set[Path] = set()
    for item in state.items:
        if item.group != "current" or item.index is None or item.index not in state.selected_indices:
            continue
        if item.path is None or item.path in original_set or item.path in addition_set:
            continue
        if path_is_runtime(item.path, paths):
            raise BKError(f"cannot select BK-managed runtime path: {item.path}")
        if not os.path.lexists(item.path):
            raise BKError(f"selected path disappeared before it could be saved: {item.path}")
        additions.append(item.path)
        addition_set.add(item.path)
    final_sources.extend(additions)

    if config is not None and final_sources == original:
        return False
    if config is None and not final_sources:
        return False
    if config is None:
        ensure_runtime_directory(paths)
        new_config = Config(CONFIG_VERSION, DEFAULT_RETENTION, [normalize_path(paths.config), *final_sources])
    else:
        new_config = Config(config.version, config.retention, [config.sources[0], *final_sources])
    write_config(paths, new_config)
    return True


def edit_command(paths: Paths) -> int:
    """Interactively edit all configured user sources in one transaction."""

    if not selector_tty_available():
        error_console.print("bk edit requires an interactive TTY; use bk list for read-only inspection")
        return 2
    try:
        config = read_config(paths)
        children = immediate_children(Path.cwd())
        items, groups, selected = build_edit_selector(paths, children, config)
        result = run_edit_selector(items, groups, selected, context=Path.cwd())
    except (BKError, SelectorUnavailable) as exc:
        error_console.print(str(exc))
        return 2
    if result.cancelled:
        console.print("Edit cancelled.")
        return 1

    state = SelectorState(items, groups=groups, selected_indices=set(result.selected_indices))
    try:
        changed = apply_edit_selection(paths, config, state)
    except BKError as exc:
        error_console.print(str(exc))
        return 2
    if not changed:
        console.print("No changes made.")
    else:
        console.print(f"Updated backup sources in {display_path(paths.config)}")
    return 0


def classify_regular_file(path: Path) -> ClassResult:
    """Classify one regular file through the required system ``file`` command."""

    try:
        completed = subprocess.run(
            ["file", "--brief", "--", str(path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except FileNotFoundError as exc:
        raise BKError("the required file utility is not available") from exc
    except OSError as exc:
        raise BKError(f"cannot run file classifier for {path}: {exc}") from exc
    output = completed.stdout.strip()
    if completed.returncode != 0:
        detail = (completed.stderr or output).strip()
        raise BKError(f"file classifier failed for {path}: {detail or 'unknown error'}")
    if not output:
        raise BKError(f"file classifier returned no result for {path}")
    normalized = output.lower()
    if re.search(r"sqlite\s+3\.x\s+database", normalized):
        return ClassResult(handler="sqlite", output=output)
    return ClassResult(handler="copy", output=output)


def lstat_kind(path: Path) -> tuple[str, os.stat_result]:
    try:
        metadata = os.lstat(path)
    except FileNotFoundError as exc:
        raise BKError(f"source disappeared: {path}") from exc
    except OSError as exc:
        raise BKError(f"cannot inspect source {path}: {exc}") from exc
    mode = metadata.st_mode
    if stat.S_ISREG(mode):
        return "file", metadata
    if stat.S_ISDIR(mode):
        return "directory", metadata
    if stat.S_ISLNK(mode):
        return "symlink", metadata
    raise BKError(f"unsupported special filesystem object: {path}")


def validate_plan_entry(entry: PlanEntry) -> None:
    """Fail if a planned object was replaced or changed incompatibly."""

    kind, metadata = lstat_kind(entry.source_path)
    if kind != entry.kind or (metadata.st_dev, metadata.st_ino) != (entry.device, entry.inode):
        raise BKError(f"source changed type or identity during snapshot: {entry.source_path}")
    if (
        kind == "file"
        and entry.classification is not None
        and entry.classification.handler == "copy"
        and (metadata.st_size, metadata.st_mtime_ns) != (entry.size, entry.modified_ns)
    ):
        raise BKError(f"source file changed after classification: {entry.source_path}")


def build_snapshot_plan(config: Config, progress: BackupProgress | None = None) -> PlanSummary:
    classification_cache: dict[tuple[int, int], ClassResult] = {}
    source_plans: list[SourcePlan] = []

    def visit(source_index: int, source: Path, current: Path, relative: Path, target: SourcePlan) -> None:
        kind, metadata = lstat_kind(current)
        if kind == "file":
            identity = (metadata.st_dev, metadata.st_ino)
            classification = classification_cache.get(identity)
            if classification is None:
                classification = classify_regular_file(current)
                classification_cache[identity] = classification
            target.entries.append(
                PlanEntry(
                    source_index=source_index,
                    source_path=current,
                    relative=relative,
                    kind=kind,
                    device=metadata.st_dev,
                    inode=metadata.st_ino,
                    size=metadata.st_size,
                    modified_ns=metadata.st_mtime_ns,
                    classification=classification,
                )
            )
            return
        target.entries.append(
            PlanEntry(
                source_index=source_index,
                source_path=current,
                relative=relative,
                kind=kind,
                device=metadata.st_dev,
                inode=metadata.st_ino,
                size=metadata.st_size,
                modified_ns=metadata.st_mtime_ns,
            )
        )
        if kind != "directory":
            return
        try:
            with os.scandir(current) as iterator:
                children = sorted(iterator, key=lambda entry: entry.name)
        except OSError as exc:
            raise BKError(f"cannot traverse source directory {current}: {exc}") from exc
        for child in children:
            visit(source_index, source, Path(child.path), relative / child.name, target)

    for index, source in enumerate(config.sources, start=1):
        if progress is not None:
            progress.stage(f"Scanning source {index}/{len(config.sources)}")
        root_kind, _ = lstat_kind(source)
        target = SourcePlan(index=index, source=source, root_kind=root_kind)
        visit(index, source, source, Path(), target)
        source_plans.append(target)

    # Sidecar decisions are made after the full traversal.  Every regular file
    # was classified once while constructing the plan, including sidecars.
    for source_plan in source_plans:
        db_names_by_parent: dict[Path, set[str]] = {}
        for entry in source_plan.entries:
            if entry.is_regular and entry.classification and entry.classification.handler == "sqlite":
                db_names_by_parent.setdefault(entry.source_path.parent, set()).add(entry.source_path.name)
        if not db_names_by_parent:
            continue
        for entry in source_plan.entries:
            if entry.kind not in {"file", "symlink"}:
                continue
            candidate_name = entry.source_path.name
            parent_names = db_names_by_parent.get(entry.source_path.parent)
            if not parent_names:
                continue
            if entry.classification and entry.classification.handler == "sqlite":
                continue
            if any(
                candidate_name == f"{db_name}{suffix}"
                for db_name in parent_names
                for suffix in SQLITE_SIDECAR_SUFFIXES
            ):
                entry.skip_sidecar = True

    file_count = 0
    directory_count = 0
    symlink_count = 0
    input_bytes = 0
    skipped_sidecars = 0
    sqlite_count = 0
    for source_plan in source_plans:
        for entry in source_plan.entries:
            if entry.kind == "file":
                file_count += 1
                if entry.skip_sidecar:
                    skipped_sidecars += 1
                    continue
                try:
                    input_bytes += entry.source_path.stat().st_size
                except OSError as exc:
                    raise BKError(f"cannot stat source file {entry.source_path}: {exc}") from exc
                if entry.classification and entry.classification.handler == "sqlite":
                    sqlite_count += 1
            elif entry.kind == "directory":
                directory_count += 1
            elif entry.kind == "symlink":
                symlink_count += 1
    return PlanSummary(
        sources=source_plans,
        file_count=file_count,
        directory_count=directory_count,
        symlink_count=symlink_count,
        input_bytes=input_bytes,
        skipped_sidecars=skipped_sidecars,
        classifications=len(classification_cache),
        sqlite_count=sqlite_count,
    )


def source_label(source: Path) -> str:
    name = source.name
    if name:
        return name
    anchor = source.anchor.rstrip(os.sep)
    return anchor or "root"


def entry_destination(payload: Path, entry: PlanEntry, source: Path) -> Path:
    root = payload / f"{entry.source_index:04d}" / source_label(source)
    return root if not entry.relative.parts else root.joinpath(*entry.relative.parts)


def copy_symlink(source: Path, destination: Path) -> None:
    try:
        target = os.readlink(source)
        os.symlink(target, destination)
    except OSError as exc:
        raise BKError(f"cannot preserve symlink {source}: {exc}") from exc


def copy_regular_file(source: Path, destination: Path) -> None:
    try:
        shutil.copy2(source, destination, follow_symlinks=False)
    except OSError as exc:
        raise BKError(f"cannot stage file {source}: {exc}") from exc


def sqlite_source_uri(source: Path, *, immutable: bool = False) -> str:
    options = "mode=ro&immutable=1" if immutable else "mode=ro"
    return "file:" + quote(str(source), safe="/") + "?" + options


def sqlite_open_policy(source: Path) -> tuple[str, str, tuple[int, int, int, int] | None]:
    """Choose a read-only URI without creating sidecars for closed WAL databases."""

    try:
        with source.open("rb") as stream:
            header = stream.read(20)
    except OSError as exc:
        raise BKError(f"cannot inspect SQLite header for {source}: {exc}") from exc
    wal_mode = len(header) >= 20 and header.startswith(b"SQLite format 3\x00") and header[18:20] == b"\x02\x02"
    wal = source.with_name(source.name + "-wal")
    shm = source.with_name(source.name + "-shm")
    if wal_mode and not os.path.lexists(wal):
        metadata = os.lstat(source)
        guard = (metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns)
        return sqlite_source_uri(source, immutable=True), "guarded immutable read-only (closed WAL database)", guard
    if os.path.lexists(wal) and not os.path.lexists(shm):
        raise BKError(
            f"SQLite WAL exists without its shared-memory sidecar; refusing to create source state: {source}"
        )
    return sqlite_source_uri(source), "read-only SQLite connection", None


def copy_sqlite_file(source: Path, destination: Path, log_lines: list[str]) -> None:
    source_connection: sqlite3.Connection | None = None
    destination_connection: sqlite3.Connection | None = None
    try:
        source_uri, policy, immutable_guard = sqlite_open_policy(source)
        source_connection = sqlite3.connect(source_uri, uri=True, timeout=30)
        destination_connection = sqlite3.connect(str(destination), timeout=30)
        source_connection.backup(destination_connection, pages=0, sleep=0.1)
        destination_connection.commit()
        result = destination_connection.execute("PRAGMA quick_check").fetchone()
        if not result or str(result[0]).lower() != "ok":
            raise BKError(f"SQLite integrity check failed for {source}: {result!r}")
        if immutable_guard is not None:
            metadata = os.lstat(source)
            current_guard = (metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns)
            wal = source.with_name(source.name + "-wal")
            shm = source.with_name(source.name + "-shm")
            if current_guard != immutable_guard or os.path.lexists(wal) or os.path.lexists(shm):
                raise BKError(f"closed WAL database changed during its guarded snapshot: {source}")
        log_lines.append(f"SQLite snapshot: {source} -> {destination}")
        log_lines.append(f"SQLite source policy: {policy}")
        log_lines.append("SQLite backup result: online backup completed; PRAGMA quick_check: ok")
    except BKError:
        raise
    except (sqlite3.Error, OSError) as exc:
        raise BKError(f"SQLite backup failed for {source}: {exc}") from exc
    finally:
        if destination_connection is not None:
            try:
                destination_connection.close()
            except sqlite3.Error:
                pass
        if source_connection is not None:
            try:
                source_connection.close()
            except sqlite3.Error:
                pass
        # A destination connection should not leave stale SQLite sidecars in
        # the staged payload.  These are paths BK itself created under .work.
        for suffix in SQLITE_SIDECAR_SUFFIXES:
            sidecar = destination.with_name(destination.name + suffix)
            try:
                if sidecar.is_file() or sidecar.is_symlink():
                    sidecar.unlink()
            except OSError as exc:
                raise BKError(f"cannot remove staged SQLite sidecar {sidecar}: {exc}") from exc


def materialize_plan(
    plan: PlanSummary,
    run_directory: Path,
    log_lines: list[str] | None = None,
    progress: BackupProgress | None = None,
) -> tuple[Path, list[str]]:
    payload = run_directory / "payload"
    payload.mkdir(mode=0o700)
    if log_lines is None:
        log_lines = []
    for source_plan in plan.sources:
        root = payload / f"{source_plan.index:04d}" / source_label(source_plan.source)
        for entry in source_plan.entries:
            if entry.skip_sidecar:
                if progress is not None:
                    progress.advance()
                continue
            validate_plan_entry(entry)
            destination = entry_destination(payload, entry, source_plan.source)
            if entry.kind == "directory":
                try:
                    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
                except OSError as exc:
                    raise BKError(f"cannot create staged directory {destination}: {exc}") from exc
            elif entry.kind == "symlink":
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                copy_symlink(entry.source_path, destination)
            elif entry.kind == "file":
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                if not entry.classification:
                    raise BKError(f"missing classification for staged file {entry.source_path}")
                if entry.classification.handler == "sqlite":
                    log_lines.append(f"Classified SQLite: {entry.source_path}: {entry.classification.output}")
                    copy_sqlite_file(entry.source_path, destination, log_lines)
                else:
                    copy_regular_file(entry.source_path, destination)
            else:  # pragma: no cover - plan construction rejects this
                raise BKError(f"unsupported staged entry kind {entry.kind}: {entry.source_path}")
            if progress is not None:
                progress.advance()

    manifest = {
        "schema": "bk-archive",
        "version": 1,
        "sources": [
            {
                "index": source_plan.index,
                "path": str(source_plan.source),
                "kind": source_plan.root_kind,
                "payload": f"payload/{source_plan.index:04d}/{source_label(source_plan.source)}",
            }
            for source_plan in plan.sources
        ],
        "entries": [
            {
                "source_index": entry.source_index,
                "path": str(entry.source_path),
                "relative": str(entry.relative),
                "kind": entry.kind,
            }
            for source_plan in plan.sources
            for entry in source_plan.entries
        ],
    }
    atomic_write_bytes(run_directory / "manifest.json", (json.dumps(manifest, indent=2) + "\n").encode("utf-8"))
    return payload, log_lines


def directory_identity(path: Path, label: str) -> tuple[int, int]:
    try:
        metadata = os.lstat(path)
    except OSError as exc:
        raise BKError(f"cannot inspect BK {label} directory {path}: {exc}") from exc
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise BKError(f"BK {label} path is not a real directory: {path}")
    return metadata.st_dev, metadata.st_ino


def cleanup_run_directory(
    run_directory: Path,
    work_root: Path,
    *,
    expected_work_identity: tuple[int, int] | None = None,
    expected_run_identity: tuple[int, int] | None = None,
) -> None:
    """Remove exactly one BK-created run directory under the known work root."""

    run_directory = normalize_path(run_directory)
    work_root = normalize_path(work_root)
    if run_directory.parent != work_root or not run_directory.name.startswith("run-"):
        raise BKError(f"refusing unsafe staging cleanup path: {run_directory}")
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        work_descriptor = os.open(work_root, flags)
    except OSError as exc:
        raise BKError(f"cannot safely open BK work directory {work_root}: {exc}") from exc
    try:
        work_metadata = os.fstat(work_descriptor)
        work_identity = (work_metadata.st_dev, work_metadata.st_ino)
        if expected_work_identity is not None and work_identity != expected_work_identity:
            raise BKError(f"refusing cleanup because the BK work directory was replaced: {work_root}")
        try:
            run_metadata = os.stat(run_directory.name, dir_fd=work_descriptor, follow_symlinks=False)
        except FileNotFoundError:
            return
        except OSError as exc:
            raise BKError(f"cannot safely inspect staging directory {run_directory}: {exc}") from exc
        run_identity = (run_metadata.st_dev, run_metadata.st_ino)
        if not stat.S_ISDIR(run_metadata.st_mode) or stat.S_ISLNK(run_metadata.st_mode):
            raise BKError(f"refusing to remove replaced staging directory: {run_directory}")
        if expected_run_identity is not None and run_identity != expected_run_identity:
            raise BKError(f"refusing cleanup because the staging directory was replaced: {run_directory}")

        final_work = os.fstat(work_descriptor)
        final_run = os.stat(run_directory.name, dir_fd=work_descriptor, follow_symlinks=False)
        if (final_work.st_dev, final_work.st_ino) != work_identity:
            raise BKError(f"refusing cleanup because the BK work directory changed: {work_root}")
        if (final_run.st_dev, final_run.st_ino) != run_identity:
            raise BKError(f"refusing cleanup because the staging directory changed: {run_directory}")
        try:
            shutil.rmtree(run_directory.name, dir_fd=work_descriptor)
        except OSError as exc:
            raise BKError(f"cannot clean staging directory {run_directory}: {exc}") from exc
    finally:
        os.close(work_descriptor)


def make_archive_pair(paths: Paths, timestamp: datetime) -> ArchivePair:
    stamp = timestamp.astimezone().strftime("%Y-%m-%d_%H-%M-%S")
    used_collisions: set[int] = set()
    try:
        entries = list(paths.archive.iterdir())
    except OSError as exc:
        raise BKError(f"cannot inspect backup archive directory: {exc}") from exc
    for entry in entries:
        name = entry.name
        archive_name = name if name.endswith(".tgz") else name.removesuffix(".log") + ".tgz"
        match = ARCHIVE_RE.fullmatch(archive_name)
        if match and match.group("stamp") == stamp:
            used_collisions.add(int(match.group("collision") or 0))

    first_collision = max(used_collisions, default=-1) + 1
    for collision in range(first_collision, 10000):
        suffix = "" if collision == 0 else f"-{collision:02d}"
        stem = f"backup-{stamp}{suffix}"
        archive = paths.archive / f"{stem}.tgz"
        log = paths.archive / f"{stem}.log"
        if not os.path.lexists(archive) and not os.path.lexists(log):
            return ArchivePair(archive=archive, log=log, stamp=stamp + suffix)
    raise BKError(f"could not find an unused archive name for timestamp {stamp}")


def archive_candidates(paths: Paths) -> list[ArchivePair]:
    if not paths.archive.is_dir() or paths.archive.is_symlink():
        return []
    pairs: list[ArchivePair] = []
    try:
        entries = list(paths.archive.iterdir())
    except OSError as exc:
        raise BKError(f"cannot inspect backup archive directory: {exc}") from exc
    for archive in entries:
        match = ARCHIVE_RE.fullmatch(archive.name)
        if not match or not archive.is_file() or archive.is_symlink():
            continue
        log = archive.with_suffix(".log")
        if not log.is_file() or log.is_symlink():
            continue
        pairs.append(ArchivePair(archive=archive, log=log, stamp=match.group(0)[len("backup-") : -len(".tgz")]))
    def sort_key(pair: ArchivePair) -> tuple[str, int]:
        match = ARCHIVE_RE.fullmatch(pair.archive.name)
        if match is None:  # pragma: no cover - candidates were already matched
            return (pair.archive.name, 0)
        return (match.group("stamp"), int(match.group("collision") or 0))

    return sorted(pairs, key=sort_key)


def prune_archives(paths: Paths, retention: int) -> int:
    pairs = archive_candidates(paths)
    if len(pairs) <= retention:
        return len(pairs)
    current_inode: tuple[int, int] | None = None
    try:
        current_stat = os.stat(paths.current_archive)
        current_inode = (current_stat.st_dev, current_stat.st_ino)
    except OSError:
        pass
    while len(pairs) > retention:
        victim_index: int | None = None
        for index, pair in enumerate(pairs):
            if current_inode is not None:
                try:
                    metadata = os.stat(pair.archive)
                except OSError as exc:
                    raise BKError(f"cannot inspect archive {pair.archive}: {exc}") from exc
                if (metadata.st_dev, metadata.st_ino) == current_inode:
                    continue
            victim_index = index
            break
        if victim_index is None:
            raise BKError("retention cannot be applied without removing the current backup")
        victim = pairs.pop(victim_index)
        try:
            victim.archive.unlink()
            victim.log.unlink()
        except OSError as exc:
            raise BKError(f"cannot prune old backup {victim.archive}: {exc}") from exc
    return len(pairs)


def create_gzip_tar(payload: Path, archive_temporary: Path) -> None:
    try:
        with tarfile.open(archive_temporary, mode="w:gz", compresslevel=6) as archive:
            archive.add(payload.parent / "manifest.json", arcname="manifest.json", recursive=False)
            archive.add(payload, arcname="payload", recursive=True)
    except (OSError, tarfile.TarError) as exc:
        raise BKError(f"cannot create compressed backup archive: {exc}") from exc
    try:
        with archive_temporary.open("rb") as handle:
            os.fsync(handle.fileno())
    except OSError as exc:
        raise BKError(f"cannot flush compressed backup archive: {exc}") from exc


def write_archive_log(path: Path, content: str) -> None:
    atomic_write_bytes(path, content.encode("utf-8"), mode=0o600)


def _managed_regular_file(path: Path) -> bool:
    try:
        return stat.S_ISREG(os.lstat(path).st_mode)
    except OSError:
        return False


def publish_current_links(paths: Paths, pair: ArchivePair) -> None:
    """Atomically replace both current hardlinks, rolling back on a log failure."""

    token = f"{os.getpid()}-{time.time_ns()}"
    archive_link = paths.runtime / f".backup.tgz.{token}.new"
    log_link = paths.runtime / f".backup.log.{token}.new"
    old_archive_link = paths.runtime / f".backup.tgz.{token}.old"
    old_log_link = paths.runtime / f".backup.log.{token}.old"
    archive_replaced = False
    log_replaced = False
    try:
        os.link(pair.archive, archive_link)
        os.link(pair.log, log_link)
        if os.path.lexists(paths.current_archive):
            if not _managed_regular_file(paths.current_archive):
                raise BKError(f"current backup path is not a regular file: {paths.current_archive}")
            os.link(paths.current_archive, old_archive_link)
        if os.path.lexists(paths.current_log):
            if not _managed_regular_file(paths.current_log):
                raise BKError(f"current log path is not a regular file: {paths.current_log}")
            os.link(paths.current_log, old_log_link)
        os.replace(archive_link, paths.current_archive)
        archive_replaced = True
        os.replace(log_link, paths.current_log)
        log_replaced = True
    except (OSError, BKError) as exc:
        if log_replaced:
            # Both paths were replaced; the normal cleanup below is enough.
            pass
        elif archive_replaced:
            try:
                if os.path.lexists(old_archive_link):
                    os.replace(old_archive_link, paths.current_archive)
                else:
                    paths.current_archive.unlink(missing_ok=True)
            except OSError:
                pass
        raise BKError(f"cannot publish current backup links: {exc}") from exc
    finally:
        for temporary in (archive_link, log_link, old_archive_link, old_log_link):
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
            except OSError:
                pass


def iso_timestamp(value: datetime) -> str:
    return value.astimezone().isoformat(timespec="seconds")


def history_entries(paths: Paths) -> list[dict[str, Any]]:
    history: list[dict[str, Any]] = []
    for pair in reversed(archive_candidates(paths)):
        try:
            archive_stat = pair.archive.stat()
            created = datetime.fromtimestamp(archive_stat.st_mtime, tz=timezone.utc).astimezone().isoformat(
                timespec="seconds"
            )
        except OSError:
            continue
        history.append(
            {
                "archive_path": str(pair.archive),
                "log_path": str(pair.log),
                "filename": pair.archive.name,
                "size": archive_stat.st_size,
                "created_at": created,
            }
        )
    return history


def current_backup_info(paths: Paths, history: list[dict[str, Any]]) -> dict[str, Any]:
    if not _managed_regular_file(paths.current_archive) or not _managed_regular_file(paths.current_log):
        return {
            "current_backup_path": None,
            "current_backup_size": None,
            "current_successful_log_path": None,
            "current_backup_created_at": None,
        }
    try:
        current_archive_stat = os.stat(paths.current_archive)
        current_log_stat = os.stat(paths.current_log)
    except OSError:
        return {
            "current_backup_path": None,
            "current_backup_size": None,
            "current_successful_log_path": None,
            "current_backup_created_at": None,
        }
    matching = None
    for entry in history:
        try:
            archive_stat = os.stat(entry["archive_path"])
            log_stat = os.stat(entry["log_path"])
        except OSError:
            continue
        if (
            (archive_stat.st_dev, archive_stat.st_ino) == (current_archive_stat.st_dev, current_archive_stat.st_ino)
            and (log_stat.st_dev, log_stat.st_ino) == (current_log_stat.st_dev, current_log_stat.st_ino)
        ):
            matching = entry
            break
    if matching is None:
        return {
            "current_backup_path": None,
            "current_backup_size": None,
            "current_successful_log_path": None,
            "current_backup_created_at": None,
        }
    return {
        "current_backup_path": str(paths.current_archive),
        "current_backup_size": current_archive_stat.st_size,
        "current_successful_log_path": str(paths.current_log),
        "current_backup_created_at": matching["created_at"],
    }


def read_existing_status(paths: Paths) -> dict[str, Any] | None:
    if not os.path.lexists(paths.status):
        return None
    if paths.status.is_symlink() or not paths.status.is_file():
        raise BKError(f"status path is not a regular file: {paths.status}")
    try:
        value = json.loads(paths.status.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise BKError(f"status.json is malformed: {exc}") from exc
    if not isinstance(value, dict) or value.get("version") != STATUS_VERSION:
        raise BKError("status.json has an unsupported schema version")
    return value


def make_status(
    paths: Paths,
    config: Config,
    status_value: str,
    started: datetime,
    finished: datetime,
    duration: float,
    error: str | None,
    failed_path: Path | None,
) -> dict[str, Any]:
    history = history_entries(paths)
    current = current_backup_info(paths, history)
    last_success = None
    if history:
        last_success = history[0]["created_at"]
    failed_log_path = str(failed_path) if failed_path is not None else (
        str(paths.failed_log) if _managed_regular_file(paths.failed_log) else None
    )
    result: dict[str, Any] = {
        "schema": "bk-status",
        "version": STATUS_VERSION,
        "status": status_value,
        "configured": True,
        "attempt": {
            "started_at": iso_timestamp(started),
            "finished_at": iso_timestamp(finished),
            "duration_seconds": round(duration, 3),
        },
        "last_attempt_started_at": iso_timestamp(started),
        "last_attempt_finished_at": iso_timestamp(finished),
        "last_attempt_duration_seconds": round(duration, 3),
        "last_successful_backup_timestamp": last_success,
        "configured_retention": config.retention,
        "retained_successful_archives": len(history),
        "history": history,
        "error": error,
        "failed_log_path": failed_log_path,
    }
    result.update(current)
    return result


def write_status(paths: Paths, status: dict[str, Any]) -> None:
    atomic_write_bytes(paths.status, (json.dumps(status, indent=2, ensure_ascii=False) + "\n").encode("utf-8"), mode=0o600)


def write_failure_log(paths: Paths, content: str) -> None:
    atomic_write_bytes(paths.failed_log, content.encode("utf-8"), mode=0o600)


def remove_unpublished_pair(pair: ArchivePair | None) -> None:
    """Remove only a newly generated pair that was never made current."""

    if pair is None:
        return
    for path in (pair.archive, pair.log):
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        except OSError:
            pass


def create_run_log(
    started: datetime,
    finished: datetime,
    duration: float,
    config: Config,
    plan: PlanSummary,
    pair: ArchivePair,
    archive_size: int,
    log_lines: list[str],
    retained: int,
) -> str:
    ratio = "n/a" if plan.input_bytes == 0 else f"{archive_size / plan.input_bytes:.3f}"
    lines = [
        "BK backup run",
        f"Started: {iso_timestamp(started)}",
        f"Finished: {iso_timestamp(finished)}",
        f"Duration seconds: {duration:.3f}",
        f"Configured sources: {len(config.sources)}",
        f"Discovered files: {plan.file_count}",
        f"Discovered directories: {plan.directory_count}",
        f"Discovered symlinks: {plan.symlink_count}",
        f"Input bytes: {plan.input_bytes}",
        f"Classified regular files: {plan.classifications}",
        f"SQLite handlers used: {plan.sqlite_count}",
        f"Skipped SQLite sidecars: {plan.skipped_sidecars}",
        f"Archive: {pair.archive}",
        f"Archive size: {archive_size}",
        f"Compression ratio (archive/input): {ratio}",
        f"Retained successful archives: {retained}",
    ]
    if log_lines:
        lines.append("Special handlers:")
        lines.extend(f"  {line}" for line in log_lines)
    lines.append("Status: OK")
    return "\n".join(lines) + "\n"


def create_failure_log(
    started: datetime,
    finished: datetime,
    duration: float,
    config: Config,
    error: str,
    details: list[str],
) -> str:
    lines = [
        "BK backup run",
        f"Started: {iso_timestamp(started)}",
        f"Finished: {iso_timestamp(finished)}",
        f"Duration seconds: {duration:.3f}",
        f"Configured sources: {len(config.sources)}",
        f"Status: FAILED",
        f"Error: {error}",
    ]
    if details:
        lines.append("Diagnostics:")
        lines.extend(f"  {detail}" for detail in details)
    return "\n".join(lines) + "\n"


@contextmanager
def backup_lock(paths: Paths) -> Iterator[None]:
    try:
        descriptor = os.open(paths.lock, os.O_RDWR | os.O_CREAT, 0o600)
    except OSError as exc:
        raise BKError(f"cannot open BK run lock {paths.lock}: {exc}") from exc
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            if exc.errno in (errno.EACCES, errno.EAGAIN):
                raise BackupAlreadyRunning("a backup is already running") from exc
            raise BKError(f"cannot acquire BK run lock: {exc}") from exc
        yield
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


def run_backup(
    paths: Paths,
    config: Config,
    *,
    already_locked: bool,
    progress: BackupProgress | None = None,
) -> tuple[int, dict[str, Any]]:
    if not already_locked:
        raise BKError("internal backup execution requires the caller to hold the run lock")
    started = datetime.now().astimezone()
    started_monotonic = time.monotonic()
    run_directory: Path | None = None
    details: list[str] = []
    plan: PlanSummary | None = None
    pair: ArchivePair | None = None
    pair_created = False
    current_published = False
    archive_temporary: Path | None = None
    work_identity: tuple[int, int] | None = None
    run_identity: tuple[int, int] | None = None
    try:
        ensure_archive_and_work_directories(paths)
        work_identity = directory_identity(paths.work, "work")
        with nullcontext():
            run_directory = Path(tempfile.mkdtemp(prefix="run-", dir=paths.work))
            run_identity = directory_identity(run_directory, "staging")
            try:
                if progress is not None:
                    progress.stage("Scanning and classifying sources")
                plan = build_snapshot_plan(config, progress)
                if progress is not None:
                    entry_count = sum(len(source.entries) for source in plan.sources)
                    progress.stage("Creating staged snapshot", total=entry_count)
                payload, _ = materialize_plan(plan, run_directory, details, progress)
                manifest = run_directory / "manifest.json"
                if not manifest.is_file():
                    raise BKError("staging manifest was not created")

                pair = make_archive_pair(paths, datetime.now().astimezone())
                descriptor, temporary_name = tempfile.mkstemp(
                    prefix=f".{pair.archive.name}.", suffix=".tmp", dir=paths.archive
                )
                os.close(descriptor)
                archive_temporary = Path(temporary_name)
                if progress is not None:
                    progress.stage("Compressing snapshot")
                create_gzip_tar(payload, archive_temporary)
                archive_size = archive_temporary.stat().st_size
                cleanup_run_directory(
                    run_directory,
                    paths.work,
                    expected_work_identity=work_identity,
                    expected_run_identity=run_identity,
                )
                run_directory = None

                existing_count = len(archive_candidates(paths))
                expected_retained = min(config.retention, existing_count + 1)
                finished = datetime.now().astimezone()
                duration = time.monotonic() - started_monotonic
                log_text = create_run_log(
                    started,
                    finished,
                    duration,
                    config,
                    plan,
                    pair,
                    archive_size,
                    details,
                    expected_retained,
                )
                log_temporary = paths.archive / f".{pair.log.name}.{os.getpid()}-{time.time_ns()}.tmp"
                write_archive_log(log_temporary, log_text)
                archive_linked = False
                log_linked = False
                try:
                    os.link(archive_temporary, pair.archive)
                    archive_linked = True
                    os.link(log_temporary, pair.log)
                    log_linked = True
                    archive_temporary.unlink()
                    archive_temporary = None
                    log_temporary.unlink()
                    pair_created = True
                except OSError as exc:
                    if log_linked:
                        try:
                            pair.log.unlink()
                        except FileNotFoundError:
                            pass
                    if archive_linked:
                        try:
                            pair.archive.unlink()
                        except FileNotFoundError:
                            pass
                    try:
                        log_temporary.unlink()
                    except FileNotFoundError:
                        pass
                    raise BKError(f"cannot publish timestamped backup pair: {exc}") from exc

                if progress is not None:
                    progress.stage("Publishing backup")
                publish_current_links(paths, pair)
                current_published = True
                retained = prune_archives(paths, config.retention)
                finished = datetime.now().astimezone()
                duration = time.monotonic() - started_monotonic
                # The paired log is complete before current-link publication;
                # retention is deterministic and reflected in status.json.
                status = make_status(paths, config, "ok", started, finished, duration, None, None)
                status["retained_successful_archives"] = retained
                write_status(paths, status)
                if progress is not None:
                    progress.stage("Backup ready", total=1)
                    progress.advance()
                return 0, status
            finally:
                if archive_temporary is not None:
                    try:
                        archive_temporary.unlink()
                    except FileNotFoundError:
                        pass
                if run_directory is not None:
                    cleanup_run_directory(
                        run_directory,
                        paths.work,
                        expected_work_identity=work_identity,
                        expected_run_identity=run_identity,
                    )
                    run_directory = None
    except BKError as exc:
        if current_published:
            finished = datetime.now().astimezone()
            duration = time.monotonic() - started_monotonic
            warning = f"backup was published, but post-publication maintenance failed: {exc}"
            try:
                status = make_status(paths, config, "ok", started, finished, duration, None, None)
                status["warning"] = warning
                write_status(paths, status)
            except BKError as status_error:
                status = {
                    "schema": "bk-status",
                    "version": STATUS_VERSION,
                    "status": "ok",
                    "configured": True,
                    "current_backup_path": str(paths.current_archive),
                    "current_successful_log_path": str(paths.current_log),
                    "warning": f"{warning}; status update also failed: {status_error}",
                }
            return 0, status
        if not current_published:
            remove_unpublished_pair(pair if pair_created else None)
        finished = datetime.now().astimezone()
        duration = time.monotonic() - started_monotonic
        error = str(exc)
        if plan is not None:
            details.append(
                f"Plan summary: files={plan.file_count}, directories={plan.directory_count}, "
                f"symlinks={plan.symlink_count}, sqlite={plan.sqlite_count}"
            )
        try:
            write_failure_log(
                paths,
                create_failure_log(started, finished, duration, config, error, details),
            )
            status = make_status(paths, config, "failed", started, finished, duration, error, paths.failed_log)
            write_status(paths, status)
        except BKError as status_error:
            error = f"{error}; additionally could not update failure diagnostics: {status_error}"
            status = {
                "schema": "bk-status",
                "version": STATUS_VERSION,
                "status": "failed",
                "configured": True,
                "error": error,
            }
        return 1, status
    except Exception as exc:  # pragma: no cover - defensive boundary for guest reliability
        if current_published:
            finished = datetime.now().astimezone()
            duration = time.monotonic() - started_monotonic
            warning = f"backup was published, but unexpected post-publication maintenance failed: {exc}"
            try:
                status = make_status(paths, config, "ok", started, finished, duration, None, None)
                status["warning"] = warning
                write_status(paths, status)
            except BKError as status_error:
                status = {
                    "schema": "bk-status",
                    "version": STATUS_VERSION,
                    "status": "ok",
                    "configured": True,
                    "current_backup_path": str(paths.current_archive),
                    "current_successful_log_path": str(paths.current_log),
                    "warning": f"{warning}; status update also failed: {status_error}",
                }
            return 0, status
        if not current_published:
            remove_unpublished_pair(pair if pair_created else None)
        finished = datetime.now().astimezone()
        duration = time.monotonic() - started_monotonic
        error = f"unexpected BK failure: {exc}"
        details.append("".join(traceback.format_exception_only(type(exc), exc)).strip())
        try:
            write_failure_log(paths, create_failure_log(started, finished, duration, config, error, details))
            status = make_status(paths, config, "failed", started, finished, duration, error, paths.failed_log)
            write_status(paths, status)
        except BKError:
            status = {"schema": "bk-status", "version": STATUS_VERSION, "status": "failed", "error": error}
        return 1, status


def status_without_configuration(json_mode: bool) -> int:
    result = {
        "schema": "bk-status",
        "version": STATUS_VERSION,
        "status": "unconfigured",
        "configured": False,
        "attempt": None,
        "last_attempt_started_at": None,
        "last_attempt_finished_at": None,
        "last_attempt_duration_seconds": None,
        "last_successful_backup_timestamp": None,
        "current_backup_path": None,
        "current_backup_size": None,
        "current_successful_log_path": None,
        "failed_log_path": None,
        "configured_retention": None,
        "retained_successful_archives": 0,
        "history": [],
        "error": None,
    }
    if json_mode:
        emit_json(result)
    else:
        console.print("Backup is unconfigured; no backup.yaml exists and no backup has run.")
    return 0


def record_config_failure(paths: Paths, started: datetime, started_monotonic: float, error: str) -> dict[str, Any]:
    """Record a failed run whose configuration could not be parsed."""

    ensure_runtime_directory(paths)
    finished = datetime.now().astimezone()
    duration = time.monotonic() - started_monotonic
    failure_log = "\n".join(
        (
            "BK backup run",
            f"Started: {iso_timestamp(started)}",
            f"Finished: {iso_timestamp(finished)}",
            f"Duration seconds: {duration:.3f}",
            "Configured sources: unavailable because backup.yaml is invalid",
            "Status: FAILED",
            f"Error: {error}",
            "",
        )
    )
    write_failure_log(paths, failure_log)
    history = history_entries(paths) if paths.archive.is_dir() and not paths.archive.is_symlink() else []
    status: dict[str, Any] = {
        "schema": "bk-status",
        "version": STATUS_VERSION,
        "status": "failed",
        "configured": True,
        "attempt": {
            "started_at": iso_timestamp(started),
            "finished_at": iso_timestamp(finished),
            "duration_seconds": round(duration, 3),
        },
        "last_attempt_started_at": iso_timestamp(started),
        "last_attempt_finished_at": iso_timestamp(finished),
        "last_attempt_duration_seconds": round(duration, 3),
        "last_successful_backup_timestamp": history[0]["created_at"] if history else None,
        "current_backup_path": None,
        "current_backup_size": None,
        "current_successful_log_path": None,
        "current_backup_created_at": None,
        "failed_log_path": str(paths.failed_log),
        "configured_retention": None,
        "retained_successful_archives": len(history),
        "history": history,
        "error": error,
    }
    status.update(current_backup_info(paths, history))
    write_status(paths, status)
    return status


def status_command(paths: Paths, json_mode: bool) -> int:
    config = read_config(paths)
    if config is None:
        return status_without_configuration(json_mode)
    existing = read_existing_status(paths)
    if existing is None:
        history = history_entries(paths) if paths.archive.is_dir() else []
        result = {
            "schema": "bk-status",
            "version": STATUS_VERSION,
            "status": "unconfigured",
            "configured": True,
            "attempt": None,
            "last_attempt_started_at": None,
            "last_attempt_finished_at": None,
            "last_attempt_duration_seconds": None,
            "last_successful_backup_timestamp": history[0]["created_at"] if history else None,
            "configured_retention": config.retention,
            "retained_successful_archives": len(history),
            "history": history,
            "error": None,
            "failed_log_path": str(paths.failed_log) if _managed_regular_file(paths.failed_log) else None,
        }
        result.update(current_backup_info(paths, history))
    else:
        history = history_entries(paths)
        result = dict(existing)
        result["configured_retention"] = config.retention
        result["retained_successful_archives"] = len(history)
        result["history"] = history
        result["last_successful_backup_timestamp"] = history[0]["created_at"] if history else None
        result.update(current_backup_info(paths, history))
    if json_mode:
        emit_json(result)
        return 0

    render_status(result, paths)
    return 0


def render_status(status: dict[str, Any], paths: Paths) -> None:
    value = str(status.get("status", "unknown")).upper()
    console.print(f"[bold]Last attempt[/bold]\n  Status       {value}")
    attempt = status.get("attempt") or {}
    if attempt:
        console.print(f"  Started      {attempt.get('started_at', 'unknown')}")
        console.print(f"  Finished     {attempt.get('finished_at', 'unknown')}")
        console.print(f"  Duration     {attempt.get('duration_seconds', 'unknown')} seconds")
    if status.get("error"):
        console.print(f"  Error        {status['error']}")
    if status.get("failed_log_path") and value == "FAILED":
        console.print(f"  Log          {status['failed_log_path']}")

    console.print("\n[bold]Last successful backup[/bold]")
    current_path = status.get("current_backup_path")
    if current_path:
        console.print(f"  Created      {status.get('current_backup_created_at', 'unknown')}")
        console.print(f"  Size         {status.get('current_backup_size', 'unknown')} bytes")
        console.print(f"  Archive      {current_path}")
        console.print(f"  Log          {status.get('current_successful_log_path', paths.current_log)}")
    else:
        console.print("  None")
    if value == "FAILED" and current_path:
        console.print("The last attempt failed; the displayed current backup is the older valid backup.")

    history = status.get("history") or []
    console.print(f"\n[bold]Retained successful backups ({len(history)})[/bold]")
    if history:
        table = Table(show_header=True, header_style="bold")
        table.add_column("Archive")
        table.add_column("Created")
        table.add_column("Size", justify="right")
        for entry in history:
            table.add_row(str(entry.get("filename", entry.get("archive_path", ""))), str(entry.get("created_at", "")), f"{entry.get('size', 0)} bytes")
        console.print(table)
    else:
        console.print("No successful backups yet.")


def emit_json(value: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n")


def run_command(paths: Paths, json_mode: bool) -> int:
    def unconfigured_result() -> dict[str, Any]:
        return {
            "schema": "bk-status",
            "version": STATUS_VERSION,
            "status": "unconfigured",
            "configured": False,
            "result": "no-op",
            "message": "No backup configuration exists; nothing was backed up.",
        }

    if not os.path.lexists(paths.config):
        code = 0
        result = unconfigured_result()
    else:
        ensure_runtime_directory(paths)
        try:
            with backup_lock(paths):
                started = datetime.now().astimezone()
                started_monotonic = time.monotonic()
                try:
                    config = read_config(paths)
                except ConfigError as exc:
                    code = 1
                    result = record_config_failure(paths, started, started_monotonic, str(exc))
                else:
                    if config is None:
                        code = 0
                        result = unconfigured_result()
                    else:
                        if json_mode:
                            code, result = run_backup(paths, config, already_locked=True)
                        else:
                            with BackupProgress() as progress:
                                code, result = run_backup(
                                    paths,
                                    config,
                                    already_locked=True,
                                    progress=progress,
                                )
        except BackupAlreadyRunning as exc:
            code = 75
            result = {
                "schema": "bk-status",
                "version": STATUS_VERSION,
                "status": "failed",
                "configured": True,
                "error": str(exc),
                "busy": True,
            }

    if code == 0 and result["status"] == "unconfigured":
        if json_mode:
            emit_json(result)
        else:
            console.print(result["message"])
        return 0
    if json_mode:
        emit_json(result)
    elif code == 0:
        console.print(f"Backup completed: {result.get('current_backup_path', paths.current_archive)}")
        console.print(f"Log: {result.get('current_successful_log_path', paths.current_log)}")
        if result.get("warning"):
            error_console.print(f"Warning: {result['warning']}")
    else:
        error_console.print(f"Backup failed: {result.get('error', 'unknown error')}")
        if result.get("failed_log_path"):
            error_console.print(f"Failure log: {result['failed_log_path']}")
        if result.get("current_backup_path"):
            error_console.print("The previous valid current backup was preserved.")
    return code


def main(argv: list[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if not arguments or arguments == ["--help"] or arguments == ["-h"]:
        print_help()
        return 0
    command = arguments.pop(0)
    aliases = {"h": "help", "help": "help", "l": "list", "e": "edit", "r": "run", "s": "status"}
    command = aliases.get(command, command)
    if command == "help":
        if arguments:
            raise BKError("help does not accept additional arguments")
        print_help()
        return 0
    if command not in {"list", "edit", "run", "status"}:
        raise BKError(f"unknown command: {command}")
    _, json_mode = command_json(arguments)
    if json_mode and command == "edit":
        raise BKError("--json is not supported for edit")
    paths = Paths.from_home()
    if command == "list":
        return list_command(paths, json_mode)
    if command == "edit":
        return edit_command(paths)
    if command == "run":
        return run_command(paths, json_mode)
    return status_command(paths, json_mode)


if __name__ == "__main__":
    raw_arguments = sys.argv[1:]
    try:
        raise SystemExit(main())
    except BKError as exc:
        # Keep stdout machine-readable when a supported JSON command fails
        # before its command handler can emit a result (for example, a
        # malformed backup.yaml).
        command = raw_arguments[0] if raw_arguments else ""
        if "--json" in raw_arguments and command in {"list", "l", "run", "r", "status", "s"}:
            emit_json(
                {
                    "schema": "bk-status",
                    "version": STATUS_VERSION,
                    "status": "failed",
                    "configured": None,
                    "error": str(exc),
                }
            )
        else:
            error_console.print(f"BK error: {exc}")
        raise SystemExit(2)
