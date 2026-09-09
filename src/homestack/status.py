"""Status support for HomeStack."""

from __future__ import annotations

from typing import Any, Callable
import re
import shlex

from .config import Config
from .guest import derive_ip, guest_out_on_node, parse_disk_size_gb, parse_ipconfig0, workspace_home_usage
from .models import AppError, GOLD_TAG, WORKSPACE_TAG, integer_value, validate_name
from .proxmox import attached_disk_volumes, cluster_node_statuses, cluster_vm_resource, disk_option, disk_storage, has_tag, home_label, homestack_storage_ids_for_node, homestack_storage_layout_name, node_run, orphaned_homestack_volumes, parse_tags, qm_config_on_node, qm_status_on_node, require_workspace_tag, storage_capacity
from .transports.base import Transport

DESTROY_WARN_PERCENT = 10.0


DESTROY_DANGER_PERCENT = 50.0


def expected_ip(cfg: Config, vmid: int) -> str | None:
    return f"{cfg.network_prefix}.{vmid}" if 2 <= vmid <= 254 else None


def homestack_storage_capacities(
    session: Transport,
    cfg: Config,
    *,
    progress: Callable[[str, float], None] | None = None,
    node_statuses: list[dict[str, Any]] | None = None,
) -> tuple[list[dict[str, Any]], list[str]]:
    entries: list[dict[str, Any]] = []
    warnings: list[str] = []
    statuses = node_statuses if node_statuses is not None else cluster_node_statuses(session)
    targets: list[tuple[str, str, str, str, bool]] = []

    for node_info in statuses:
        node = str(node_info["node"])
        layout = homestack_storage_layout_name(node)
        if layout is None:
            continue
        for storage in homestack_storage_ids_for_node(cfg, node):
            targets.append(
                (
                    layout,
                    node,
                    storage,
                    str(node_info.get("status") or "unknown"),
                    bool(node_info.get("online")),
                )
            )

    target_count = max(1, len(targets))
    for index, (layout, node, storage, node_status, node_online) in enumerate(targets):
        if progress is not None:
            progress(
                f"Read {layout} capacity: {storage} on {node}",
                index / target_count,
            )
        if not node_online:
            capacity = storage_capacity(storage, {})
            ok = False
        else:
            try:
                data = session.run_json_value(
                    "pvesh get "
                    f"/nodes/{shlex.quote(node)}/storage/{shlex.quote(storage)}/status "
                    "--output-format json",
                    timeout=30,
                )
            except AppError as exc:
                warnings.append(
                    f"HomeStack storage capacity is unavailable for {layout} "
                    f"({storage} on {node}): {exc}"
                )
                capacity = storage_capacity(storage, {})
                ok = False
            else:
                if not isinstance(data, dict):
                    warnings.append(
                        f"HomeStack storage capacity for {layout} "
                        f"({storage} on {node}) did not return a JSON object"
                    )
                    capacity = storage_capacity(storage, {})
                    ok = False
                else:
                    capacity = storage_capacity(storage, data)
                    ok = True

        entries.append(
            {
                "layout": layout,
                "node": node,
                "storage": storage,
                "node_status": node_status,
                "ok": ok,
                **capacity,
            }
        )

    if progress is not None:
        progress("HomeStack storage capacities complete", 1.0)
    return entries, warnings


def occupancy_level(percent: float | None) -> int:
    if percent is None:
        return 2
    if percent > DESTROY_DANGER_PERCENT:
        return 2
    if percent > DESTROY_WARN_PERCENT:
        return 1
    return 0


def occupancy_risk(percent: float | None) -> str:
    return ("low", "warning", "danger")[occupancy_level(percent)]


def homestack_status_resources(
    resources: list[dict[str, Any]],
    cfg: Config,
) -> list[dict[str, Any]]:
    return [
        item
        for item in resources
        if int(item["vmid"]) == cfg.gold_vmid
        or has_tag(item.get("tags"), WORKSPACE_TAG)
    ]


