"""Herdr installation, persistent user service and desktop machine registration."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

from .models import AppError
from .transports.base import run_local

UNIT_PATH = ".config/systemd/user/herdr.service"
INSTALL_COMMAND = "curl -fsSL https://herdr.dev/install.sh | sh"
AUTH_NOTICE = "Connecting desktop Herdr through separate SSH authentication. Touch your YubiKey/security key if prompted."


def register_desktop(command):
    """Expose the terminal when Herdr setup is invoked from the plain CLI."""
    print(AUTH_NOTICE, file=sys.stderr, flush=True)
    print(shlex.join(command), file=sys.stderr, flush=True)
    return subprocess.run(command, text=True)


def unit_content() -> bytes:
    return Path(__file__).with_name("assets").joinpath("herdr/herdr.service").read_bytes()


def _command(ws, cfg, command):
    from .setup import command_environment
    return ws.run(command_environment(cfg, command), check=False)


def _systemctl(ws, cfg, arguments):
    return _command(ws, cfg, f"env XDG_RUNTIME_DIR=/run/user/{cfg.user_uid} systemctl --user {arguments}")


def desktop_profile(cfg, target) -> dict | None:
    try:
        result = run_local(["herdr", "machine", "list", "--json"], check=False)
    except OSError:
        raise AppError("Herdr setup requires a working desktop herdr executable") from None
    try:
        profiles = json.loads(result.stdout)
        if result.returncode or not isinstance(profiles, list):
            raise ValueError
        for profile in profiles:
            if (not isinstance(profile, dict)
                    or any(not isinstance(profile.get(key), str) for key in ("id", "target", "label", "session"))
                    or not isinstance(profile.get("enabled"), bool)):
                raise ValueError
    except (TypeError, ValueError):
        raise AppError("Could not inspect desktop Herdr machine profiles; output withheld") from None
    targets = {f"{cfg.user_name}@{target['name']}", f"{cfg.user_name}@{target['ip']}"}
    matches = [p for p in profiles if p["target"] in targets and p["session"] == "default"]
    if any(p["label"] == target["name"] and p not in matches for p in profiles):
        raise AppError("The VM name is already used by another desktop Herdr profile; resolve the label conflict first")
    if len(matches) > 1:
        raise AppError("Multiple desktop Herdr profiles reference this workspace; resolve duplicates first")
    return matches[0] if matches else None


def verify_desktop_alias(cfg, target):
    result = run_local(["ssh", "-G", f"{cfg.user_name}@{target['name']}"], check=False)
    values = {}
    for line in result.stdout.splitlines():
        key, _, value = line.partition(" ")
        values.setdefault(key, []).append(value.strip())
    identities = {str(Path(path).expanduser()) for path in values.get("identityfile", [])}
    expected = {str(Path(path).expanduser()) for path in cfg.workspace_ssh.identity_files}
    if (result.returncode or values.get("hostname") != [target["ip"]]
            or values.get("user") != [cfg.user_name] or not expected.issubset(identities)
            or values.get("identitiesonly") != ["yes" if cfg.workspace_ssh.identities_only else "no"]):
        raise AppError("Desktop SSH alias does not match the workspace address/account/keys. Include ~/.ssh/config.d/homestack/*.conf in ~/.ssh/config, then retry app=herdr")


def running_sessions(ws, cfg) -> list[str]:
    result = _command(ws, cfg, '"$HOME/.local/bin/herdr" session list --json')
    try:
        data = json.loads(result.stdout)
        sessions = data["sessions"]
        if result.returncode or not isinstance(sessions, list):
            raise ValueError
        if any(not isinstance(s, dict) or not isinstance(s.get("name"), str)
               or not isinstance(s.get("running"), bool) for s in sessions):
            raise ValueError
    except (KeyError, TypeError, ValueError):
        raise AppError("Could not inspect Herdr sessions; refusing to start or replace a server") from None
    return [s["name"] for s in sessions if s["running"]]


def inspect(ws, cfg, target) -> dict:
    from .setup import guest
    installed = _command(ws, cfg, '"$HOME/.local/bin/herdr" --version').returncode == 0
    sessions = running_sessions(ws, cfg) if installed else []
    files = guest(ws, cfg, "managed-files-inspect", paths=[UNIT_PATH]).get("items", [])
    unit = next((f for f in files if f["path"] == UNIT_PATH), {})
    changed = unit.get("sha256") != hashlib.sha256(unit_content()).hexdigest() or unit.get("mode") != 0o600
    enabled = _systemctl(ws, cfg, "is-enabled herdr.service").returncode == 0
    active = _systemctl(ws, cfg, "is-active herdr.service").returncode == 0
    linger = _command(ws, cfg, "loginctl show-user " + shlex.quote(cfg.user_name) + " --property=Linger --value")
    lingering = linger.returncode == 0 and linger.stdout.strip() == "yes"
    profile = desktop_profile(cfg, target)
    connected = profile is not None and profile["label"] == target["name"] and profile["enabled"]
    deferred = bool(sessions) and not active
    ready = installed and not changed and enabled and lingering and connected and ("default" in sessions) and (active or deferred)
    return {
        "installed": installed, "ready": ready, "exists": installed or bool(unit.get("exists")),
        "state": "configured; service starts after VM reboot" if ready and deferred else "configured" if ready else "needs setup" if installed else "not installed",
        "files": files, "will_overwrite": changed and bool(unit.get("exists")),
        "changed": changed, "snapshot_paths": [UNIT_PATH] if changed and unit.get("exists") else [],
        "service_enabled": enabled, "service_active": active, "linger": lingering,
        "running_sessions": sessions, "desktop_profile": connected,
        "desktop_profile_exists": profile is not None,
        "detail": "Existing Herdr server left running; the user service is enabled for the next VM boot" if deferred else "",
    }


def preflight(ws, cfg, target, *, unattended=False) -> dict:
    from .setup import guest, require_tool
    for tool in ("herdr", "ssh"):
        if shutil.which(tool) is None:
            raise AppError(f"Herdr setup requires desktop {tool}")
    for tool in ("bash", "curl", "awk", "sha256sum", "systemctl", "loginctl"):
        require_tool(ws, cfg, tool)
    if _systemctl(ws, cfg, "show-environment").returncode:
        raise AppError("Herdr requires a usable user systemd manager; prepare the user session separately")
    guest(ws, cfg, "paths", paths=[{"relative": ".local/bin", "directory": True},
                                 {"relative": ".local/bin/herdr"}, {"relative": UNIT_PATH}])
    state = inspect(ws, cfg, target)
    if unattended and not state["desktop_profile_exists"]:
        raise AppError("Desktop Herdr registration requires an interactive terminal for SSH/security-key authentication; run app=herdr interactively")
    if not state["desktop_profile_exists"]:
        verify_desktop_alias(cfg, target)
    return state


def connect_desktop(cfg, target, *, desktop=register_desktop, activity, unattended=False):
    # Re-read after guest setup so concurrent catalog edits are not overwritten.
    profile = desktop_profile(cfg, target)
    if profile is not None:
        commands = []
        if profile["label"] != target["name"]:
            commands.append(["herdr", "machine", "rename", profile["id"], "--label", target["name"]])
        if not profile["enabled"]:
            commands.append(["herdr", "machine", "enable", profile["id"]])
        for command in commands:
            if run_local(command, check=False).returncode:
                raise AppError("Could not configure the existing desktop Herdr profile; guest service remains configured")
        activity("Desktop Herdr profile configured; no new SSH authentication needed")
        return
    if unattended:
        raise AppError("Desktop Herdr profile is missing; guest service is configured. Retry app=herdr interactively for SSH authentication")
    verify_desktop_alias(cfg, target)
    command = ["herdr", "machine", "add", f"{cfg.user_name}@{target['name']}",
               "--label", target["name"], "--remote-session", "default"]
    activity(AUTH_NOTICE)
    result = desktop(command)
    if result.returncode:
        # Classify known diagnostics without echoing arbitrary external output.
        output = ((result.stdout or "") + (result.stderr or "")).lower()
        if "interactive" in output or "approval" in output:
            raise AppError("Herdr requires an interactive decision; the guest service is configured. Run " + shlex.join(command) + " in a terminal, then retry app=herdr")
        if "permission denied" in output:
            raise AppError("Desktop SSH authentication failed; the guest service is configured. Unlock/load your hardware key in ssh-agent and retry app=herdr")
        raise AppError("Desktop Herdr registration failed; the guest service is configured. Retry app=herdr to finish connecting")


def apply(ws, cfg, target, state, *, desktop=register_desktop, activity, unattended=False):
    from .setup import guest
    if not state["installed"]:
        activity("Install Herdr using the official installer")
        if _command(ws, cfg, INSTALL_COMMAND).returncode:
            raise AppError("Herdr installer failed; output withheld")
    if _command(ws, cfg, '"$HOME/.local/bin/herdr" --version').returncode:
        raise AppError("Herdr installation verification failed")

    activity("Enable linger for the workspace user")
    if _command(ws, cfg, "loginctl --no-ask-password enable-linger " + shlex.quote(cfg.user_name)).returncode:
        raise AppError("Could not enable user linger. An administrator must run loginctl enable-linger " + cfg.user_name + "; then retry app=herdr")

    if state["changed"]:
        activity("Write Herdr user service")
        content = unit_content()
        previous = next((f for f in state["files"] if f["path"] == UNIT_PATH), {})
        guest(ws, cfg, "managed-files-install", directories=[".config/systemd/user"], files=[{
            "path": UNIT_PATH, "content": base64.b64encode(content).decode(),
            "sha256": hashlib.sha256(content).hexdigest(), "expected_sha256": previous.get("sha256"), "mode": 0o600,
        }])
    for command in ("daemon-reload", "enable herdr.service"):
        if _systemctl(ws, cfg, command).returncode:
            raise AppError("Could not configure Herdr user service")

    # Inspect again immediately before start; desktop clients may start a server
    # after preflight. Never update or stop a running unmanaged server.
    sessions = running_sessions(ws, cfg)
    active = _systemctl(ws, cfg, "is-active herdr.service").returncode == 0
    deferred = bool(sessions) and not active
    if deferred:
        activity("Leave existing Herdr sessions running; service will start after the next VM reboot")
    elif not active:
        activity("Start Herdr service; it updates before launching the server")
        if _systemctl(ws, cfg, "start herdr.service").returncode:
            raise AppError("Could not start Herdr user service")
        if _systemctl(ws, cfg, "is-active herdr.service").returncode:
            raise AppError("Herdr user service did not become active")

    connect_desktop(cfg, target, desktop=desktop, activity=activity, unattended=unattended)
    if not inspect(ws, cfg, target)["ready"]:
        raise AppError("Herdr setup verification failed")
    detail = "Herdr configured; desktop label: " + target["name"]
    if deferred:
        detail += "; existing sessions preserved; service starts after VM reboot"
    else:
        detail += "; user service enabled and active"
    return "succeeded", detail
