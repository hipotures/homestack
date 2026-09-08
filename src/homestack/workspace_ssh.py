"""Workspace Ssh support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from urllib.parse import unquote
import os
import re
import shlex
import shutil
import tempfile

from .config import Config
from .guest import guest_exec
from .models import AppError
from .proxmox import qm_config, qm_status
from .transports.base import Transport, run_local

def forget_local_ssh_host(host: str) -> list[str]:
    if shutil.which("ssh-keygen") is None:
        raise AppError("Required local command 'ssh-keygen' was not found")

    targets = [host, f"[{host}]:22"]
    removed: list[str] = []
    for target in targets:
        found = run_local(["ssh-keygen", "-F", target], check=False)
        if found.returncode == 1:
            continue
        if found.returncode != 0:
            detail = (found.stderr or found.stdout).strip()
            raise AppError(
                f"Could not inspect local SSH known_hosts entry for {target!r}"
                + (f"\n{detail}" if detail else "")
            )

        result = run_local(["ssh-keygen", "-R", target], check=False)
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            raise AppError(
                f"Could not remove local SSH known_hosts entry for {target!r}"
                + (f"\n{detail}" if detail else "")
            )
        removed.append(target)

    return removed


def write_local_ssh_config(cfg: Config, vmid: int, name: str, ip: str) -> Path:
    directory = Path.home() / ".ssh" / "config.d" / "homestack"
    target = directory / f"vm{vmid}-{name}.conf"
    ssh_cfg = cfg.workspace_ssh
    lines = [
        f"Host {name}",
        f"    HostName {ip}",
        f"    User {ssh_cfg.user}",
        *(f"    IdentityFile {path}" for path in ssh_cfg.identity_files),
        f"    IdentitiesOnly {'yes' if ssh_cfg.identities_only else 'no'}",
        f"    LogLevel {ssh_cfg.log_level}",
    ]
    content = "\n".join(lines) + "\n"
    temporary: Path | None = None
    try:
        directory.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            newline="\n",
            dir=directory,
            prefix=f".{target.name}.",
            delete=False,
        ) as handle:
            temporary = Path(handle.name)
            os.fchmod(handle.fileno(), 0o600)
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())

        os.replace(temporary, target)
        temporary = None

        for stale in directory.glob(f"vm{vmid}-*.conf"):
            if stale != target:
                stale.unlink()
        return target
    except OSError as exc:
        raise AppError(f"Could not write local SSH config {target}: {exc}") from exc
    finally:
        if temporary is not None:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass


def remove_local_ssh_config(vmid: int) -> list[Path]:
    if not isinstance(vmid, int) or isinstance(vmid, bool) or vmid < 1:
        raise AppError(f"Invalid VMID for local SSH config cleanup: {vmid!r}")

    directory = Path.home() / ".ssh" / "config.d" / "homestack"
    removed: list[Path] = []
    current_path = directory
    try:
        for path in sorted(directory.glob(f"vm{vmid}-*.conf")):
            current_path = path
            path.unlink()
            removed.append(path)
    except OSError as exc:
        raise AppError(f"Could not remove local SSH config {current_path}: {exc}") from exc
    return removed


def validate_authorized_keys(text: str) -> str:
    keys: list[str] = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if not re.match(r"^(ssh-|ecdsa-|sk-)", line):
            raise AppError(f"Unexpected line in authorized_keys source: {line[:80]!r}")
        keys.append(line)
    if not keys:
        raise AppError("No SSH public keys found for workspace user")
    return "\n".join(keys) + "\n"


def parse_authorized_key_records(text: str) -> list[dict[str, str]]:
    validated = validate_authorized_keys(text)
    records: list[dict[str, str]] = []
    for line in validated.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 2:
            raise AppError(f"Invalid authorized_keys line: {line[:80]!r}")
        key_type, blob = parts[0], parts[1]
        comment = parts[2].strip() if len(parts) == 3 else ""
        label = comment or f"{key_type} {blob[:12]}…"
        records.append(
            {
                "type": key_type,
                "blob": blob,
                "comment": comment,
                "label": label,
                "identity": f"{key_type} {blob}",
            }
        )
    return records


def get_workspace_authorized_keys(session: Transport, cfg: Config) -> tuple[str, str]:
    gold_cfg = qm_config(session, cfg.gold_vmid)
    configured_keys = gold_cfg.get("sshkeys")
    if configured_keys:
        try:
            return (
                validate_authorized_keys(unquote(configured_keys)),
                f"Gold VM {cfg.gold_vmid}:qm sshkeys",
            )
        except AppError:
            pass

    if qm_status(session, cfg.gold_vmid) != "running":
        raise AppError(
            f"Gold VM {cfg.gold_vmid} is stopped and its Proxmox sshkeys setting "
            "does not contain usable public keys."
        )

    if session.run(f"qm guest cmd {cfg.gold_vmid} ping", check=False).returncode != 0:
        raise AppError(f"Gold VM {cfg.gold_vmid} QEMU Guest Agent is not available")

    candidates = (
        f"/home/{cfg.user_name}/.ssh/authorized_keys",
        "/root/.ssh/authorized_keys",
    )
    for candidate in candidates:
        result = guest_exec(
            session,
            cfg.gold_vmid,
            f"test -s {shlex.quote(candidate)} && cat {shlex.quote(candidate)}",
            check=False,
        )
        exitcode = result.get("exitcode")
        if exitcode is None or int(exitcode) != 0:
            continue
        try:
            return (
                validate_authorized_keys(str(result.get("out-data", ""))),
                f"Gold VM {cfg.gold_vmid}:{candidate}",
            )
        except AppError:
            continue

    raise AppError(
        f"Gold VM {cfg.gold_vmid} has no usable public keys in its Proxmox sshkeys "
        f"setting, /home/{cfg.user_name}/.ssh/authorized_keys, or /root/.ssh/authorized_keys"
    )