def global_status(
    session: Transport,
    cfg: Config,
    *,
    progress: Callable[[str, float], None] | None = None,
) -> dict[str, Any]:
    if progress is not None:
        progress("Load cluster inventory", 0.0)
    inventory = session.run_json_value(
        "pvesh get /cluster/resources --type vm --output-format json",
        timeout=30,
    )
    if not isinstance(inventory, list):
        raise AppError("Proxmox VM inventory did not return a JSON array")
    node_statuses = cluster_node_statuses(session)
    node_status_by_name = {
        str(item["node"]): item for item in node_statuses
    }

    resources: list[dict[str, Any]] = []
    for item in inventory:
        if not isinstance(item, dict) or item.get("type") != "qemu":
            continue
        vmid = integer_value(item.get("vmid"))
        if vmid is None:
            continue
        resources.append({**item, "vmid": vmid})
    resources.sort(key=lambda item: item["vmid"])

    warnings: list[str] = []
    vm_configs: dict[int, dict[str, str]] = {}
    attached_volumes: set[str] = set()
    managed_resources = homestack_status_resources(resources, cfg)
    managed_count = max(1, len(managed_resources))
    for resource_index, item in enumerate(managed_resources):
        vmid = int(item["vmid"])
        node = str(item.get("node") or cfg.node)
        if progress is not None:
            fraction = 0.05 + 0.20 * (resource_index / managed_count)
            progress(f"Inspect HomeStack VM {vmid} on {node}", fraction)
        node_info = node_status_by_name.get(node)
        if node_info is None or not node_info.get("online"):
            vm_configs[vmid] = {}
            continue
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        vm_configs[vmid] = vm_cfg
        attached_volumes.update(attached_disk_volumes(vm_cfg))
    if progress is not None:
        progress("Inspect workspace homes", 0.25)

    gold_resource = next((x for x in resources if x["vmid"] == cfg.gold_vmid), None)
    gold: dict[str, Any] | None = None
    if gold_resource is None:
        warnings.append(f"Configured Gold VM {cfg.gold_vmid} does not exist.")
    else:
        gold_node = str(gold_resource.get("node") or cfg.node)
        gold_cfg = vm_configs[cfg.gold_vmid]
        gold_tags = parse_tags(gold_cfg.get("tags") or gold_resource.get("tags"))
        role_tag_ok = GOLD_TAG in gold_tags
        warning = None
        if not role_tag_ok:
            warning = f"Configured Gold VM {cfg.gold_vmid} is missing required tag {GOLD_TAG!r}."
            warnings.append(warning)
        gold = {
            "vmid": cfg.gold_vmid,
            "role": "GOLD",
            "name": gold_resource.get("name") or gold_cfg.get("name") or None,
            "status": gold_resource.get("status") or "unknown",
            "node": gold_node,
            "node_status": str(
                node_status_by_name.get(gold_node, {}).get("status") or "unknown"
            ),
            "ip": expected_ip(cfg, cfg.gold_vmid),
            "root_storage": disk_storage(gold_cfg.get(cfg.root_disk)),
            "home": None,
            "role_tag_ok": role_tag_ok,
            "warning": warning,
        }

    workspaces: list[dict[str, Any]] = []
    total_home_bytes = 0
    known_used_bytes = 0
    known_free_bytes = 0
    known_usage_count = 0
    missing_count = 0
    workspace_resources = [
        item
        for item in managed_resources
        if int(item["vmid"]) != cfg.gold_vmid
    ]
    workspace_count_for_progress = max(1, len(workspace_resources))

    for workspace_index, item in enumerate(workspace_resources):
        vmid = int(item["vmid"])
        node = str(item.get("node") or "")
        if progress is not None:
            fraction = 0.25 + 0.20 * (workspace_index / workspace_count_for_progress)
            progress(f"Inspect home for VM {vmid}", fraction)
        vm_cfg = vm_configs[vmid]
        node_info = node_status_by_name.get(node)
        node_online = bool(node_info and node_info.get("online"))
        tags = parse_tags(vm_cfg.get("tags") or item.get("tags"))
        warning_parts: list[str] = []
        if GOLD_TAG in tags:
            warning_parts.append(f"forbidden tag {GOLD_TAG!r}")
        home_cfg = vm_cfg.get(cfg.home_disk, "")
        home: dict[str, Any] | None = None
        if not node_online:
            home = None
        elif not home_cfg:
            warning_parts.append(f"missing {cfg.home_disk}")
            missing_count += 1
        else:
            expected = home_label(vmid)
            serial = disk_option(home_cfg, "serial")
            if serial != expected:
                warning_parts.append(f"{cfg.home_disk} serial {serial!r} != {expected!r}")
            usage = workspace_home_usage(
                session,
                cfg,
                node,
                vmid,
                str(item.get("status") or "unknown"),
                home_cfg,
            )
            home = usage
            size = usage.get("size_bytes")
            used = usage.get("used_bytes")
            free = usage.get("free_bytes")
            if size is not None:
                total_home_bytes += int(size)
            if used is not None and free is not None:
                known_usage_count += 1
                known_used_bytes += int(used)
                known_free_bytes += int(free)
        warning = "; ".join(warning_parts) if warning_parts else None
        if warning:
            warnings.append(f"Workspace VM {vmid}: {warning}.")
        workspaces.append(
            {
                "vmid": vmid,
                "role": "WS",
                "name": item.get("name") or vm_cfg.get("name") or None,
                "status": item.get("status") or "unknown",
                "node": node,
                "node_status": str(
                    node_info.get("status") if node_info else "unknown"
                ),
                "ip": expected_ip(cfg, vmid),
                "root_storage": disk_storage(vm_cfg.get(cfg.root_disk)),
                "home": home,
                "role_tag_ok": GOLD_TAG not in tags,
                "warning": warning,
            }
        )

    if progress is not None:
        progress("Scan detached HomeStack volumes", 0.45)

    def orphan_progress(description: str, fraction: float) -> None:
        if progress is not None:
            progress(description, 0.45 + 0.50 * max(0.0, min(1.0, fraction)))

    orphaned_volumes, orphan_scan_warnings, orphan_scan_complete = orphaned_homestack_volumes(
        session,
        cfg,
        attached_volumes,
        progress=orphan_progress if progress is not None else None,
        node_statuses=node_statuses,
    )
    if orphaned_volumes:
        warnings.append(
            "Detached HomeStack volumes: "
            + ", ".join(str(item["volid"]) for item in orphaned_volumes)
        )
    warnings.extend(f"HomeStack orphan scan incomplete: {warning}" for warning in orphan_scan_warnings)

    def storage_progress(description: str, fraction: float) -> None:
        if progress is not None:
            progress(description, 0.95 + 0.04 * max(0.0, min(1.0, fraction)))

    storage_layouts, storage_warnings = homestack_storage_capacities(
        session,
        cfg,
        progress=storage_progress if progress is not None else None,
        node_statuses=node_statuses,
    )
    warnings.extend(storage_warnings)

    root_summary = next(
        (
            {
                key: value
                for key, value in item.items()
                if key in {"name", "total_bytes", "used_bytes", "available_bytes", "percent"}
            }
            for item in storage_layouts
            if item.get("node") == cfg.node and item.get("storage") == cfg.root_storage
        ),
        storage_capacity(cfg.root_storage, {}),
    )

    all_usage_known = known_usage_count == len(workspaces) and len(workspaces) > 0
    free_percent = (
        100.0 * known_free_bytes / total_home_bytes
        if all_usage_known and total_home_bytes
        else None
    )
    running = sum(item["status"] == "running" for item in workspaces)
    stopped = sum(item["status"] == "stopped" for item in workspaces)
    if progress is not None:
        progress("Status complete", 1.0)
    return {
        "ok": True,
        "command": "status",
        "scope": "global",
        "gold": gold,
        "workspaces": workspaces,
        "summary": {
            "workspace_count": len(workspaces),
            "running": running,
            "stopped": stopped,
            "root_storage": root_summary,
            "storage_layouts": storage_layouts,
            "nodes": node_statuses,
            "storage_unit": cfg.storage_display_unit,
            "storage_decimals": cfg.storage_display_decimals,
            "homes": {
                "used_bytes": known_used_bytes if known_usage_count else None,
                "quota_bytes": total_home_bytes,
                "free_bytes": known_free_bytes if all_usage_known else None,
                "free_percent": free_percent,
                "physical_available_bytes": None,
                "missing_count": missing_count,
            },
            "orphaned_volumes": orphaned_volumes,
            "orphan_scan_complete": orphan_scan_complete,
        },
        "warnings": warnings,
        "config": str(cfg.path),
        "execution": session.execution_info(),
    }


