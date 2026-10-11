"""Add one SSH public key to a workspace through Proxmox Cloud-Init.

The key goes into the VM's Proxmox ``sshkeys`` and into a step of the HomeStack vendor
snippet that appends it to root's and the workspace user's ``authorized_keys``. A new
``instance-id`` makes Cloud-Init run once more on the next start. Nothing is removed from
``authorized_keys``, and the persistent home is never formatted or rewritten.
"""

from __future__ import annotations

import os
import re
import shlex
import tempfile
import tomllib
import uuid
from dataclasses import replace
from pathlib import Path
from typing import Any, Callable
from urllib.parse import unquote

from .cloudinit import remote_write_text, snippet_names
from .config import Config, _toml_array
from .guest import guest_out_on_node, wait_for_qga_on_node
from .models import AppError
from .proxmox import node_run, shutdown_vm_on_node
from .status import resolve_existing_workspace
from .transports.base import Transport
from .workspace_ssh import forget_local_ssh_host, parse_authorized_key_records, write_local_ssh_config

KEYS_HEREDOC = "HS_KEYS"
ENSURE_MARKER = "# HomeStack ensure-keys"
ENSURE_DONE = "HomeStack: ensured SSH keys from Cloud-Init"
_ITEM_START = re.compile(r"^  - ", re.MULTILINE)
_ALLOW_FORMAT = re.compile(r"^(\s*)allow_format=[01]\s*$", re.MULTILINE)


def read_public_key(path: Path) -> dict[str, str]:
    """Read exactly one SSH public key from ``path``."""
    try:
        text = Path(path).expanduser().read_text(encoding="utf-8")
    except OSError as exc:
        raise AppError(f"Cannot read public key {path}: {exc.strerror or exc}") from None
    records = parse_authorized_key_records(text)
    if len(records) != 1:
        raise AppError(f"{path} must contain exactly one public key, found {len(records)}")
    record = records[0]
    if not record["comment"]:
        raise AppError(f"{path} has no comment; give the key a name, for example ssh-keygen -C trezor-vm")
    return {**record, "line": f"{record['identity']} {record['comment']}", "path": str(path)}


def merge_sshkeys(current: str, key: dict[str, str]) -> tuple[list[str], bool]:
    """Return the keys for Proxmox ``sshkeys`` with ``key`` added, and whether it was already there."""
    lines = [line.strip() for line in unquote(current or "").splitlines() if line.strip()]
    blobs = {line.split()[1] for line in lines if len(line.split()) >= 2}
    if key["blob"] in blobs:
        return lines, True
    return [*lines, key["line"]], False


def ensure_keys_item(keys: list[str], user_name: str) -> str:
    """The Cloud-Init ``runcmd`` item that appends missing keys to root's and the user's file."""
    body = [
        ENSURE_MARKER,
        "set -eu",
        "keys=$(cat <<'" + KEYS_HEREDOC + "'",
        *keys,
        KEYS_HEREDOC,
        ")",
        f"for dir in /root /home/{user_name}; do",
        '    [ -d "$dir" ] || continue',
        '    owner=$(stat -c %U "$dir")',
        '    group=$(stat -c %G "$dir")',
        '    f="$dir/.ssh/authorized_keys"',
        '    install -d -m 700 -o "$owner" -g "$group" "$dir/.ssh"',
        '    [ -f "$f" ] || install -m 600 -o "$owner" -g "$group" /dev/null "$f"',
        '    [ -z "$(tail -c1 "$f")" ] || echo >> "$f"',
        "    printf '%s\\n' \"$keys\" | while IFS= read -r key; do",
        '        [ -n "$key" ] || continue',
        "        blob=$(printf '%s' \"$key\" | awk '{print $2}')",
        '        grep -qF "$blob" "$f" || printf \'%s\\n\' "$key" >> "$f"',
        "    done",
        "done",
        f"echo '{ENSURE_DONE}'",
    ]
    return "  - |\n" + "".join(f"      {line}\n" for line in body)


def update_vendor(text: str, keys: list[str], user_name: str) -> str:
    """Rewrite the vendor snippet: no home rewrite on rerun, and one ensure-keys step."""
    if not _ALLOW_FORMAT.search(text):
        raise AppError("The vendor snippet has no allow_format line; it is not a HomeStack snippet")
    # With allow_format=1 a rerun would overwrite authorized_keys and reset the home permissions.
    text = _ALLOW_FORMAT.sub(lambda match: f"{match.group(1)}allow_format=0", text)
    starts = [match.start() for match in _ITEM_START.finditer(text)]
    if not starts:
        raise AppError("The vendor snippet has no runcmd steps; it is not a HomeStack snippet")
    ends = [*starts[1:], len(text)]
    head = text[:starts[0]]
    items = [text[begin:end] for begin, end in zip(starts, ends) if KEYS_HEREDOC not in text[begin:end]]
    if not items:
        raise AppError("The vendor snippet has no HomeStack home step; refusing to edit it")
    return head + "".join(item if item.endswith("\n") else item + "\n" for item in items) \
        + ensure_keys_item(keys, user_name)


