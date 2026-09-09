"""Lifecycle support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any
import base64
import json
import re
import shlex
import uuid
from rich.progress import BarColumn, Progress, SpinnerColumn, TaskProgressColumn, TextColumn, TimeElapsedColumn

from .cloudinit import cicustom_value, remove_stale_create_snippets, snippet_names, stale_create_snippets, sync_snippets_to_node, write_snippets
from .config import Config
from .guest import derive_ip, extract_mac, guest_exec_on_node, guest_out_on_node, parse_disk_size_gb, wait_for_qga_on_node
from .models import AppError, WORKSPACE_TAG, integer_value, validate_name
from .proxmox import allocate_named_raw_volume, boot_order_contains_disk, check_remote_requirements, cluster_nodes, cluster_vm_resource, disk_option, has_tag, home_label, home_volume_name, node_run, node_shell_command, parse_home_size, qm_config_on_node, qm_exists_on_node, qm_status_on_node, rename_attached_disk_volume, require_gold_tag, resolve_homestack_storage, root_volume_name, set_workspace_role_tags, shutdown_vm_on_node, storage_capacity, verify_workspace_role_tags
from .status import occupancy_level, resolve_existing_workspace, vm_volume_inventory
from .transports.base import Transport
from .ui import console, human_bytes, show_create_result, show_kv_panel, show_refresh_result, ui_vm_status
from .workspace_ssh import forget_local_ssh_host, get_workspace_authorized_keys, parse_authorized_key_records, remove_local_ssh_config, write_local_ssh_config

CLONE_TRANSFER_RE = re.compile(
    r"transferred\s+([0-9.]+)\s+([KMGT]?i?B)\s+of\s+"
    r"([0-9.]+)\s+([KMGT]?i?B)\s+\(([0-9.]+)%\)"
)


MIGRATION_LV_CREATED_RE = re.compile(r'Logical volume "([^"]+)" created\.')


MIGRATION_IMPORTED_RE = re.compile(r"successfully imported '([^']+)'")


MIGRATION_DD_PROGRESS_RE = re.compile(
    r"([0-9]+)\s+bytes\s+\([^)]+\)\s+copied,\s+"
    r"[0-9.]+\s+s,\s+([0-9.]+)\s+([KMGT]?B/s)"
)


def resolve_workspace_target(session: Transport, cfg: Config, target: str | int) -> int:
    if isinstance(target, int):
        return target

    text = str(target).strip()
    if re.fullmatch(r"[0-9]+", text):
        return int(text)
    if not re.fullmatch(r"[A-Za-z][A-Za-z0-9.-]{0,62}", text):
        raise AppError(
            f"Invalid workspace target {text!r}. Use a numeric VMID or a workspace name "
            "whose first character is a letter."
        )

    data = session.run_json_value(
        "pvesh get /cluster/resources --type vm --output-format json",
        timeout=30,
    )
    if not isinstance(data, list):
        raise AppError("Proxmox VM inventory did not return a JSON array")

    matches: list[int] = []
    non_workspace_matches: list[int] = []
    for item in data:
        if not isinstance(item, dict) or item.get("type") != "qemu":
            continue
        if str(item.get("name") or "") != text:
            continue
        vmid = integer_value(item.get("vmid"))
        node = str(item.get("node") or "")
        if vmid is None or not node:
            continue
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        if has_tag(vm_cfg.get("tags"), WORKSPACE_TAG):
            matches.append(vmid)
        else:
            non_workspace_matches.append(vmid)

    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        raise AppError(
            f"Workspace name {text!r} is ambiguous; matching VMIDs: "
            + ", ".join(str(vmid) for vmid in sorted(matches))
        )
    if non_workspace_matches:
        raise AppError(
            f"VM name {text!r} exists but is not a HomeStack workspace "
            f"(VMID {non_workspace_matches[0]})."
        )
    raise AppError(f"No HomeStack workspace named {text!r} exists.")


def run_transfer_with_progress(
    session: Transport,
    cfg: Config,
    node: str,
    command: str,
    *,
    progress: Progress | None,
    description: str,
    timeout: int = 3600,
) -> None:
    routed_command = node_shell_command(cfg, node, command)
    if progress is None:
        session.run(routed_command, timeout=timeout)
        return

    transfer_task = progress.add_task(description, total=100)
    last_percent = -1.0

    def update_transfer(output: str) -> None:
        nonlocal last_percent
        matches = list(CLONE_TRANSFER_RE.finditer(output))
        if not matches:
            return
        match = matches[-1]
        current, current_unit, total, total_unit, percent_text = match.groups()
        percent = max(0.0, min(100.0, float(percent_text)))
        if current_unit == total_unit:
            detail = f"{current} / {total} {current_unit}"
        else:
            detail = f"{current} {current_unit} / {total} {total_unit}"
        if percent != last_percent:
            progress.update(
                transfer_task,
                completed=percent,
                description=f"{description} {detail}",
            )
            last_percent = percent

    try:
        session.run_with_progress(
            routed_command,
            update_transfer,
            timeout=timeout,
            poll_interval=0.5,
        )
    finally:
        progress.remove_task(transfer_task)


def clone_full(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    name: str,
    storage: str,
    *,
    progress: Progress | None = None,
) -> None:
    command = shlex.join(
        [
            "qm",
            "clone",
            str(cfg.gold_vmid),
            str(vmid),
            "--name",
            name,
            "--full",
            "1",
            "--storage",
            storage,
        ]
    )
    run_transfer_with_progress(
        session,
        cfg,
        node,
        command,
        progress=progress,
        description="  ↳ Root disk",
        timeout=3600,
    )


def build_create_plan(
    session: Transport,
    cfg: Config,
    vmid: int,
    name: str,
    home_size: str,
    storage: str | None = None,
) -> dict[str, Any]:
    validate_name(name)
    ip = derive_ip(cfg, vmid)
    check_remote_requirements(session, cfg, cfg.node)

    gold_resource = cluster_vm_resource(session, cfg.gold_vmid)
    if gold_resource is None:
        raise AppError(f"Gold VM {cfg.gold_vmid} does not exist")
    gold_node = str(gold_resource.get("node") or "")
    if gold_node != cfg.node:
        raise AppError(
            f"Gold VM {cfg.gold_vmid} is on {gold_node!r}, expected configured node {cfg.node!r}"
        )
    if cluster_vm_resource(session, vmid) is not None:
        raise AppError(f"VMID {vmid} already exists")

    gold_cfg = qm_config_on_node(session, cfg, cfg.node, cfg.gold_vmid)
    require_gold_tag(cfg.gold_vmid, gold_cfg)
    disk_cfg = gold_cfg.get(cfg.root_disk)
    if not disk_cfg:
        raise AppError(f"Gold VM {cfg.gold_vmid} has no {cfg.root_disk} disk")
    disk_size_gb = parse_disk_size_gb(disk_cfg)

    selected_storage = resolve_homestack_storage(cfg, cfg.node, storage)
    normalized_home_size, home_size_gib = parse_home_size(home_size)
    label = home_label(vmid)

    authorized_keys, key_source = get_workspace_authorized_keys(session, cfg)
    key_records = parse_authorized_key_records(authorized_keys)
    stale_snippets = stale_create_snippets(session, cfg, cfg.node, name)

    return {
        "command": "create",
        "vmid": vmid,
        "name": name,
        "node": cfg.node,
        "ip": ip,
        "cidr": cfg.network_cidr,
        "gateway": cfg.gateway,
        "gold_vmid": cfg.gold_vmid,
        "clone_type": "full",
        "root_storage": selected_storage,
        "root_disk": cfg.root_disk,
        "root_disk_gb": disk_size_gb,
        "root_disk_policy": "inherit Gold unchanged",
        "root_volume_name": root_volume_name(vmid),
        "home_storage": selected_storage,
        "home_disk": cfg.home_disk,
        "home_size": normalized_home_size,
        "home_size_gib": home_size_gib,
        "home_label": label,
        "home_policy": "persistent Proxmox disk; ext4 mounted by filesystem label",
        "home_volume_name": home_volume_name(vmid, cfg.user_name),
        "user": cfg.user_name,
        "uid": cfg.user_uid,
        "gid": cfg.user_gid,
        "ssh_key_source": key_source,
        "ssh_public_keys": [record["label"] for record in key_records],
        "stale_snippets": [str(path) for path in stale_snippets],
        "transport": cfg.transport_type,
        "config": str(cfg.path),
    }


def create_workspace(
    session: Transport,
    cfg: Config,
    plan: dict[str, Any],
    *,
    json_mode: bool,
) -> dict[str, Any]:
    vmid = int(plan["vmid"])
    node = str(plan["node"])
    name = str(plan["name"])
    ip = str(plan["ip"])
    home_fs_label = str(plan["home_label"])
    home_size_gib = int(plan["home_size_gib"])

    created_vm = False
    created_snippets: list[Path] = []
    boot_started = False

    progress: Progress | None = None
    overall = None

    try:
        if not json_mode:
            progress = Progress(
                SpinnerColumn(),
                TextColumn("[bold]{task.description}"),
                BarColumn(),
                TaskProgressColumn(),
                TimeElapsedColumn(),
                console=console,
                refresh_per_second=2,
            )
            progress.start()
            overall = progress.add_task("Create workspace", total=10)

        def start_step(description: str) -> None:
            if progress is not None and overall is not None:
                progress.update(overall, description=description)

        def finish_step() -> None:
            if progress is not None and overall is not None:
                progress.update(overall, advance=1)

        start_step("Stale Cloud-Init snippets")
        stale_paths = [str(path) for path in plan.get("stale_snippets", [])]
        if stale_paths:
            remove_stale_create_snippets(session, cfg, node, name, stale_paths)
        finish_step()

        start_step("SSH keys")
        authorized_keys, key_source = get_workspace_authorized_keys(session, cfg)
        expected_key_records = parse_authorized_key_records(authorized_keys)
        finish_step()

        start_step("Full clone")
        clone_full(
            session,
            cfg,
            node,
            vmid,
            name,
            str(plan["root_storage"]),
            progress=progress,
        )
        created_vm = True
        rename_attached_disk_volume(
            session,
            cfg,
            node,
            vmid,
            cfg.root_disk,
            root_volume_name(vmid),
        )
        finish_step()

        start_step("Persistent home disk")
        home_volume = allocate_named_raw_volume(
            session,
            cfg,
            node,
            str(plan["home_storage"]),
            vmid,
            home_volume_name(vmid, cfg.user_name),
            f"{home_size_gib}G",
        )
        home_spec = (
            f"{home_volume},discard=on,iothread=1,ssd=1,serial={home_fs_label}"
        )
        try:
            node_run(
                session,
                cfg,
                node,
                shlex.join(["qm", "set", str(vmid), f"--{cfg.home_disk}", home_spec]),
                timeout=600,
            )
        except Exception:
            node_run(
                session,
                cfg,
                node,
                shlex.join(["pvesm", "free", home_volume]),
                check=False,
                timeout=600,
            )
            raise
        finish_step()

        start_step("Clone identity")
        set_workspace_role_tags(session, cfg, node, vmid)
        cloned_cfg = qm_config_on_node(session, cfg, node, vmid)
        net0 = cloned_cfg.get("net0")
        if not net0:
            raise AppError(f"Cloned VM {vmid} has no net0")
        mac = extract_mac(net0)
        configured_home = cloned_cfg.get(cfg.home_disk, "")
        if disk_option(configured_home, "serial") != home_fs_label:
            raise AppError(
                f"VM {vmid} {cfg.home_disk} does not have expected serial {home_fs_label}"
            )
        finish_step()

        start_step("Cloud-Init snippets")
        paths = write_snippets(
            session,
            cfg,
            node,
            name,
            vmid,
            mac,
            ip,
            home_fs_label,
            authorized_keys=authorized_keys,
        )
        created_snippets = list(paths.values())
        finish_step()

        start_step("VM configuration")
        node_run(
            session,
            cfg,
            node,
            shlex.join(
                [
                    "qm",
                    "set",
                    str(vmid),
                    "--ipconfig0",
                    f"ip={ip}/{cfg.network_cidr},gw={cfg.gateway}",
                ]
            )
        )
        node_run(session, cfg, node, shlex.join(["qm", "set", str(vmid), "--cicustom", cicustom_value(cfg, name)]))
        node_run(session, cfg, node, shlex.join(["qm", "cloudinit", "update", str(vmid)]))
        verify_workspace_role_tags(vmid, qm_config_on_node(session, cfg, node, vmid))
        finish_step()

        start_step("Boot")
        node_run(session, cfg, node, shlex.join(["qm", "start", str(vmid)]))
        boot_started = True
        finish_step()

        start_step("Verify workspace")
        wait_for_qga_on_node(session, cfg, node, vmid, timeout=600)
        guest_exec_on_node(
            session,
            cfg,
            node,
            vmid,
            "command -v cloud-init >/dev/null 2>&1 && "
            "timeout 180 cloud-init status --wait >/dev/null 2>&1 || true",
            check=False,
        )

        actual_hostname = guest_out_on_node(session, cfg, node, vmid, "hostname")
        if actual_hostname != name:
            raise AppError(f"Verification failed: hostname is {actual_hostname!r}, expected {name!r}")

        actual_addr = guest_out_on_node(session, cfg, node, vmid, "ip -4 -o addr show dev eth0")
        expected_addr = f"{ip}/{cfg.network_cidr}"
        if expected_addr not in actual_addr:
            raise AppError(f"Verification failed: eth0 does not have {expected_addr}\n{actual_addr}")

        mount = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            f"findmnt -n -o SOURCE,FSTYPE,TARGET /home/{cfg.user_name}",
        )
        if "ext4" not in mount or f"/home/{cfg.user_name}" not in mount:
            raise AppError(
                f"Verification failed: /home/{cfg.user_name} is not mounted as ext4\n{mount}"
            )
        mounted_label = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            f"findmnt -n -o SOURCE /home/{cfg.user_name} | xargs -r blkid -s LABEL -o value",
        )
        if mounted_label != home_fs_label:
            raise AppError(
                f"Verification failed: persistent home label is {mounted_label!r}, "
                f"expected {home_fs_label!r}"
            )

        user_id = guest_out_on_node(session, cfg, node, vmid, f"id {shlex.quote(cfg.user_name)}")
        if f"uid={cfg.user_uid}({cfg.user_name})" not in user_id:
            raise AppError(f"Verification failed: unexpected user identity: {user_id}")

        user_key_ok = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            f"test -s /home/{cfg.user_name}/.ssh/authorized_keys && echo OK || echo MISSING",
        )
        if user_key_ok != "OK":
            raise AppError("Verification failed: user authorized_keys is missing")

        actual_authorized_keys = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            f"cat /home/{cfg.user_name}/.ssh/authorized_keys",
        )
        actual_key_records = parse_authorized_key_records(actual_authorized_keys)
        expected_identities = {record["identity"] for record in expected_key_records}
        actual_identities = {record["identity"] for record in actual_key_records}
        missing_records = [
            record for record in expected_key_records if record["identity"] not in actual_identities
        ]
        unexpected_records = [
            record for record in actual_key_records if record["identity"] not in expected_identities
        ]
        if missing_records or unexpected_records:
            parts: list[str] = []
            if missing_records:
                parts.append("missing: " + ", ".join(record["label"] for record in missing_records))
            if unexpected_records:
                parts.append("unexpected: " + ", ".join(record["label"] for record in unexpected_records))
            raise AppError("Verification failed: SSH public keys mismatch (" + "; ".join(parts) + ")")

        root_key_ok = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            "test -s /root/.ssh/authorized_keys && echo OK || echo MISSING",
        )
        if root_key_ok != "OK":
            raise AppError("Verification failed: root authorized_keys is missing")

        regular_users = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            """awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd""",
        ).splitlines()
        expected_regular_user = f"{cfg.user_name}:{cfg.user_uid}:{cfg.user_gid}"
        if regular_users != [expected_regular_user]:
            raise AppError(
                "Verification failed: unexpected regular user accounts: "
                + (", ".join(regular_users) if regular_users else "none")
            )

        sudo_state = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            "if command -v sudo >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
        )
        if sudo_state != "ABSENT":
            raise AppError("Verification failed: sudo is installed in the workspace")

        cloud_id = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            "command -v cloud-id >/dev/null 2>&1 && cloud-id || true",
            check=False,
        )
        finish_step()

        ssh_known_hosts_removed = forget_local_ssh_host(ip)
        final_status = qm_status_on_node(session, cfg, node, vmid)

        start_step("Local SSH config")
        try:
            ssh_config_path = write_local_ssh_config(cfg, vmid, name, ip)
        except AppError as exc:
            raise AppError(
                f"Workspace VM {vmid} ({name}) was created and verified successfully, "
                f"but local SSH config generation failed: {exc}"
            ) from exc
        finish_step()

        result = {
            "ok": True,
            **plan,
            "status": final_status,
            "mac": mac,
            "ssh_key_source": key_source,
            "ssh_public_keys": [
                {"label": record["label"], "verified": True}
                for record in expected_key_records
            ],
            "cloud_id": cloud_id or None,
            "ssh_known_hosts_removed": ssh_known_hosts_removed,
            "ssh_config_path": str(ssh_config_path),
            "ssh": {
                "root": f"ssh root@{ip}",
                "user": f"ssh {cfg.user_name}@{ip}",
            },
            "sync_command": f"homestack sync {vmid}" if cfg.sync_paths else None,
        }

        if progress is not None:
            progress.stop()
        if not json_mode:
            show_create_result(result)
        return result

    except Exception:
        if progress is not None:
            progress.stop()

        if not boot_started:
            if created_vm and qm_exists_on_node(session, cfg, node, vmid):
                node_run(session, cfg, node, f"qm destroy {vmid} --purge 1", check=False, timeout=600)
            for path in created_snippets:
                node_run(session, cfg, node, f"rm -f {shlex.quote(str(path))}", check=False)
        raise


def build_destroy_plan(session: Transport, cfg: Config, vmid: int) -> dict[str, Any]:
    info = resolve_existing_workspace(session, cfg, vmid, require_network=False)
    check_remote_requirements(session, cfg, str(info["node"]))
    usage = info["home_usage"]
    size_bytes = usage.get("size_bytes")
    used_bytes = usage.get("used_bytes")
    root_storage = str(info.get("root_storage") or "—")
    home_storage = str(info.get("home_storage") or "—")
    storage_status: list[dict[str, Any]] = []
    for storage in dict.fromkeys((root_storage, home_storage)):
        try:
            data = session.run_json_value(
                "pvesh get "
                f"/nodes/{shlex.quote(str(info['node']))}/storage/{shlex.quote(storage)}/status "
                "--output-format json",
                timeout=30,
            )
            capacity = (
                storage_capacity(storage, data)
                if isinstance(data, dict)
                else storage_capacity(storage, {})
            )
        except AppError:
            capacity = storage_capacity(storage, {})
        storage_status.append(capacity)

    percent = None
    if size_bytes and used_bytes is not None:
        percent = 100.0 * int(used_bytes) / int(size_bytes)
    return {
        "command": "destroy",
        "vmid": vmid,
        "name": info["name"],
        "node": info["node"],
        "status": info["status"],
        "ip": derive_ip(cfg, vmid),
        "root_disk": info["root_disk"],
        "root_storage": root_storage,
        "root_size_gib": parse_disk_size_gb(str(info["root_disk_config"])),
        "home_disk": info["home_disk"],
        "home_storage": home_storage,
        "home_size_gib": parse_disk_size_gb(str(info["home_disk_config"])),
        "storage_status": storage_status,
        "home_volume": info["home_volume"],
        "home_label": info["home_label"],
        "home_used_bytes": used_bytes,
        "home_capacity_bytes": size_bytes,
        "home_percent": percent,
        "home_risk": occupancy_level(percent),
        "root_action": "delete",
        "home_action": "delete permanently with VM",
        "config": str(cfg.path),
    }


def destroy_workspace(
    session: Transport,
    cfg: Config,
    plan: dict[str, Any],
) -> dict[str, Any]:
    vmid = int(plan["vmid"])
    node = str(plan["node"])
    name = str(plan["name"])

    shutdown_vm_on_node(session, cfg, node, vmid)
    current = resolve_existing_workspace(session, cfg, vmid, require_network=False)
    if current["home_label"] != plan["home_label"]:
        raise AppError("Persistent home identity changed after destroy confirmation")

    node_run(session, cfg, node, f"qm destroy {vmid} --purge 1", timeout=600)
    if cluster_vm_resource(session, vmid) is not None:
        raise AppError(f"VM {vmid} still exists after qm destroy")

    names = snippet_names(name)
    for cleanup_node in cluster_nodes(session):
        filenames = [*names.values(), _refresh_journal_path(cfg, vmid).name]
        for filename in filenames:
            node_run(
                session,
                cfg,
                cleanup_node,
                f"rm -f {shlex.quote(str(cfg.snippet_dir / filename))}",
                check=False,
            )

    try:
        ssh_config_removed = remove_local_ssh_config(vmid)
        ssh_known_hosts_removed = forget_local_ssh_host(str(plan["ip"]))
    except AppError as exc:
        raise AppError(
            f"Workspace VM {vmid} ({name}) was destroyed successfully, "
            f"but local SSH cleanup failed: {exc}"
        ) from exc

    return {
        "ok": True,
        **plan,
        "status": "absent",
        "vm_deleted": True,
        "home_deleted": True,
        "ssh_config_removed": [str(path) for path in ssh_config_removed],
        "ssh_known_hosts_removed": ssh_known_hosts_removed,
    }


def _refresh_journal_path(cfg: Config, vmid: int) -> Path:
    return cfg.snippet_dir / f"homestack-vm{vmid}-refresh.json"


def _read_refresh_journal(
    session: Transport, cfg: Config, node: str, vmid: int
) -> dict[str, Any] | None:
    path = _refresh_journal_path(cfg, vmid)
    result = node_run(
        session,
        cfg,
        node,
        f"test -s {shlex.quote(str(path))} && cat {shlex.quote(str(path))}",
        check=False,
    )
    if result.returncode == 1:
        return None
    if result.returncode != 0:
        raise AppError(f"Could not read refresh journal {path}: {result.output}")
    try:
        value = json.loads(result.output)
    except json.JSONDecodeError as exc:
        raise AppError(f"Refresh journal {path} is not valid JSON") from exc
    try:
        journal_vmid = int(value.get("vmid", -1)) if isinstance(value, dict) else -1
    except (TypeError, ValueError):
        journal_vmid = -1
    if not isinstance(value, dict) or journal_vmid != vmid:
        raise AppError(f"Refresh journal {path} has an invalid VM identity")
    _validate_refresh_journal(cfg, value)
    return value


def _validate_refresh_journal(cfg: Config, journal: dict[str, Any]) -> None:
    try:
        vmid = int(journal["vmid"])
    except (KeyError, TypeError, ValueError) as exc:
        raise AppError("Refresh journal has an invalid VM identity") from exc
    if journal.get("version") != 1:
        raise AppError("Refresh journal has an unsupported version")
    transaction_id = str(journal.get("transaction_id") or "")
    if re.fullmatch(r"[0-9a-f]{32}", transaction_id) is None:
        raise AppError("Refresh journal has an invalid transaction identity")
    if journal.get("phase") not in {
        "prepared",
        "imported",
        "switching",
        "switched",
        "verifying",
        "verified",
    }:
        raise AppError("Refresh journal has an invalid phase")
    if journal.get("initial_status") not in {"running", "stopped"}:
        raise AppError("Refresh journal has an invalid initial power state")
    if re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9.-]*", str(journal.get("node") or "")
    ) is None:
        raise AppError("Refresh journal has an invalid node")
    if journal.get("root_disk") != cfg.root_disk or journal.get("home_disk") != cfg.home_disk:
        raise AppError("Current disk-slot configuration does not match the refresh journal")
    if cfg.root_disk == cfg.home_disk:
        raise AppError("Root and persistent-home disk slots must be different")
    if re.fullmatch(r"unused[0-9]+", str(journal.get("new_unused_key") or "")) is None:
        raise AppError("Refresh journal has an invalid staging disk slot")

    old_volume = str(journal.get("old_root_volume") or "")
    home_volume = str(journal.get("home_volume") or "")
    old_spec = str(journal.get("old_root_spec") or "")
    new_volume = str(journal.get("new_root_volume") or "")
    phase = str(journal["phase"])
    if ":" not in old_volume or old_spec.split(",", 1)[0].strip() != old_volume:
        raise AppError("Refresh journal has an invalid original root identity")
    if ":" not in home_volume or home_volume == old_volume:
        raise AppError("Refresh journal has an invalid persistent-home identity")
    if str(journal.get("home_label") or "") != home_label(vmid):
        raise AppError("Refresh journal has an invalid persistent-home label")
    if new_volume and (":" not in new_volume or new_volume in {old_volume, home_volume}):
        raise AppError("Refresh journal has an invalid staged root identity")
    if phase != "prepared" and not new_volume:
        raise AppError("Refresh journal is missing the staged root identity")
    old_unused_key = str(journal.get("old_unused_key") or "")
    if old_unused_key and re.fullmatch(r"unused[0-9]+", old_unused_key) is None:
        raise AppError("Refresh journal has an invalid original-root staging slot")


def _write_refresh_journal(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    journal: dict[str, Any],
) -> None:
    path = _refresh_journal_path(cfg, vmid)
    payload = base64.b64encode(
        (json.dumps(journal, sort_keys=True) + "\n").encode("utf-8")
    ).decode("ascii")
    directory = shlex.quote(str(path.parent))
    target = shlex.quote(str(path))
    command = (
        f"mkdir -p {directory} && "
        f"hs_journal=$(mktemp {directory}/.homestack-refresh.XXXXXX) && "
        "trap 'rm -f \"$hs_journal\"' EXIT HUP INT TERM && "
        f"printf %s {shlex.quote(payload)} | base64 -d > \"$hs_journal\" && "
        "chmod 600 \"$hs_journal\" && "
        f"mv -f \"$hs_journal\" {target}"
    )
    node_run(session, cfg, node, command)


def _remove_refresh_journal(
    session: Transport, cfg: Config, node: str, vmid: int
) -> None:
    node_run(
        session,
        cfg,
        node,
        f"rm -f {shlex.quote(str(_refresh_journal_path(cfg, vmid)))}",
    )


def _unused_refs(vm_cfg: dict[str, str]) -> dict[str, str]:
    return {
        key: str(value).split(",", 1)[0].strip()
        for key, value in vm_cfg.items()
        if re.fullmatch(r"unused[0-9]+", key)
    }


def _unused_key_for_volume(vm_cfg: dict[str, str], volume: str) -> str | None:
    matches = [key for key, value in _unused_refs(vm_cfg).items() if value == volume]
    if len(matches) > 1:
        raise AppError(f"Volume {volume!r} has multiple unused references: {matches}")
    return matches[0] if matches else None


def _free_unused_key(vm_cfg: dict[str, str]) -> str:
    for index in range(256):
        key = f"unused{index}"
        if key not in vm_cfg:
            return key
    raise AppError("VM has no free unused disk slot for a staged root")


def _digest_args(vm_cfg: dict[str, str]) -> list[str]:
    digest = str(vm_cfg.get("digest") or "").strip()
    return ["--digest", digest] if digest else []


def _root_attachment_spec(volume: str, gold_disk_config: str) -> str:
    parts = [volume]
    for key in ("iothread", "discard", "ssd", "cache", "aio", "backup", "replicate", "ro"):
        value = disk_option(gold_disk_config, key)
        if value is not None:
            parts.append(f"{key}={value}")
    return ",".join(parts)


def _assert_refresh_home(
    cfg: Config, vmid: int, vm_cfg: dict[str, str], journal: dict[str, Any]
) -> None:
    home_cfg = str(vm_cfg.get(cfg.home_disk) or "")
    home_volume = home_cfg.split(",", 1)[0].strip()
    expected_volume = str(journal["home_volume"])
    expected_label = str(journal["home_label"])
    if home_volume != expected_volume or disk_option(home_cfg, "serial") != expected_label:
        raise AppError(
            f"Persistent home identity changed during refresh of VM {vmid}; "
            "refusing further disk changes"
        )


def _unlink_disk(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    disk: str,
    vm_cfg: dict[str, str],
    *,
    force: bool,
) -> dict[str, str]:
    args = ["qm", "disk", "unlink", str(vmid), "--idlist", disk]
    if force:
        args.extend(["--force", "1"])
    args.extend(_digest_args(vm_cfg))
    node_run(session, cfg, node, shlex.join(args), timeout=600)
    return qm_config_on_node(session, cfg, node, vmid)


def _set_vm_values(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    vm_cfg: dict[str, str],
    values: list[tuple[str, str]],
) -> dict[str, str]:
    args = ["qm", "set", str(vmid)]
    for key, value in values:
        args.extend([f"--{key}", value])
    args.extend(_digest_args(vm_cfg))
    node_run(session, cfg, node, shlex.join(args), timeout=600)
    return qm_config_on_node(session, cfg, node, vmid)


def _delete_unused_volume(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    vm_cfg: dict[str, str],
    volume: str,
    *,
    require_reference: bool = False,
) -> dict[str, str]:
    key = _unused_key_for_volume(vm_cfg, volume)
    if key is None:
        if require_reference:
            raise AppError(
                f"Cannot safely delete volume {volume!r}: its unused reference is missing"
            )
        return vm_cfg
    return _unlink_disk(session, cfg, node, vmid, key, vm_cfg, force=True)


def _verify_refreshed_guest(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    name: str,
    ip: str,
    cidr: int,
    home_fs_label: str,
) -> str | None:
    wait_for_qga_on_node(session, cfg, node, vmid, timeout=600)
    guest_exec_on_node(
        session,
        cfg,
        node,
        vmid,
        "command -v cloud-init >/dev/null 2>&1 && "
        "timeout 180 cloud-init status --wait >/dev/null 2>&1 || true",
        check=False,
        timeout=240,
    )
    if guest_out_on_node(session, cfg, node, vmid, "hostname") != name:
        raise AppError("Refresh verification failed: workspace hostname is incorrect")
    actual_addr = guest_out_on_node(session, cfg, node, vmid, "ip -4 -o addr show dev eth0")
    if f"{ip}/{cidr}" not in actual_addr:
        raise AppError(f"Refresh verification failed: eth0 does not have {ip}/{cidr}")
    mount = guest_out_on_node(
        session, cfg, node, vmid, f"findmnt -n -o SOURCE,FSTYPE,TARGET /home/{cfg.user_name}"
    )
    if "ext4" not in mount or f"/home/{cfg.user_name}" not in mount:
        raise AppError(f"Refresh verification failed: /home/{cfg.user_name} is not ext4")
    mounted_label = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        f"findmnt -n -o SOURCE /home/{cfg.user_name} | xargs -r blkid -s LABEL -o value",
    )
    if mounted_label != home_fs_label:
        raise AppError(f"Refresh verification failed: home label is {mounted_label!r}")
    user_id = guest_out_on_node(
        session, cfg, node, vmid, f"id {shlex.quote(cfg.user_name)}"
    )
    if f"uid={cfg.user_uid}({cfg.user_name})" not in user_id:
        raise AppError(f"Refresh verification failed: unexpected user identity: {user_id}")
    for command, message in (
        (
            f"test -s /home/{cfg.user_name}/.ssh/authorized_keys && echo OK || echo MISSING",
            "persistent user authorized_keys is missing",
        ),
        ("test -s /root/.ssh/authorized_keys && echo OK || echo MISSING", "root authorized_keys is missing"),
    ):
        if guest_out_on_node(session, cfg, node, vmid, command) != "OK":
            raise AppError(f"Refresh verification failed: {message}")
    regular_users = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        """awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd""",
    ).splitlines()
    if regular_users != [f"{cfg.user_name}:{cfg.user_uid}:{cfg.user_gid}"]:
        raise AppError("Refresh verification failed: unexpected regular user accounts")
    sudo_state = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "if command -v sudo >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
    )
    if sudo_state != "ABSENT":
        raise AppError("Refresh verification failed: sudo is installed in the workspace")
    return guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "command -v cloud-id >/dev/null 2>&1 && cloud-id || true",
        check=False,
    ) or None


def _recover_refresh(
    session: Transport, cfg: Config, journal: dict[str, Any]
) -> dict[str, Any]:
    _validate_refresh_journal(cfg, journal)
    vmid = int(journal["vmid"])
    node = str(journal["node"])
    phase = str(journal.get("phase") or "")
    current_status = qm_status_on_node(session, cfg, node, vmid)
    if current_status == "running":
        shutdown_vm_on_node(session, cfg, node, vmid)
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    _assert_refresh_home(cfg, vmid, vm_cfg, journal)
    old_volume = str(journal["old_root_volume"])
    new_volume = str(journal.get("new_root_volume") or "")
    if not new_volume:
        candidate = _unused_refs(vm_cfg).get(str(journal.get("new_unused_key") or ""), "")
        if candidate and candidate not in {old_volume, str(journal["home_volume"])}:
            new_volume = candidate
            journal["new_root_volume"] = candidate

    if phase == "verified":
        if str(vm_cfg.get(cfg.root_disk, "")).split(",", 1)[0] != new_volume:
            raise AppError("Verified refresh journal does not match the attached root")
        vm_cfg = _delete_unused_volume(
            session, cfg, node, vmid, vm_cfg, old_volume
        )
        if str(journal["initial_status"]) == "running":
            node_run(session, cfg, node, f"qm start {vmid}", timeout=300)
        _remove_refresh_journal(session, cfg, node, vmid)
        return {"mode": "recover", "recovery": "completed verified cleanup"}

    attached_root = str(vm_cfg.get(cfg.root_disk) or "")
    attached_volume = attached_root.split(",", 1)[0].strip()
    if attached_volume == new_volume and new_volume:
        vm_cfg = _unlink_disk(
            session, cfg, node, vmid, cfg.root_disk, vm_cfg, force=False
        )
        _assert_refresh_home(cfg, vmid, vm_cfg, journal)
        attached_volume = ""
    if attached_volume not in {"", old_volume}:
        raise AppError(
            f"Refresh recovery found unexpected root volume {attached_volume!r}"
        )
    if not attached_volume:
        if _unused_key_for_volume(vm_cfg, old_volume) is None:
            raise AppError("Refresh recovery cannot find the original root volume")
        vm_cfg = _set_vm_values(
            session,
            cfg,
            node,
            vmid,
            vm_cfg,
            [(cfg.root_disk, str(journal["old_root_spec"]))],
        )
    if str(vm_cfg.get(cfg.root_disk, "")).split(",", 1)[0] != old_volume:
        raise AppError("Refresh recovery did not restore the original root")
    if _unused_key_for_volume(vm_cfg, old_volume) is not None:
        raise AppError("Original root is both attached and retained as an unused disk")
    _assert_refresh_home(cfg, vmid, vm_cfg, journal)
    if new_volume:
        vm_cfg = _delete_unused_volume(
            session,
            cfg,
            node,
            vmid,
            vm_cfg,
            new_volume,
            require_reference=True,
        )
    restore_values = [
        (key, str(journal[key]))
        for key in ("boot", "ipconfig0", "cicustom")
        if journal.get(key)
    ]
    if restore_values:
        vm_cfg = _set_vm_values(
            session, cfg, node, vmid, vm_cfg, restore_values
        )
    node_run(session, cfg, node, f"qm cloudinit update {vmid}", check=False, timeout=120)
    if str(journal["initial_status"]) == "running":
        node_run(session, cfg, node, f"qm start {vmid}", timeout=300)
    _remove_refresh_journal(session, cfg, node, vmid)
    return {"mode": "recover", "recovery": "rolled back to original root"}


def build_refresh_plan(session: Transport, cfg: Config, vmid: int) -> dict[str, Any]:
    resource = cluster_vm_resource(session, vmid)
    if resource is None:
        raise AppError(f"VMID {vmid} does not exist")
    node = str(resource.get("node") or "")
    if not node:
        raise AppError(f"VM {vmid} has no node in cluster inventory")
    journal = _read_refresh_journal(session, cfg, node, vmid)
    if journal is not None:
        if str(journal.get("node") or "") != node:
            raise AppError(
                f"Refresh journal belongs to node {journal.get('node')!r}, but VM {vmid} is on {node!r}"
            )
        return {
            "command": "refresh",
            "mode": "recover",
            "vmid": vmid,
            "name": str(journal.get("name") or f"VM {vmid}"),
            "node": node,
            "status": str(resource.get("status") or "unknown"),
            "home_label": journal.get("home_label"),
            "home_volume": journal.get("home_volume"),
            "recovery_phase": journal.get("phase"),
            "journal": journal,
            "config": str(cfg.path),
        }

    check_remote_requirements(session, cfg, node)
    info = resolve_existing_workspace(session, cfg, vmid, require_network=True)
    if info["node"] != cfg.node:
        raise AppError(
            f"Refresh currently requires workspace VM {vmid} on Gold node {cfg.node}; "
            f"it is on {info['node']}. Migrate it back first."
        )
    root_storage = str(info.get("root_storage") or "")
    resolve_homestack_storage(cfg, info["node"], root_storage)

    gold_resource = cluster_vm_resource(session, cfg.gold_vmid)
    if gold_resource is None:
        raise AppError(f"Gold VM {cfg.gold_vmid} does not exist")
    gold_node = str(gold_resource.get("node") or "")
    if gold_node != cfg.node:
        raise AppError(
            f"Gold VM {cfg.gold_vmid} is on {gold_node!r}, expected configured node {cfg.node!r}"
        )
    gold_cfg = qm_config_on_node(session, cfg, gold_node, cfg.gold_vmid)
    require_gold_tag(cfg.gold_vmid, gold_cfg)
    disk_cfg = gold_cfg.get(cfg.root_disk)
    if not disk_cfg:
        raise AppError(f"Gold VM {cfg.gold_vmid} has no {cfg.root_disk} disk")
    return {
        "command": "refresh",
        "mode": "replace",
        "vmid": vmid,
        "name": info["name"],
        "node": info["node"],
        "status": info["status"],
        "ip": info["ip"],
        "cidr": info["cidr"],
        "gateway": info["gateway"],
        "gold_vmid": cfg.gold_vmid,
        "root_storage": root_storage,
        "root_disk": cfg.root_disk,
        "root_disk_gb": parse_disk_size_gb(disk_cfg),
        "gold_root_disk_config": disk_cfg,
        "gold_root_volume": disk_cfg.split(",", 1)[0],
        "home_disk": info["home_disk"],
        "home_storage": info["home_storage"],
        "home_label": info["home_label"],
        "home_volume": info["home_volume"],
        "home_policy": "preserve persistent disk exactly; never format during refresh",
        "power_state_policy": "verify by boot and restore pre-refresh power state",
        "config": str(cfg.path),
    }


def refresh_workspace(
    session: Transport,
    cfg: Config,
    plan: dict[str, Any],
    *,
    json_mode: bool,
) -> dict[str, Any]:
    if plan.get("mode") == "recover":
        recovery = _recover_refresh(session, cfg, dict(plan["journal"]))
        result = {"ok": True, **plan, **recovery, "status": "recovered"}
        if not json_mode:
            show_kv_panel(
                "REFRESH RECOVERED",
                [[("VMID", str(plan["vmid"])), ("Recovery", str(recovery["recovery"]))]],
            )
        return result

    vmid = int(plan["vmid"])
    node = str(plan["node"])
    name = str(plan["name"])
    ip = str(plan["ip"])
    cidr = int(plan["cidr"])
    gateway = str(plan["gateway"])
    home_fs_label = str(plan["home_label"])
    initial_status = str(plan.get("status") or "")
    if initial_status not in {"running", "stopped"}:
        raise AppError(
            f"Workspace VM {vmid} has unsupported pre-refresh power state {initial_status!r}"
        )

    progress: Progress | None = None
    overall = None
    journal: dict[str, Any] | None = None
    try:
        if not json_mode:
            progress = Progress(
                SpinnerColumn(), TextColumn("[bold]{task.description}"), BarColumn(),
                TaskProgressColumn(), TimeElapsedColumn(), console=console,
                refresh_per_second=2,
            )
            progress.start()
            overall = progress.add_task("Refresh workspace", total=9)

        def step(description: str) -> None:
            if progress is not None and overall is not None:
                progress.update(overall, description=description)

        def done() -> None:
            if progress is not None and overall is not None:
                progress.update(overall, advance=1)

        current = resolve_existing_workspace(session, cfg, vmid, require_network=True)
        if str(current["status"]) != initial_status:
            raise AppError("Workspace power state changed after refresh confirmation")
        if current["home_volume"] != plan["home_volume"]:
            raise AppError("Persistent home identity changed after refresh confirmation")
        vm_cfg = dict(current["vm_config"])
        boot_config = str(vm_cfg.get("boot") or "")
        if not boot_order_contains_disk(boot_config, cfg.root_disk):
            raise AppError(f"Workspace boot order does not include {cfg.root_disk}")
        net0 = str(vm_cfg.get("net0") or "")
        if not net0:
            raise AppError(f"Workspace VM {vmid} has no net0")
        mac = extract_mac(net0)
        unused_key = _free_unused_key(vm_cfg)
        journal = {
            "version": 1,
            "transaction_id": uuid.uuid4().hex,
            "phase": "prepared",
            "vmid": vmid,
            "name": name,
            "node": node,
            "initial_status": initial_status,
            "old_root_spec": str(vm_cfg[cfg.root_disk]),
            "old_root_volume": str(current["root_volume"]),
            "root_disk": cfg.root_disk,
            "new_root_volume": "",
            "new_unused_key": unused_key,
            "old_unused_key": "",
            "home_volume": str(plan["home_volume"]),
            "home_label": home_fs_label,
            "home_disk": cfg.home_disk,
            "boot": boot_config,
            "ipconfig0": str(vm_cfg.get("ipconfig0") or ""),
            "cicustom": str(vm_cfg.get("cicustom") or ""),
        }
        _write_refresh_journal(session, cfg, node, vmid, journal)

        step("Import Gold root safely")
        source = node_run(
            session, cfg, node,
            shlex.join(["pvesm", "path", str(plan["gold_root_volume"])]),
        ).output.strip().splitlines()
        if not source or not source[-1].strip():
            raise AppError("Proxmox did not return a path for the Gold root volume")
        import_command = shlex.join(
            ["qm", "disk", "import", str(vmid), source[-1].strip(),
             str(plan["root_storage"]), "--target-disk", unused_key]
        )
        run_transfer_with_progress(
            session, cfg, node, import_command, progress=progress,
            description="  ↳ Gold root", timeout=3600,
        )
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        _assert_refresh_home(cfg, vmid, vm_cfg, journal)
        new_volume = _unused_refs(vm_cfg).get(unused_key, "")
        if not new_volume or new_volume in {journal["old_root_volume"], journal["home_volume"]}:
            raise AppError("Imported root did not produce a distinct unused volume")
        journal["new_root_volume"] = new_volume
        journal["phase"] = "imported"
        _write_refresh_journal(session, cfg, node, vmid, journal)
        done()

        step("Stop workspace")
        if initial_status == "running":
            shutdown_vm_on_node(session, cfg, node, vmid)
        done()

        step("Switch root disks")
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        _assert_refresh_home(cfg, vmid, vm_cfg, journal)
        if str(vm_cfg[cfg.root_disk]).split(",", 1)[0] != journal["old_root_volume"]:
            raise AppError("Root identity changed before refresh switch")
        journal["phase"] = "switching"
        _write_refresh_journal(session, cfg, node, vmid, journal)
        vm_cfg = _unlink_disk(
            session, cfg, node, vmid, cfg.root_disk, vm_cfg, force=False
        )
        old_unused = _unused_key_for_volume(vm_cfg, str(journal["old_root_volume"]))
        if old_unused is None:
            raise AppError("Proxmox did not retain the original root as an unused disk")
        journal["old_unused_key"] = old_unused
        _write_refresh_journal(session, cfg, node, vmid, journal)
        vm_cfg = _set_vm_values(
            session, cfg, node, vmid, vm_cfg,
            [(cfg.root_disk, _root_attachment_spec(new_volume, str(plan["gold_root_disk_config"])))],
        )
        if str(vm_cfg.get(cfg.root_disk, "")).split(",", 1)[0] != new_volume:
            raise AppError("New root was not attached to the configured root slot")
        if _unused_key_for_volume(vm_cfg, new_volume) is not None:
            raise AppError("New root is both attached and retained as an unused disk")
        _assert_refresh_home(cfg, vmid, vm_cfg, journal)
        journal["phase"] = "switched"
        _write_refresh_journal(session, cfg, node, vmid, journal)
        done()

        step("Cloud-Init snippets")
        write_snippets(
            session, cfg, node, name, vmid, mac, ip, home_fs_label,
            replace=True, preserve_home=True, cidr=cidr, gateway=gateway,
        )
        done()

        step("VM configuration")
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        vm_cfg = _set_vm_values(
            session, cfg, node, vmid, vm_cfg,
            [("boot", boot_config), ("ipconfig0", f"ip={ip}/{cidr},gw={gateway}"),
             ("cicustom", cicustom_value(cfg, name))],
        )
        verify_workspace_role_tags(vmid, vm_cfg)
        node_run(session, cfg, node, f"qm cloudinit update {vmid}", timeout=120)
        done()

        step("Boot verification")
        journal["phase"] = "verifying"
        _write_refresh_journal(session, cfg, node, vmid, journal)
        node_run(session, cfg, node, f"qm start {vmid}", timeout=300)
        cloud_id = _verify_refreshed_guest(
            session, cfg, node, vmid, name, ip, cidr, home_fs_label
        )
        done()

        step("Restore power state")
        if initial_status == "stopped":
            shutdown_vm_on_node(session, cfg, node, vmid)
        done()

        step("Delete previous root")
        journal["phase"] = "verified"
        _write_refresh_journal(session, cfg, node, vmid, journal)
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        _assert_refresh_home(cfg, vmid, vm_cfg, journal)
        _delete_unused_volume(
            session,
            cfg,
            node,
            vmid,
            vm_cfg,
            str(journal["old_root_volume"]),
            require_reference=True,
        )
        _remove_refresh_journal(session, cfg, node, vmid)
        journal = None
        done()

        final_status = qm_status_on_node(session, cfg, node, vmid)
        if final_status != initial_status:
            raise AppError(
                f"Refresh completed with power state {final_status!r}, expected {initial_status!r}"
            )
        ssh_known_hosts_removed = forget_local_ssh_host(ip)
        result = {
            "ok": True, **plan, "status": final_status, "mac": mac,
            "home_preserved": True, "guest_verified": True,
            "power_state_preserved": True, "cloud_id": cloud_id,
            "rollback_performed": False, "cleanup_pending": False,
            "ssh_known_hosts_removed": ssh_known_hosts_removed,
            "ssh": {"root": f"ssh root@{ip}", "user": f"ssh {cfg.user_name}@{ip}"},
        }
        if progress is not None:
            progress.stop()
        if not json_mode:
            show_refresh_result(result)
        return result
    except Exception as exc:
        if progress is not None:
            progress.stop()
        if journal is None:
            raise
        try:
            recovery = _recover_refresh(session, cfg, journal)
        except Exception as recovery_exc:
            raise AppError(
                f"Refresh failed: {exc}. Automatic recovery also failed: {recovery_exc}. "
                f"Transaction journal retained at {_refresh_journal_path(cfg, vmid)}."
            ) from exc
        raise AppError(
            f"Refresh failed: {exc}. Automatic recovery succeeded: {recovery['recovery']}."
        ) from exc


def migration_volume_key(volume: str) -> str:
    tail = volume.split(":", 1)[-1].rsplit("/", 1)[-1]
    return re.sub(r"\\.(?:raw|qcow2|vmdk)$", "", tail)


def migration_volume_plan(vm_cfg: dict[str, str], cfg: Config) -> list[dict[str, Any]]:
    volumes: list[dict[str, Any]] = []
    for item in vm_volume_inventory(vm_cfg, cfg):
        if item.get("role") == "unused":
            continue
        slot = str(item.get("slot") or "")
        disk_cfg = vm_cfg.get(slot, "")
        size_gb = parse_disk_size_gb(disk_cfg)
        size_bytes = int(size_gb * 1024**3) if size_gb is not None else None
        volume = str(item.get("volume") or "")
        volumes.append(
            {
                "slot": slot,
                "role": str(item.get("role") or "disk"),
                "volume": volume,
                "name": migration_volume_key(volume),
                "size_bytes": size_bytes,
            }
        )
    return volumes


class MigrationProgressTracker:
    def __init__(self, volumes: list[dict[str, Any]]) -> None:
        self.volumes = {
            str(item.get("name") or ""): item
            for item in volumes
            if item.get("name")
        }
        self.current: str | None = None
        self.current_bytes = 0

    def update(self, output: str) -> dict[str, Any] | None:
        created = list(MIGRATION_LV_CREATED_RE.finditer(output))
        tail = output
        if created:
            current = created[-1].group(1)
            if current != self.current:
                self.current = current
                self.current_bytes = 0
            tail = output[created[-1].end():]

        copied = list(MIGRATION_DD_PROGRESS_RE.finditer(tail))
        speed = None
        if copied and self.current is not None:
            latest = copied[-1]
            self.current_bytes = int(latest.group(1))
            speed = f"{latest.group(2)} {latest.group(3)}"

        if self.current is None:
            return None

        current_meta = self.volumes.get(self.current)
        current_total = None
        role = "disk"
        if current_meta is not None:
            role = str(current_meta.get("role") or "disk")
            if isinstance(current_meta.get("size_bytes"), int):
                current_total = int(current_meta["size_bytes"])

        percent = None
        if current_total and current_total > 0:
            percent = max(
                0.0,
                min(100.0, 100.0 * min(self.current_bytes, current_total) / current_total),
            )

        if current_total:
            detail = f"{human_bytes(self.current_bytes)} / {human_bytes(current_total)}"
        else:
            detail = human_bytes(self.current_bytes)
        if speed:
            detail += f" · {speed}"

        return {
            "percent": percent,
            "role": role,
            "volume": self.current,
            "copied_bytes": self.current_bytes,
            "total_bytes": current_total,
            "description": f"  ↳ {role} · {detail}",
        }


def build_migrate_plan(
    session: Transport,
    cfg: Config,
    vmid: int,
    target_node: str,
    target_storage: str,
) -> dict[str, Any]:
    info = resolve_existing_workspace(session, cfg, vmid, require_network=False)
    check_remote_requirements(session, cfg, str(info["node"]))
    check_remote_requirements(session, cfg, target_node)
    if target_node == info["node"]:
        raise AppError(f"VM {vmid} is already on node {target_node}")
    unused_refs = sorted(
        key
        for key in info["vm_config"]
        if re.fullmatch(r"unused[0-9]+", key)
    )
    if unused_refs:
        details = ", ".join(
            f"{key}={info['vm_config'][key].split(',', 1)[0]}"
            for key in unused_refs
        )
        raise AppError(
            f"VM {vmid} has detached/unused disk references ({details}); "
            "Proxmox migration scans them too. Clean or resolve them before migration."
        )

    target_storage = resolve_homestack_storage(cfg, target_node, target_storage)
    probe = session.run(
        f"pvesh get /nodes/{shlex.quote(target_node)}/status --output-format json",
        check=False,
        timeout=30,
    )
    if probe.returncode != 0:
        raise AppError(f"Target node {target_node!r} is not available in the Proxmox cluster")
    return {
        "command": "migrate",
        "vmid": vmid,
        "name": info["name"],
        "source_node": info["node"],
        "target_node": target_node,
        "target_storage": target_storage,
        "status": info["status"],
        "home_disk": info["home_disk"],
        "home_label": info["home_label"],
        "home_volume": info["home_volume"],
        "volumes": migration_volume_plan(info["vm_config"], cfg),
        "migration_mode": "offline; all local disks including persistent home",
        "snippet_policy": "copy generated cloud-init snippets to target before migration",
        "config": str(cfg.path),
    }


def migrate_workspace(
    session: Transport,
    cfg: Config,
    plan: dict[str, Any],
    *,
    json_mode: bool,
) -> dict[str, Any]:
    vmid = int(plan["vmid"])
    source_node = str(plan["source_node"])
    target_node = str(plan["target_node"])
    target_storage = str(plan["target_storage"])
    was_running = str(plan.get("status")) == "running"
    desired_status = "running" if was_running else "stopped"
    source_was_stopped = False

    progress: Progress | None = None
    overall = None
    transfer_task = None
    task_total = 8 if was_running else 5

    if not json_mode:
        progress = Progress(
            SpinnerColumn(),
            TextColumn("[bold]{task.description}"),
            BarColumn(),
            TaskProgressColumn(),
            TimeElapsedColumn(),
            console=console,
            refresh_per_second=2,
        )
        progress.start()
        overall = progress.add_task("Migrate workspace", total=task_total)

    def start_step(description: str) -> None:
        if progress is not None and overall is not None:
            progress.update(overall, description=description)

    def finish_step() -> None:
        if progress is not None and overall is not None:
            progress.update(overall, advance=1)

    try:
        start_step("Copy Cloud-Init snippets")
        copied = sync_snippets_to_node(
            session,
            cfg,
            source_node,
            target_node,
            str(plan["name"]),
        )
        finish_step()

        if was_running:
            start_step("Shutdown workspace")
            shutdown_vm_on_node(session, cfg, source_node, vmid)
            source_was_stopped = True
            finish_step()

        command = shlex.join(
            [
                "qm",
                "migrate",
                str(vmid),
                target_node,
                "--with-local-disks",
                "--targetstorage",
                target_storage,
            ]
        )

        start_step("Migrate local disks")
        if progress is None or overall is None:
            node_run(session, cfg, source_node, command, timeout=7200)
        else:
            tracker = MigrationProgressTracker(
                [item for item in plan.get("volumes", []) if isinstance(item, dict)]
            )
            transfer_task = progress.add_task("  ↳ waiting for disk copy", total=100)
            current_volume: str | None = None

            def update_migration(output: str) -> None:
                nonlocal current_volume
                snapshot = tracker.update(output)
                if snapshot is None:
                    return

                volume = str(snapshot.get("volume") or "")
                percent = snapshot.get("percent")
                if volume != current_volume:
                    current_volume = volume
                    progress.reset(
                        transfer_task,
                        completed=0,
                        total=100,
                        description=str(snapshot["description"]),
                    )

                kwargs: dict[str, Any] = {
                    "description": str(snapshot["description"]),
                }
                if percent is not None:
                    kwargs["completed"] = float(percent)
                progress.update(transfer_task, **kwargs)

            session.run_with_progress(
                node_shell_command(cfg, source_node, command),
                update_migration,
                timeout=7200,
                poll_interval=0.5,
            )
            progress.update(transfer_task, completed=100)
            progress.remove_task(transfer_task)
            transfer_task = None
        finish_step()

        start_step("Verify target configuration")
        resource = cluster_vm_resource(session, vmid)
        actual_node = str(resource.get("node") or "") if resource else ""
        if actual_node != target_node:
            raise AppError(
                f"Migration command completed but VM {vmid} is on {actual_node!r}, "
                f"expected {target_node!r}"
            )

        target_cfg = qm_config_on_node(session, cfg, target_node, vmid)
        if disk_option(target_cfg.get(cfg.home_disk, ""), "serial") != plan["home_label"]:
            raise AppError(
                f"Persistent home identity check failed after migration to {target_node}"
            )
        finish_step()

        start_step("Update Cloud-Init")
        node_run(session, cfg, target_node, f"qm cloudinit update {vmid}", timeout=120)
        finish_step()

        if was_running:
            start_step("Start workspace")
            node_run(session, cfg, target_node, f"qm start {vmid}", timeout=300)
            finish_step()

            start_step("Verify guest and persistent home")
            wait_for_qga_on_node(session, cfg, target_node, vmid, timeout=180)
            mounted = guest_out_on_node(
                session,
                cfg,
                target_node,
                vmid,
                f"findmnt -n -o SOURCE,FSTYPE,TARGET /home/{cfg.user_name}",
            )
            mounted_label = guest_out_on_node(
                session,
                cfg,
                target_node,
                vmid,
                f"findmnt -n -o SOURCE /home/{cfg.user_name} | "
                "xargs -r blkid -s LABEL -o value",
            )
            if "ext4" not in mounted or mounted_label != plan["home_label"]:
                raise AppError(
                    f"Migration completed but persistent home verification failed on {target_node}: "
                    f"mount={mounted!r}, label={mounted_label!r}"
                )
            finish_step()

        start_step("Verify final power state")
        status = qm_status_on_node(session, cfg, target_node, vmid)
        if status != desired_status:
            raise AppError(
                f"Migration completed but VM {vmid} power state is {status!r}; "
                f"expected {desired_status!r} to preserve its pre-migration state"
            )
        finish_step()

        result = {
            "ok": True,
            **plan,
            "status": status,
            "snippets_copied": copied,
            "home_preserved": True,
            "power_state_preserved": True,
        }
    except Exception as exc:
        recovery_note = ""
        if was_running and source_was_stopped:
            try:
                resource = cluster_vm_resource(session, vmid)
                current_node = str(resource.get("node") or "") if resource else ""
                current_status = str(resource.get("status") or "") if resource else ""
                if current_node == source_node and current_status != "running":
                    restart = node_run(
                        session,
                        cfg,
                        source_node,
                        f"qm start {vmid}",
                        check=False,
                        timeout=300,
                    )
                    if restart.returncode == 0:
                        recovery_note = (
                            f"\nRecovery: VM {vmid} was restarted automatically on "
                            f"{source_node}."
                        )
                    else:
                        recovery_note = (
                            f"\nRecovery: VM {vmid} is still on {source_node}, but "
                            f"automatic restart failed: {restart.output.strip() or restart.returncode}"
                        )
            except Exception as recovery_exc:
                recovery_note = (
                    f"\nRecovery check failed for VM {vmid} on {source_node}: "
                    f"{recovery_exc}"
                )
        if recovery_note:
            raise AppError(f"{exc}{recovery_note}") from exc
        raise
    finally:
        if progress is not None:
            if transfer_task is not None:
                progress.remove_task(transfer_task)
            progress.stop()

    if not json_mode:
        sections = [
            [
                ("VMID", str(vmid)),
                ("Node", f"[green]{target_node}[/green]"),
                ("Status", ui_vm_status(result.get("status"))),
            ],
            [
                ("Persistent home", f'[green]✓[/green] {plan["home_label"]}'),
                ("Power state", f'[green]✓ preserved[/green] {result["status"]}'),
                ("Target storage", target_storage),
                ("Snippets", f"[green]✓[/green] {len(result["snippets_copied"])} copied"),
            ],
        ]
        show_kv_panel("WORKSPACE MIGRATED", sections)
    return result