def vm_volume_inventory(vm_cfg: dict[str, str], cfg: Config) -> list[dict[str, Any]]:
    """Return configured Proxmox-backed VM volumes, including unusedN references."""
    slot_re = re.compile(
        r"^(?P<prefix>ide|sata|scsi|virtio|efidisk|tpmstate|unused)(?P<index>[0-9]+)$"
    )
    slot_order = {
        "ide": 0,
        "sata": 1,
        "scsi": 2,
        "virtio": 3,
        "efidisk": 4,
        "tpmstate": 5,
        "unused": 6,
    }

    entries: list[dict[str, Any]] = []
    for slot, raw_value in vm_cfg.items():
        match = slot_re.fullmatch(slot)
        if match is None:
            continue

        value = str(raw_value)
        volume = value.split(",", 1)[0].strip()
        if not volume or volume == "none" or ":" not in volume:
            continue

        if slot == cfg.root_disk:
            role = "root"
        elif slot == cfg.home_disk:
            role = "home"
        elif match.group("prefix") == "unused":
            role = "unused"
        elif match.group("prefix") == "efidisk":
            role = "efi"
        elif match.group("prefix") == "tpmstate":
            role = "tpm"
        elif disk_option(value, "media") == "cdrom" and "cloudinit" in volume:
            role = "cloud-init"
        else:
            role = "disk"

        entries.append(
            {
                "slot": slot,
                "role": role,
                "volume": volume,
                "storage": disk_storage(value),
                "size": disk_option(value, "size"),
                "_sort": (
                    slot_order.get(match.group("prefix"), 99),
                    int(match.group("index")),
                ),
            }
        )

    entries.sort(key=lambda item: item["_sort"])
    for item in entries:
        item.pop("_sort", None)
    return entries