def new_meta(name: str, vmid: int) -> str:
    return f"instance-id: {name}-{vmid}-{uuid.uuid4().hex[:12]}\nlocal-hostname: {name}\n"


_IDENTITY_LINE = re.compile(r"^identity_files\s*=\s*\[(?P<body>[^\]\n]*)\][ \t]*$", re.MULTILINE)


def private_key_for(public: Path) -> tuple[Path, str]:
    """The private key file next to a ``.pub`` file, and how to write it in configs (``~/`` form)."""
    pub = Path(public).expanduser()
    private = pub.with_suffix("") if pub.suffix == ".pub" else pub
    if private == pub or not private.is_file():
        raise AppError(f"The private key file {private} next to {public} was not found")
    try:
        shown = "~/" + private.resolve().relative_to(Path.home().resolve()).as_posix()
    except ValueError:
        shown = str(private)
    return private, shown


def register_identity(config_path: Path, identity: str) -> bool:
    """Append ``identity`` to the one-line ``identity_files`` array of ``[workspace_ssh]``."""
    text = config_path.read_text(encoding="utf-8")
    section = re.search(r"^\[workspace_ssh\][ \t]*$", text, re.MULTILINE)
    match = _IDENTITY_LINE.search(text, section.end()) if section else None
    following = re.search(r"^\[", text[section.end():], re.MULTILINE) if section else None
    if match is None or (following and match.start() > section.end() + following.start()):
        raise AppError("identity_files in [workspace_ssh] is not a one-line array; add the key to it by hand")
    existing = tomllib.loads("x = [" + match.group("body") + "]")["x"]
    if str(Path(identity).expanduser()) in {str(Path(item).expanduser()) for item in existing}:
        return False
    line = "identity_files = " + _toml_array((*existing, identity))
    new_text = text[:match.start()] + line + text[match.end():]
    mode = config_path.stat().st_mode & 0o777
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=config_path.parent,
                                     prefix=f".{config_path.name}.", delete=False) as handle:
        temporary = Path(handle.name)
        os.fchmod(handle.fileno(), mode)
        handle.write(new_text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, config_path)
    return True


def sync_local_ssh(cfg: Config, plan: dict[str, Any]) -> dict[str, Any]:
    """Make the new key usable locally: HomeStack config and this VM's ssh config."""
    identity = plan["private_key"]
    changed = register_identity(cfg.path, identity)
    files = tuple(cfg.workspace_ssh.identity_files)
    if str(Path(identity).expanduser()) not in {str(Path(item).expanduser()) for item in files}:
        files += (identity,)
    updated = replace(cfg, workspace_ssh=replace(cfg.workspace_ssh, identity_files=files))
    path = write_local_ssh_config(updated, plan["vmid"], plan["name"], plan["ip"])
    return {"identity_files_changed": changed, "ssh_config": str(path)}


def build_addkey_plan(session: Transport, cfg: Config, vmid: int, key_path: Path) -> dict[str, Any]:
    key = read_public_key(key_path)
    _private, private_shown = private_key_for(key_path)
    info = resolve_existing_workspace(session, cfg, vmid, require_network=True)
    name, node, vm_cfg = info["name"], info["node"], info["vm_config"]
    names = snippet_names(name)
    cicustom = vm_cfg.get("cicustom", "")
    paths = {kind: cfg.snippet_dir / names[kind] for kind in ("vendor", "meta")}
    for kind in ("vendor", "meta"):
        if f"{kind}={cfg.snippet_storage}:snippets/{names[kind]}" not in cicustom:
            raise AppError(
                f"VM {vmid} does not use the HomeStack {kind} snippet {names[kind]}; refusing to add a key"
            )
    keys, present = merge_sshkeys(vm_cfg.get("sshkeys", ""), key)
    return {
        "command": "addkey",
        "vmid": vmid,
        "name": name,
        "node": node,
        "status": info["status"],
        "ip": info["ip"],
        "user": cfg.user_name,
        "key": {"comment": key["comment"], "type": key["type"], "path": key["path"],
                "line": key["line"], "blob": key["blob"]},
        "private_key": private_shown,
        "key_already_in_sshkeys": present,
        "keys": keys,
        "snippets": {kind: str(path) for kind, path in paths.items()},
        "steps": [
            "Add the key to the Proxmox sshkeys of the VM" + (" (already there)" if present else ""),
            "Set allow_format=0 in the vendor snippet so the rerun cannot rewrite authorized_keys or home",
            "Replace the ensure-keys step: append missing keys to root and "
            f"{cfg.user_name} authorized_keys; nothing is removed",
            "Write a new instance-id so Cloud-Init runs once on the next start",
            "qm cloudinit update. The VM is NOT restarted here; you are asked about the restart at the end"
            if info["status"] == "running"
            else "qm cloudinit update (the VM is stopped and stays stopped)",
            "Only after a restart: remove the old SSH host key for the VM address from the local known_hosts",
            f"Only after a restart: check that the key is in root and {cfg.user_name} authorized_keys",
            f"Add {private_shown} to identity_files in the HomeStack config",
            "Rewrite the local ssh config of this VM; YubiKey first, Trezor when no YubiKey is connected",
        ],
    }


