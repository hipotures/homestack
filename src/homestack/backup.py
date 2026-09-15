"""HomeStack Setup integration for the guest-side BK backup tool."""
from __future__ import annotations

from dataclasses import dataclass
import base64
import hashlib
from importlib import resources

from .models import AppError


@dataclass(frozen=True)
class ManagedAsset:
    resource_name: str
    relative_path: str
    mode: int


MANAGED_ASSETS = (
    ManagedAsset("bk.py", ".local/bin/bk", 0o755),
    ManagedAsset("backup.service", ".config/systemd/user/backup.service", 0o644),
    ManagedAsset("backup.timer", ".config/systemd/user/backup.timer", 0o644),
)
MANAGED_DIRECTORIES = (".local/bin", ".config/systemd/user", "backup")


def managed_paths() -> tuple[str, ...]:
    return tuple(asset.relative_path for asset in MANAGED_ASSETS)


def load_assets() -> dict[str, bytes]:
    root = resources.files("homestack").joinpath("assets", "backup")
    try:
        return {
            asset.relative_path: root.joinpath(asset.resource_name).read_bytes()
            for asset in MANAGED_ASSETS
        }
    except (FileNotFoundError, OSError) as exc:
        raise AppError("Packaged BK setup assets are unavailable") from exc


def desired_hashes() -> dict[str, str]:
    return {
        path: hashlib.sha256(content).hexdigest()
        for path, content in load_assets().items()
    }


def _command(ws, cfg, command: str):
    from .setup import command_environment

    return ws.run(
        command_environment(cfg, command, interpreter="sh", pipefail=False),
        check=False,
    )


def _systemctl(ws, cfg, arguments: str):
    return _command(
        ws,
        cfg,
        f"env XDG_RUNTIME_DIR=/run/user/{cfg.user_uid} systemctl --user {arguments}",
    )


def _require_runtime(ws, cfg) -> None:
    from .setup import require_tool

    for tool in ("python3", "file", "git", "systemctl"):
        require_tool(ws, cfg, tool)
    modules = _command(ws, cfg, "python3 -c 'import curses, rich, sqlite3'")
    if modules.returncode:
        raise AppError(
            "BK requires Python modules curses, rich and sqlite3; prepare Gold and refresh separately"
        )
    user_systemd = _systemctl(ws, cfg, "show-environment")
    if user_systemd.returncode:
        raise AppError(
            "The workspace user systemd manager is unavailable; prepare the guest session separately"
        )


def _timer_state(ws, cfg) -> tuple[bool, bool]:
    enabled = _systemctl(ws, cfg, "is-enabled backup.timer").returncode == 0
    active = _systemctl(ws, cfg, "is-active backup.timer").returncode == 0
    return enabled, active


def inspect(ws, cfg, *, check_requirements: bool = True) -> dict:
    """Inspect managed BK assets and the user timer without reading BK user data."""
    from .setup import guest

    if check_requirements:
        _require_runtime(ws, cfg)
    guest(
        ws,
        cfg,
        "paths",
        paths=[
            {"relative": path, "directory": True}
            for path in MANAGED_DIRECTORIES
        ],
    )
    actual = guest(ws, cfg, "managed-files-inspect", paths=list(managed_paths()))
    files = actual.get("items", [])
    by_path = {item["path"]: item for item in files}
    desired = desired_hashes()
    modes = {asset.relative_path: asset.mode for asset in MANAGED_ASSETS}
    changed = [
        path
        for path in managed_paths()
        if not by_path.get(path, {}).get("exists")
        or by_path[path].get("sha256") != desired[path]
        or by_path[path].get("mode") != modes[path]
    ]
    snapshot_paths = [
        path for path in changed if by_path.get(path, {}).get("exists")
    ]
    timer_enabled, timer_active = _timer_state(ws, cfg)
    any_installed = any(item.get("exists") for item in files)
    ready = not changed and timer_enabled and timer_active
    state = (
        "configured"
        if ready
        else "not installed"
        if not any_installed
        else "needs update"
    )
    return {
        "state": state,
        "ready": ready,
        "exists": any_installed,
        "will_overwrite": bool(snapshot_paths),
        "files": files,
        "managed_paths": list(managed_paths()),
        "changed_paths": changed,
        "snapshot_paths": snapshot_paths,
        "timer_enabled": timer_enabled,
        "timer_active": timer_active,
    }


def preflight(ws, cfg) -> dict:
    return inspect(ws, cfg, check_requirements=True)


def apply(ws, cfg, state: dict, *, activity=lambda message: None) -> tuple[str, str]:
    """Atomically reconcile HomeStack-owned assets and enable the user timer."""
    from .setup import guest

    if state.get("ready"):
        return "already-ready", "BK assets and user timer already match"

    contents = load_assets()
    by_path = {item["path"]: item for item in state.get("files", [])}
    changed = set(state.get("changed_paths", managed_paths()))
    files = []
    for asset in MANAGED_ASSETS:
        if asset.relative_path not in changed:
            continue
        payload = contents[asset.relative_path]
        files.append(
            {
                "path": asset.relative_path,
                "content": base64.b64encode(payload).decode("ascii"),
                "sha256": hashlib.sha256(payload).hexdigest(),
                "expected_sha256": by_path.get(asset.relative_path, {}).get("sha256"),
                "mode": asset.mode,
            }
        )

    activity("Install or update managed BK assets")
    guest(
        ws,
        cfg,
        "managed-files-install",
        directories=list(MANAGED_DIRECTORIES),
        files=files,
    )
    unit_paths = {
        ".config/systemd/user/backup.service",
        ".config/systemd/user/backup.timer",
    }
    if changed & unit_paths:
        activity("Reload the user systemd manager")
        if _systemctl(ws, cfg, "daemon-reload").returncode:
            raise AppError("User systemd daemon-reload failed")

    activity("Enable and start backup.timer")
    if _systemctl(ws, cfg, "enable --now backup.timer").returncode:
        raise AppError("Could not enable and start backup.timer")
    enabled, active = _timer_state(ws, cfg)
    if not enabled or not active:
        raise AppError("backup.timer did not become enabled and active")

    verified = inspect(ws, cfg, check_requirements=False)
    if not verified["ready"]:
        raise AppError("BK installation verification failed")
    action = "installed" if state.get("state") == "not installed" else "updated"
    return "succeeded", f"BK {action}; backup.timer is enabled and active"
