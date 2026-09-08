"""Sync support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any
import os
import shlex
import shutil
import time
import uuid
from rich.markup import escape

from .config import Config, validate_sync_path_spec
from .guest import derive_ip
from .models import AppError
from .status import resolve_existing_workspace
from .transports.base import Transport, run_local, run_local_passthrough
from .ui import console

def sync_plan_item(cfg: Config, configured_path: str) -> dict[str, Any]:
    relative, is_directory = validate_sync_path_spec(configured_path)
    source = Path.home() / relative
    destination = Path("/home") / cfg.user_name / relative
    expected_type = "directory" if is_directory else "file"

    status = "ready"
    detail = ""
    if source.is_symlink():
        status = "type mismatch"
        detail = "symbolic links are not supported"
    elif not source.exists():
        status = "missing"
        detail = "source does not exist"
    elif is_directory and not source.is_dir():
        status = "type mismatch"
        detail = "configured with trailing '/', but source is not a directory"
    elif not is_directory and not source.is_file():
        status = "type mismatch"
        detail = "configured as a file, but source is not a regular file"

    return {
        "path": configured_path,
        "relative": relative,
        "type": expected_type,
        "is_directory": is_directory,
        "local_path": str(source),
        "destination": str(destination) + ("/" if is_directory else ""),
        "status": status,
        "detail": detail,
    }


def build_sync_plan(
    session: Transport,
    cfg: Config,
    vmid: int,
) -> dict[str, Any]:
    if not cfg.sync_paths:
        raise AppError(
            "No paths are configured for synchronization. Add [sync] paths = [...] to config.toml."
        )
    if shutil.which("ssh") is None:
        raise AppError("Required local command 'ssh' was not found")
    if shutil.which("rsync") is None:
        raise AppError("Required local command 'rsync' was not found")

    info = resolve_existing_workspace(session, cfg, vmid, require_network=False)
    if info["status"] != "running":
        raise AppError(
            f"Workspace VM {vmid} is {info['status']}; sync requires a running VM."
        )

    ip = derive_ip(cfg, vmid)
    items = [sync_plan_item(cfg, path) for path in cfg.sync_paths]
    plan = {
        "command": "sync",
        "vmid": vmid,
        "name": info["name"],
        "node": info["node"],
        "status": info["status"],
        "ip": ip,
        "user": cfg.user_name,
        "target_home": f"/home/{cfg.user_name}",
        "items": items,
        "configured": len(items),
        "ready": sum(1 for item in items if item["status"] == "ready"),
        "preflight_failed": sum(1 for item in items if item["status"] != "ready"),
        "transfer": "rsync over SSH",
        "verbose": cfg.sync_verbose,
        "delete": False,
        "config": str(cfg.path),
    }
    if cfg.sync_commands:
        plan["commands"] = list(cfg.sync_commands)
        plan["commands_configured"] = len(cfg.sync_commands)
    return plan


def sync_workspace(
    cfg: Config,
    plan: dict[str, Any],
    *,
    json_mode: bool,
) -> dict[str, Any]:
    vmid = int(plan["vmid"])
    ip = str(plan["ip"])
    user = str(plan["user"])
    target = f"{user}@{ip}"
    control_path = Path(
        f"/tmp/hs-sync-{os.getuid()}-{vmid}-{uuid.uuid4().hex[:8]}"
    )
    items = [dict(item) for item in plan.get("items", []) if isinstance(item, dict)]
    ready_items = [item for item in items if item.get("status") == "ready"]
    command_items = [
        {"command": str(command), "result": "pending", "detail": ""}
        for command in plan.get("commands", [])
    ]

    for item in items:
        if item.get("status") != "ready":
            item["result"] = str(item.get("status") or "failed")

    master_open = False
    ssh_common = [
        "-S", str(control_path),
        "-o", "ControlMaster=no",
        # Child sessions must reuse the already-authenticated master. If the
        # control socket disappears, disable all interactive authentication so
        # HomeStack fails instead of asking for another hardware-key touch.
        "-o", "PubkeyAuthentication=no",
        "-o", "PasswordAuthentication=no",
        "-o", "KbdInteractiveAuthentication=no",
        "-o", "GSSAPIAuthentication=no",
        "-o", "HostbasedAuthentication=no",
    ]

    try:
        if ready_items:
            if not json_mode:
                console.print(
                    "[dim]SSH: waiting for workspace authentication; touch the security key when requested.[/dim]"
                )
            ssh_started = time.monotonic()
            master = run_local_passthrough(
                [
                    "ssh",
                    "-M",
                    "-S", str(control_path),
                    "-o", "ControlPersist=60",
                    "-o", "StrictHostKeyChecking=accept-new",
                    "-o", "PasswordAuthentication=no",
                    "-o", "KbdInteractiveAuthentication=no",
                    "-f",
                    target,
                    "true",
                ]
            )
            if master.returncode != 0:
                for item in ready_items:
                    item["result"] = "failed"
                    item["detail"] = "could not open SSH ControlMaster session"
            else:
                master_check = run_local(
                    ["ssh", "-S", str(control_path), "-O", "check", target],
                    check=False,
                )
                if master_check.returncode != 0:
                    for item in ready_items:
                        item["result"] = "failed"
                        item["detail"] = (
                            (master_check.stderr or master_check.stdout).strip()
                            or "SSH ControlMaster socket is not available after authentication"
                        )
                else:
                    master_open = True
                    if not json_mode:
                        console.print(
                            f"[green]✓[/green] SSH ControlMaster established in "
                            f"{time.monotonic() - ssh_started:.1f}s"
                        )
                    remote_rsync = run_local(
                        ["ssh", *ssh_common, target, "command -v rsync >/dev/null 2>&1"],
                        check=False,
                    )
                    if remote_rsync.returncode == 255:
                        for item in ready_items:
                            item["result"] = "failed"
                            item["detail"] = (
                                (remote_rsync.stderr or remote_rsync.stdout).strip()
                                or "shared SSH session failed while checking remote rsync"
                            )
                    elif remote_rsync.returncode != 0:
                        for item in ready_items:
                            item["result"] = "failed"
                            item["detail"] = "rsync is not installed in the workspace"
                    else:
                        if cfg.sync_verbose and not json_mode:
                            console.print("[dim]✓ remote rsync available[/dim]")
                        for item in ready_items:
                            remote_path = str(item["destination"])
                            is_directory = bool(item["is_directory"])
                            remote_base = remote_path[:-1] if is_directory and remote_path.endswith("/") else remote_path
                            remote_parent = remote_base if is_directory else str(Path(remote_base).parent)

                            if cfg.sync_verbose and not json_mode:
                                console.print(
                                    f"[bold]Sync[/bold] {escape(str(item['path']))} "
                                    f"[dim]→ {escape(remote_base + ('/' if is_directory else ''))}[/dim]"
                                )
                            mkdir = run_local(
                                [
                                    "ssh",
                                    *ssh_common,
                                    target,
                                    f"mkdir -p -- {shlex.quote(remote_parent)}",
                                ],
                                check=False,
                            )
                            if mkdir.returncode != 0:
                                item["result"] = "failed"
                                item["detail"] = (
                                    (mkdir.stderr or mkdir.stdout).strip()
                                    or "could not create destination directory"
                                )
                                continue

                            local_source = str(item["local_path"]) + ("/" if is_directory else "")
                            remote_destination = remote_base + ("/" if is_directory else "")
                            rsync_ssh = shlex.join(["ssh", *ssh_common])
                            rsync_cmd = [
                                "rsync",
                                "-a",
                                "--no-owner",
                                "--no-group",
                                "--protect-args",
                            ]
                            if cfg.sync_verbose and not json_mode:
                                rsync_cmd.extend(
                                    [
                                        "-v",
                                        "--human-readable",
                                        "--info=progress2",
                                    ]
                                )
                            rsync_cmd.extend(
                                [
                                    "-e",
                                    rsync_ssh,
                                    "--",
                                    local_source,
                                    f"{target}:{remote_destination}",
                                ]
                            )
                            transfer_started = time.monotonic()
                            if cfg.sync_verbose and not json_mode:
                                transfer = run_local_passthrough(rsync_cmd)
                            else:
                                transfer = run_local(rsync_cmd, check=False)
                            if transfer.returncode != 0:
                                item["result"] = "failed"
                                captured = transfer.stderr or transfer.stdout or ""
                                item["detail"] = (
                                    captured.strip()
                                    or f"rsync exited with {transfer.returncode}"
                                )
                                continue

                            if cfg.sync_verbose and not json_mode:
                                console.print(
                                    f"[dim]✓ rsync finished in {time.monotonic() - transfer_started:.1f}s[/dim]"
                                )

                            if is_directory:
                                verify_command = (
                                    f"p={shlex.quote(remote_base)}; "
                                    'test -d "$p" || { echo "destination is not a directory"; exit 41; }; '
                                    f'bad=$(find "$p" \\( ! -uid {cfg.user_uid} -o ! -gid {cfg.user_gid} \\) '
                                    '-print -quit); '
                                    'test -z "$bad" || { printf "unexpected owner: %s\\n" "$bad"; exit 42; }; '
                                    "echo OK"
                                )
                            else:
                                verify_command = (
                                    f"p={shlex.quote(remote_base)}; "
                                    'test -f "$p" || { echo "destination is not a file"; exit 41; }; '
                                    'owner=$(stat -c "%u:%g" -- "$p"); '
                                    f'test "$owner" = "{cfg.user_uid}:{cfg.user_gid}" || '
                                    '{ printf "unexpected owner: %s\\n" "$owner"; exit 42; }; '
                                    "echo OK"
                                )

                            verify = run_local(
                                ["ssh", *ssh_common, target, verify_command],
                                check=False,
                            )
                            if verify.returncode != 0:
                                item["result"] = "failed"
                                item["detail"] = (
                                    (verify.stderr or verify.stdout).strip()
                                    or "destination verification failed"
                                )
                                continue

                            item["result"] = "synced"
                            item["detail"] = f"owner {cfg.user_uid}:{cfg.user_gid} verified"
                            if cfg.sync_verbose and not json_mode:
                                console.print(
                                    f"[dim]✓ destination owner {cfg.user_uid}:{cfg.user_gid} verified[/dim]"
                                )

                    paths_succeeded = all(
                        item.get("result") == "synced" for item in items
                    )
                    if command_items and paths_succeeded:
                        for index, item in enumerate(command_items):
                            command = str(item["command"])
                            if cfg.sync_verbose and not json_mode:
                                console.print(f"[bold]Command[/bold] {escape(command)}")
                                command_result = run_local_passthrough(
                                    ["ssh", *ssh_common, target, command]
                                )
                            else:
                                command_result = run_local(
                                    ["ssh", *ssh_common, target, command],
                                    check=False,
                                )
                            if command_result.returncode != 0:
                                item["result"] = "failed"
                                item["detail"] = (
                                    f"exited with status {command_result.returncode}"
                                )
                                for skipped in command_items[index + 1 :]:
                                    skipped["result"] = "skipped"
                                    skipped["detail"] = (
                                        "not run after an earlier command failed"
                                    )
                                break
                            item["result"] = "succeeded"
                    elif command_items:
                        for item in command_items:
                            item["result"] = "skipped"
                            item["detail"] = (
                                "not run because path synchronization failed"
                            )
    finally:
        if master_open:
            run_local(
                ["ssh", *ssh_common, "-O", "exit", target],
                check=False,
            )
        try:
            control_path.unlink(missing_ok=True)
        except OSError:
            pass

    for item in command_items:
        if item["result"] == "pending":
            item["result"] = "skipped"
            item["detail"] = "not run because path synchronization failed"

    synced = sum(1 for item in items if item.get("result") == "synced")
    failed = len(items) - synced
    commands_succeeded = sum(
        1 for item in command_items if item["result"] == "succeeded"
    )
    commands_failed = sum(1 for item in command_items if item["result"] == "failed")
    commands_skipped = sum(1 for item in command_items if item["result"] == "skipped")
    result = {
        "ok": failed == 0 and commands_failed == 0 and commands_skipped == 0,
        **plan,
        "items": items,
        "synced": synced,
        "failed": failed,
    }
    if command_items:
        result.update(
            {
                "command_items": command_items,
                "commands_succeeded": commands_succeeded,
                "commands_failed": commands_failed,
                "commands_skipped": commands_skipped,
            }
        )
    return result