def _read_remote(session: Transport, cfg: Config, node: str, path: str) -> str:
    return node_run(session, cfg, node, f"cat {shlex.quote(path)}", timeout=30).output


def _set_sshkeys(session: Transport, cfg: Config, node: str, vmid: int, keys: list[str]) -> None:
    temporary = Path(f"/tmp/hs-sshkeys-{vmid}-{uuid.uuid4().hex[:8]}")
    remote_write_text(session, cfg, node, temporary, "\n".join(keys) + "\n", 0o600)
    node_run(
        session, cfg, node,
        f"qm set {vmid} --sshkeys {shlex.quote(str(temporary))}; rc=$?; rm -f {shlex.quote(str(temporary))}; exit $rc",
        timeout=60,
    )


def add_key(session: Transport, cfg: Config, plan: dict[str, Any],
            restart: Callable[[], bool] | bool = False) -> dict[str, Any]:
    """Prepare the key; restart a running VM only if ``restart`` is true (or answers yes)."""
    vmid, name, node, user = plan["vmid"], plan["name"], plan["node"], plan["user"]
    vendor_path, meta_path = plan["snippets"]["vendor"], plan["snippets"]["meta"]
    key = plan["key"]
    old_vendor = _read_remote(session, cfg, node, vendor_path)
    old_meta = _read_remote(session, cfg, node, meta_path)
    vendor = update_vendor(old_vendor, plan["keys"], user)
    written: list[str] = []
    try:
        if not plan["key_already_in_sshkeys"]:
            _set_sshkeys(session, cfg, node, vmid, plan["keys"])
            written.append("sshkeys")
        remote_write_text(session, cfg, node, Path(vendor_path), vendor, 0o644)
        written.append("vendor")
        remote_write_text(session, cfg, node, Path(meta_path), new_meta(name, vmid), 0o644)
        written.append("meta")
        node_run(session, cfg, node, f"qm cloudinit update {vmid}", timeout=120)
    except Exception:
        # Put the snippets back; the Proxmox sshkeys keep the new key, which is harmless on its own.
        if "vendor" in written:
            remote_write_text(session, cfg, node, Path(vendor_path), old_vendor, 0o644)
        if "meta" in written:
            remote_write_text(session, cfg, node, Path(meta_path), old_meta, 0o644)
        raise
    was_running = plan["status"] == "running"
    restarted = was_running and (restart() if callable(restart) else bool(restart))
    if restarted:
        shutdown_vm_on_node(session, cfg, node, vmid)
        node_run(session, cfg, node, f"qm start {vmid}", timeout=300)
        wait_for_qga_on_node(session, cfg, node, vmid)
        text = guest_out_on_node(
            session, cfg, node, vmid, "timeout 300 cloud-init status --wait 2>&1 || true", timeout=330,
        )
        if not re.search(r"status:\s*done", text):
            raise AppError(
                f"Cloud-Init on VM {vmid} did not report 'done' after the restart: {text.strip()[:200]}"
            )
    removed = forget_local_ssh_host(plan["ip"]) if restarted else []
    verified = False
    if restarted:
        text = guest_out_on_node(
            session, cfg, node, vmid,
            f"grep -cF {shlex.quote(key['blob'])} /root/.ssh/authorized_keys /home/{user}/.ssh/authorized_keys || true",
        )
        counts = re.findall(r":(\d+)\s*$", text, re.MULTILINE)
        verified = len(counts) == 2 and all(int(count) >= 1 for count in counts)
        if not verified:
            raise AppError(f"Key {key['comment']} is not in both authorized_keys files of VM {vmid}: {text.strip()[:200]}")
    result = {
        "ok": True,
        "command": "addkey",
        "vmid": vmid,
        "name": name,
        "key": key["comment"],
        "restarted": restarted,
        "pending_restart": was_running and not restarted,
        "known_hosts_removed": removed,
        "verified": verified,
    }
    try:
        result["local"] = sync_local_ssh(cfg, plan)
    except (AppError, OSError, ValueError) as exc:
        result["local_error"] = str(exc)
    return result