def resolve_existing_workspace(
    session: Transport,
    cfg: Config,
    vmid: int,
    *,
    require_network: bool,
) -> dict[str, Any]:
    if vmid == cfg.gold_vmid:
        raise AppError(f"Refusing lifecycle operation on Gold VM {cfg.gold_vmid}")

    resource = cluster_vm_resource(session, vmid)
    if resource is None:
        raise AppError(f"VMID {vmid} does not exist")
    node = str(resource.get("node") or "")
    if not node:
        raise AppError(f"VM {vmid} has no node in cluster inventory")
    status = str(resource.get("status") or qm_status_on_node(session, cfg, node, vmid))

    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    require_workspace_tag(vmid, vm_cfg)
    name = vm_cfg.get("name", "").strip()
    if not name:
        raise AppError(f"VM {vmid} has no name")
    validate_name(name)

    root_cfg = vm_cfg.get(cfg.root_disk, "")
    if not root_cfg:
        raise AppError(f"VM {vmid} has no disposable root disk {cfg.root_disk}")

    home_cfg = vm_cfg.get(cfg.home_disk, "")
    if not home_cfg:
        raise AppError(f"VM {vmid} has no persistent home disk {cfg.home_disk}")
    expected_label = home_label(vmid)
    root_volume = root_cfg.split(",", 1)[0].strip()
    home_volume = home_cfg.split(",", 1)[0].strip()
    if root_volume == home_volume:
        raise AppError(
            f"VM {vmid} root disk {cfg.root_disk} and persistent home {cfg.home_disk} "
            f"reference the same volume {root_volume!r}"
        )
    serial = disk_option(home_cfg, "serial")
    if serial != expected_label:
        raise AppError(
            f"VM {vmid} {cfg.home_disk} serial is {serial!r}, expected {expected_label!r}"
        )
    if vm_cfg.get("virtiofs0"):
        raise AppError(
            f"VM {vmid} still has legacy virtiofs0 configured; refusing new HomeStack lifecycle"
        )

    network: dict[str, Any] = {}
    if require_network:
        network = parse_ipconfig0(vm_cfg.get("ipconfig0", ""))

    usage = workspace_home_usage(session, cfg, node, vmid, status, home_cfg)
    return {
        "vmid": vmid,
        "name": name,
        "node": node,
        "status": status,
        "vm_config": vm_cfg,
        "root_disk": cfg.root_disk,
        "root_disk_config": root_cfg,
        "root_volume": root_volume,
        "root_storage": disk_storage(root_cfg),
        "home_disk": cfg.home_disk,
        "home_disk_config": home_cfg,
        "home_volume": home_volume,
        "home_storage": disk_storage(home_cfg),
        "home_label": expected_label,
        "home_usage": usage,
        **network,
    }


