"""Config support for HomeStack."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import os
from pathlib import Path
import json
import tempfile
from typing import Any
import re
import tomllib

from .models import AppError


@dataclass(frozen=True)
class WorkspaceSSHConfig:
    user: str
    identity_files: tuple[str, ...]
    identities_only: bool
    log_level: str


@dataclass(frozen=True)
class Config:
    path: Path
    transport_type: str
    node: str
    control_node: str
    gold_vmid: int
    storage_layouts: dict[str, tuple[str, ...]]
    root_storage: str
    root_disk: str
    home_storage: str
    home_disk: str
    default_home_size: str
    network_prefix: str
    network_cidr: int
    gateway: str
    dns_servers: tuple[str, ...]
    snippet_storage: str
    snippet_dir: Path
    user_name: str
    user_uid: int
    user_gid: int
    workspace_ssh: WorkspaceSSHConfig
    herdr_workspace: str
    herdr_tab: str
    herdr_debug: bool
    storage_display_unit: str
    storage_display_decimals: int
    sync_paths: tuple[str, ...] = ()
    sync_commands: tuple[str, ...] = ()
    sync_verbose: bool = False


def default_config_path() -> Path:
    config_home = os.environ.get("XDG_CONFIG_HOME")
    base = Path(config_home).expanduser() if config_home else Path.home() / ".config"
    return base / "homestack" / "config.toml"


DEFAULT_CONFIG = default_config_path()


HOMESTACK_STORAGE_RE = re.compile(r"^homestack-storage-[1-9][0-9]*$")


def validate_sync_path_spec(value: str) -> tuple[str, bool]:
    if not value.startswith("~/"):
        raise AppError(f"Sync path must start with '~/': {value!r}")
    if "\x00" in value or "\n" in value or "\r" in value:
        raise AppError(f"Sync path contains an invalid control character: {value!r}")

    relative = value[2:]
    is_directory = relative.endswith("/")
    relative_core = relative[:-1] if is_directory else relative
    if not relative_core:
        raise AppError("Syncing the entire home directory ('~/') is not allowed")

    parts = relative_core.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise AppError(
            f"Sync path must be a normalized path below '~/' without '.' or '..': {value!r}"
        )
    return relative_core, is_directory


def _need(data: dict[str, Any], section: str, key: str) -> Any:
    try:
        return data[section][key]
    except KeyError as exc:
        raise AppError(f"Missing configuration value [{section}] {key}") from exc


def load_config(path: Path) -> Config:
    try:
        with path.open("rb") as handle:
            data = tomllib.load(handle)
    except FileNotFoundError as exc:
        raise AppError(f"Configuration file not found: {path}") from exc
    except tomllib.TOMLDecodeError as exc:
        raise AppError(f"Invalid TOML configuration {path}: {exc}") from exc

    version = data.get("version")
    if version != 1:
        raise AppError(f"Unsupported config version: {version!r}; expected 1")

    dns = tuple(str(x) for x in _need(data, "network", "dns"))
    if not dns:
        raise AppError("[network] dns must contain at least one DNS server")

    node = str(_need(data, "node", "name"))

    raw_layouts = data.get("storage_layouts") or {}
    if not isinstance(raw_layouts, dict):
        raise AppError("[storage_layouts] must be a TOML table")
    storage_layouts: dict[str, tuple[str, ...]] = {}
    for layout_name, raw_storages in raw_layouts.items():
        layout = str(layout_name)
        if HOMESTACK_STORAGE_RE.fullmatch(layout) is None:
            raise AppError(
                f"Invalid HomeStack storage layout name {layout!r}; "
                "expected homestack-storage-<N>"
            )
        if not isinstance(raw_storages, list) or not raw_storages:
            raise AppError(f"[storage_layouts] {layout} must be a non-empty array")
        storages = tuple(str(item).strip() for item in raw_storages)
        if any(not item for item in storages):
            raise AppError(f"[storage_layouts] {layout} contains an empty storage ID")
        if len(set(storages)) != len(storages):
            raise AppError(f"[storage_layouts] {layout} contains duplicate storage IDs")
        storage_layouts[layout] = storages

    control = data.get("control") or {}
    if not isinstance(control, dict):
        raise AppError("[control] must be a TOML table")
    control_node = str(control.get("node", node))

    transport = data.get("transport") or {}
    if not isinstance(transport, dict):
        raise AppError("[transport] must be a TOML table")
    transport_type = str(transport.get("type", "")).strip()
    if not transport_type:
        raise AppError("Missing configuration value [transport] type")
    if transport_type != "herdr":
        raise AppError(
            f"Unsupported transport type {transport_type!r}; only 'herdr' is implemented"
        )

    herdr = transport.get("herdr") or {}
    if not isinstance(herdr, dict):
        raise AppError("[transport.herdr] must be a TOML table")
    herdr_debug = herdr.get("debug", True)
    if not isinstance(herdr_debug, bool):
        raise AppError("[transport.herdr] debug must be true or false")
    herdr_workspace = str(herdr.get("workspace", "")).strip()
    if not herdr_workspace:
        raise AppError("Missing configuration value [transport.herdr] workspace")
    herdr_tab = str(herdr.get("tab", "")).strip()
    if not herdr_tab:
        raise AppError("Missing configuration value [transport.herdr] tab")

    display = data.get("display") or {}
    if not isinstance(display, dict):
        raise AppError("[display] must be a TOML table")
    storage_display_unit = str(display.get("storage_unit", "GiB"))
    if storage_display_unit not in {"KiB", "MiB", "GiB", "TiB", "PiB"}:
        raise AppError(
            "[display] storage_unit must be one of KiB, MiB, GiB, TiB, PiB"
        )
    try:
        storage_display_decimals = int(display.get("storage_decimals", 0))
    except (TypeError, ValueError) as exc:
        raise AppError("[display] storage_decimals must be an integer") from exc
    if not 0 <= storage_display_decimals <= 4:
        raise AppError("[display] storage_decimals must be between 0 and 4")

    sync = data.get("sync") or {}
    if not isinstance(sync, dict):
        raise AppError("[sync] must be a TOML table")
    raw_sync_paths = sync.get("paths", [])
    if not isinstance(raw_sync_paths, list):
        raise AppError("[sync] paths must be an array")
    sync_paths_list: list[str] = []
    for raw_path in raw_sync_paths:
        if not isinstance(raw_path, str):
            raise AppError("[sync] paths entries must be strings")
        value = raw_path.strip()
        validate_sync_path_spec(value)
        sync_paths_list.append(value)
    if len(set(sync_paths_list)) != len(sync_paths_list):
        raise AppError("[sync] paths contains duplicate entries")
    sync_paths = tuple(sync_paths_list)
    raw_sync_commands = sync.get("commands", [])
    if not isinstance(raw_sync_commands, list):
        raise AppError("[sync] commands must be an array")
    sync_commands_list: list[str] = []
    for raw_command in raw_sync_commands:
        if not isinstance(raw_command, str):
            raise AppError("[sync] commands entries must be strings")
        command = raw_command.strip()
        if not command:
            raise AppError("[sync] commands entries must not be empty")
        sync_commands_list.append(command)
    sync_commands = tuple(sync_commands_list)
    sync_verbose = sync.get("verbose", False)
    if not isinstance(sync_verbose, bool):
        raise AppError("[sync] verbose must be true or false")

    workspace_ssh_data = data.get("workspace_ssh")
    if not isinstance(workspace_ssh_data, dict):
        raise AppError("Missing or invalid [workspace_ssh] configuration table")

    ssh_user_raw = workspace_ssh_data.get("user")
    if not isinstance(ssh_user_raw, str) or not ssh_user_raw.strip():
        raise AppError("[workspace_ssh] user must be a non-empty string")
    ssh_user = ssh_user_raw.strip()
    user_name_raw = _need(data, "user", "name")
    if not isinstance(user_name_raw, str) or not user_name_raw.strip():
        raise AppError("[user] name must be a non-empty string")
    user_name = user_name_raw.strip()
    if ssh_user != user_name:
        raise AppError(
            f"[workspace_ssh] user {ssh_user!r} must match [user] name {user_name!r}"
        )

    raw_identity_files = workspace_ssh_data.get("identity_files")
    if not isinstance(raw_identity_files, list) or not raw_identity_files:
        raise AppError("[workspace_ssh] identity_files must be a non-empty array")
    identity_files_list: list[str] = []
    for raw_identity_file in raw_identity_files:
        if not isinstance(raw_identity_file, str):
            raise AppError("[workspace_ssh] identity_files entries must be strings")
        identity_file = raw_identity_file.strip()
        if (
            not identity_file
            or any(ch in identity_file for ch in ("\x00", "\n", "\r"))
            or any(ch.isspace() for ch in identity_file)
        ):
            raise AppError(
                "[workspace_ssh] identity_files entries must be non-empty paths "
                "without whitespace or control characters"
            )
        identity_files_list.append(identity_file)
    if len(set(identity_files_list)) != len(identity_files_list):
        raise AppError("[workspace_ssh] identity_files contains duplicate entries")

    identities_only = workspace_ssh_data.get("identities_only")
    if not isinstance(identities_only, bool):
        raise AppError("[workspace_ssh] identities_only must be true or false")

    log_level_raw = workspace_ssh_data.get("log_level")
    if not isinstance(log_level_raw, str) or not log_level_raw.strip():
        raise AppError("[workspace_ssh] log_level must be a non-empty string")
    log_level = log_level_raw.strip().upper()
    valid_log_levels = {
        "QUIET",
        "FATAL",
        "ERROR",
        "INFO",
        "VERBOSE",
        "DEBUG",
        "DEBUG1",
        "DEBUG2",
        "DEBUG3",
    }
    if log_level not in valid_log_levels:
        raise AppError(
            "[workspace_ssh] log_level must be one of "
            + ", ".join(sorted(valid_log_levels))
        )

    workspace_ssh = WorkspaceSSHConfig(
        user=ssh_user,
        identity_files=tuple(identity_files_list),
        identities_only=identities_only,
        log_level=log_level,
    )

    node_match = re.search(r"([1-9][0-9]*)$", node)
    if node_match is None:
        raise AppError(
            f"Cannot derive HomeStack storage layout from node name {node!r}; "
            "expected a numeric node suffix"
        )
    active_layout_name = f"homestack-storage-{node_match.group(1)}"
    active_layout = storage_layouts.get(active_layout_name)
    if active_layout is None:
        raise AppError(
            f"Missing [storage_layouts] {active_layout_name} for configured node {node}"
        )
    default_storage = active_layout[0]

    return Config(
        path=path,
        transport_type=transport_type,
        node=node,
        control_node=control_node,
        gold_vmid=int(_need(data, "node", "gold_vmid")),
        storage_layouts=storage_layouts,
        root_storage=default_storage,
        root_disk=str(_need(data, "root", "disk")),
        home_storage=default_storage,
        home_disk=str(_need(data, "home", "disk")),
        default_home_size=str(_need(data, "home", "default_size")),
        network_prefix=str(_need(data, "network", "prefix")),
        network_cidr=int(_need(data, "network", "cidr")),
        gateway=str(_need(data, "network", "gateway")),
        dns_servers=dns,
        snippet_storage=str(_need(data, "cloud_init", "snippet_storage")),
        snippet_dir=Path(str(_need(data, "cloud_init", "snippet_dir"))),
        user_name=user_name,
        user_uid=int(_need(data, "user", "uid")),
        user_gid=int(_need(data, "user", "gid")),
        workspace_ssh=workspace_ssh,
        herdr_workspace=herdr_workspace,
        herdr_tab=herdr_tab,
        herdr_debug=herdr_debug,
        storage_display_unit=storage_display_unit,
        storage_display_decimals=storage_display_decimals,
        sync_paths=sync_paths,
        sync_commands=sync_commands,
        sync_verbose=sync_verbose,
    )


def _toml_string(value: str) -> str:
    """Return a TOML basic string without adding a TOML dependency."""
    return json.dumps(value, ensure_ascii=False)


def _toml_array(values: tuple[str, ...]) -> str:
    return "[" + ", ".join(_toml_string(value) for value in values) + "]"


def config_to_toml(cfg: Config) -> str:
    """Serialize the current runtime configuration schema."""
    lines = [
        "version = 1",
        "",
        "[node]",
        f"name = {_toml_string(cfg.node)}",
        f"gold_vmid = {cfg.gold_vmid}",
        "",
        "[control]",
        f"node = {_toml_string(cfg.control_node)}",
        "",
        "[storage_layouts]",
    ]
    for layout, storages in cfg.storage_layouts.items():
        lines.append(f"{layout} = {_toml_array(storages)}")
    lines.extend(
        [
            "",
            "[root]",
            f"disk = {_toml_string(cfg.root_disk)}",
            "",
            "[home]",
            f"disk = {_toml_string(cfg.home_disk)}",
            f"default_size = {_toml_string(cfg.default_home_size)}",
            "",
            "[display]",
            f"storage_unit = {_toml_string(cfg.storage_display_unit)}",
            f"storage_decimals = {cfg.storage_display_decimals}",
            "",
            "[network]",
            f"prefix = {_toml_string(cfg.network_prefix)}",
            f"cidr = {cfg.network_cidr}",
            f"gateway = {_toml_string(cfg.gateway)}",
            f"dns = {_toml_array(cfg.dns_servers)}",
            "",
            "[cloud_init]",
            f"snippet_storage = {_toml_string(cfg.snippet_storage)}",
            f"snippet_dir = {_toml_string(str(cfg.snippet_dir))}",
            "",
            "[user]",
            f"name = {_toml_string(cfg.user_name)}",
            f"uid = {cfg.user_uid}",
            f"gid = {cfg.user_gid}",
            "",
            "[workspace_ssh]",
            f"user = {_toml_string(cfg.user_name)}",
            f"identity_files = {_toml_array(cfg.workspace_ssh.identity_files)}",
            f"identities_only = {str(cfg.workspace_ssh.identities_only).lower()}",
            f"log_level = {_toml_string(cfg.workspace_ssh.log_level)}",
            "",
            "[sync]",
            f"verbose = {str(cfg.sync_verbose).lower()}",
            f"paths = {_toml_array(cfg.sync_paths)}",
            f"commands = {_toml_array(cfg.sync_commands)}",
            "",
            "[transport]",
            f"type = {_toml_string(cfg.transport_type)}",
            "",
            "[transport.herdr]",
            f"workspace = {_toml_string(cfg.herdr_workspace)}",
            f"tab = {_toml_string(cfg.herdr_tab)}",
            f"debug = {str(cfg.herdr_debug).lower()}",
            "",
        ]
    )
    return "\n".join(lines)


def validate_config_text(text: str) -> Config:
    """Validate generated TOML through the normal runtime loader."""
    with tempfile.TemporaryDirectory(prefix="homestack-config-") as directory:
        path = Path(directory) / "config.toml"
        path.write_text(text, encoding="utf-8")
        return load_config(path)


def _write_bytes_fsynced(path: Path, data: bytes, *, exclusive: bool) -> None:
    flags = os.O_WRONLY | os.O_CREAT | (os.O_EXCL if exclusive else os.O_TRUNC)
    descriptor = os.open(path, flags, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb", closefd=False) as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
    finally:
        os.close(descriptor)


def _backup_path(path: Path) -> Path:
    stamp = datetime.now().astimezone().strftime("%Y%m%d-%H%M%S-%f")
    return path.with_name(f"{path.name}.backup-{stamp}")


def publish_config(cfg: Config) -> Path | None:
    """Validate and atomically publish cfg, backing up an existing target."""
    target = cfg.path
    payload = config_to_toml(cfg).encode("utf-8")
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary: Path | None = None
    backup: Path | None = None
    try:
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=f".{target.name}.", suffix=".tmp", dir=target.parent
        )
        temporary = Path(temporary_name)
        try:
            os.fchmod(descriptor, 0o600)
            with os.fdopen(descriptor, "wb", closefd=False) as handle:
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
        finally:
            os.close(descriptor)

        load_config(temporary)

        if target.exists():
            backup = _backup_path(target)
            _write_bytes_fsynced(backup, target.read_bytes(), exclusive=True)

        os.replace(temporary, target)
        temporary = None
        directory_fd = os.open(target.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        return backup
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