def workspace_status(session: Transport, cfg: Config, vmid: int) -> dict[str, Any]:
    resource = cluster_vm_resource(session, vmid)
    if resource is None:
        return {"ok": False, "command": "status", "vmid": vmid, "status": "absent"}

    node = str(resource.get("node") or "")
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    name = vm_cfg.get("name", "")
    tags = parse_tags(vm_cfg.get("tags"))
    if vmid == cfg.gold_vmid:
        role = "GOLD"
        role_tag_ok = GOLD_TAG in tags
        role_warning = None if role_tag_ok else f"Configured Gold VM is missing required tag {GOLD_TAG!r}."
    elif WORKSPACE_TAG in tags:
        role = "WS"
        role_tag_ok = GOLD_TAG not in tags
        role_warning = None if role_tag_ok else f"Workspace also has forbidden Gold tag {GOLD_TAG!r}."
    else:
        role = None
        role_tag_ok = False
        role_warning = f"Required workspace tag {WORKSPACE_TAG!r} is missing."

    ip = derive_ip(cfg, vmid) if 2 <= vmid <= 254 else None
    status = str(resource.get("status") or qm_status_on_node(session, cfg, node, vmid))
    home_cfg = vm_cfg.get(cfg.home_disk, "")
    expected_home_label = home_label(vmid) if role == "WS" else None
    home_serial = disk_option(home_cfg, "serial") if home_cfg else None
    home_storage = disk_storage(home_cfg) if home_cfg else None
    home_size_gb = parse_disk_size_gb(home_cfg) if home_cfg else None

    qga = False
    actual_hostname = None
    actual_ip = None
    home_mount = None
    mounted_home_label = None
    ubuntu_user = None

    if status == "running":
        qga = node_run(session, cfg, node, f"qm guest cmd {vmid} ping", check=False).returncode == 0
        if qga:
            actual_hostname = guest_out_on_node(session, cfg, node, vmid, "hostname", check=False) or None
            actual_ip = guest_out_on_node(
                session,
                cfg,
                node,
                vmid,
                "ip -4 -o addr show dev eth0 2>/dev/null | awk '{print $4}' | head -n1",
                check=False,
            ) or None
            home_mount = guest_out_on_node(
                session,
                cfg,
                node,
                vmid,
                f"findmnt -n -o SOURCE,FSTYPE,TARGET /home/{cfg.user_name} 2>/dev/null || true",
                check=False,
            ) or None
            mounted_home_label = guest_out_on_node(
                session,
                cfg,
                node,
                vmid,
                f"findmnt -n -o SOURCE /home/{cfg.user_name} 2>/dev/null | "
                "xargs -r blkid -s LABEL -o value",
                check=False,
            ) or None
            ubuntu_user = (
                guest_out_on_node(
                    session,
                    cfg,
                    node,
                    vmid,
                    "if getent passwd ubuntu >/dev/null 2>&1; then echo true; else echo false; fi",
                    check=False,
                )
                == "true"
            )

    return {
        "ok": True,
        "command": "status",
        "vmid": vmid,
        "name": name or None,
        "role": role,
        "tags": sorted(tags),
        "role_tag_ok": role_tag_ok,
        "role_warning": role_warning,
        "status": status,
        "node": node,
        "configured_ip": ip,
        "actual_hostname": actual_hostname,
        "actual_ip": actual_ip,
        "qga": qga,
        "root_storage": disk_storage(vm_cfg.get(cfg.root_disk)),
        "volumes": vm_volume_inventory(vm_cfg, cfg),
        "home_disk": cfg.home_disk if home_cfg else None,
        "home_storage": home_storage,
        "home_size_gb": home_size_gb,
        "home_label": expected_home_label,
        "home_serial": home_serial,
        "home_identity_ok": bool(expected_home_label and home_serial == expected_home_label),
        "home_mount": home_mount,
        "mounted_home_label": mounted_home_label,
        "home_mount_ok": bool(
            expected_home_label
            and mounted_home_label == expected_home_label
            and home_mount
            and "ext4" in home_mount
        ),
        "ubuntu_user_present": ubuntu_user,
        "config": str(cfg.path),
        "execution": session.execution_info(),
        "ssh": {
            "root": f"ssh root@{ip}" if ip else None,
            "user": f"ssh {cfg.user_name}@{ip}" if ip else None,
        },
    }


def transport_status(session: Transport, cfg: Config) -> dict[str, Any]:
    node_status = session.run_json(
        f"pvesh get /nodes/{shlex.quote(cfg.node)}/status --output-format json",
        timeout=15,
    )
    return {
        "ok": True,
        "command": "transport",
        "transport": session.execution_info(),
        "pve": {
            "node": cfg.node,
            "version": node_status.get("pveversion"),
            "kernel": node_status.get("kversion"),
            "uptime": node_status.get("uptime"),
        },
    }


def free_percent(total: Any, available: Any) -> float | None:
    total_value = integer_value(total)
    available_value = integer_value(available)
    if total_value is None or available_value is None or total_value <= 0:
        return None
    return max(0.0, min(100.0, 100.0 * available_value / total_value))
