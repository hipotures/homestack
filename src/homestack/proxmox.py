"""Proxmox support for HomeStack."""

from __future__ import annotations

from typing import Any, Callable
import re
import shlex

from .config import Config
from .models import AppError, GOLD_TAG, HOME_LABEL_PREFIX, RemoteResult, WORKSPACE_TAG, integer_value
from .transports.base import Transport

HOMESTACK_VOLUME_RE = re.compile(
    r"^vm-(?P<vmid>[0-9]+)-hs-(?P<role>root|home)-(?P<name>[A-Za-z0-9_.-]+)(?:\\.[A-Za-z0-9]+)?$"
)


def command_exists_on_node(
    session: Transport, cfg: Config, node: str, name: str
) -> bool:
    return node_run(
        session,
        cfg,
        node,
        f"command -v {shlex.quote(name)} >/dev/null 2>&1",
        check=False,
    ).returncode == 0


def qm_exists_on_node(session: Transport, cfg: Config, node: str, vmid: int) -> bool:
    return qm_status_on_node(session, cfg, node, vmid) != "absent"


def parse_tags(value: Any) -> frozenset[str]:
    if value is None:
        return frozenset()
    return frozenset(token.strip() for token in str(value).split(";") if token.strip())


def has_tag(value: Any, tag: str) -> bool:
    return tag in parse_tags(value)


def require_workspace_tag(vmid: int, vm_cfg: dict[str, str]) -> None:
    if not has_tag(vm_cfg.get("tags"), WORKSPACE_TAG):
        raise AppError(
            f"VM {vmid} is not a HomeStack workspace: required tag {WORKSPACE_TAG!r} is missing."
        )


def require_gold_tag(vmid: int, vm_cfg: dict[str, str]) -> None:
    if not has_tag(vm_cfg.get("tags"), GOLD_TAG):
        raise AppError(
            f"Configured Gold VM {vmid} is missing required tag {GOLD_TAG!r}."
        )


def verify_workspace_role_tags(vmid: int, vm_cfg: dict[str, str]) -> None:
    tags = parse_tags(vm_cfg.get("tags"))
    if WORKSPACE_TAG not in tags:
        raise AppError(f"VM {vmid} tag verification failed: {WORKSPACE_TAG!r} is missing")
    if GOLD_TAG in tags:
        raise AppError(f"VM {vmid} tag verification failed: inherited {GOLD_TAG!r}")
    if tags != frozenset({WORKSPACE_TAG}):
        raise AppError(
            f"VM {vmid} tag verification failed: expected exactly {WORKSPACE_TAG!r}, "
            f"got {sorted(tags)!r}"
        )


def set_workspace_role_tags(
    session: Transport, cfg: Config, node: str, vmid: int
) -> None:
    node_run(
        session,
        cfg,
        node,
        shlex.join(["qm", "set", str(vmid), "--tags", WORKSPACE_TAG]),
    )
    verify_workspace_role_tags(vmid, qm_config_on_node(session, cfg, node, vmid))


def disk_storage(disk_config: str | None) -> str | None:
    if not disk_config:
        return None
    volume = disk_config.split(",", 1)[0].strip()
    if ":" not in volume:
        return None
    storage, _ = volume.split(":", 1)
    return storage or None


def attached_disk_volumes(vm_cfg: dict[str, str]) -> set[str]:
    volumes: set[str] = set()
    for key, value in vm_cfg.items():
        if not re.fullmatch(r"(?:ide|sata|scsi|virtio)[0-9]+|efidisk[0-9]+|tpmstate[0-9]+", key):
            continue
        volume = str(value).split(",", 1)[0].strip()
        if ":" in volume and volume != "none":
            volumes.add(volume)
    return volumes


def homestack_storage_layout_name(node: str) -> str | None:
    match = re.search(r"([1-9][0-9]*)$", node)
    if match is None:
        return None
    return f"homestack-storage-{match.group(1)}"


def homestack_storage_ids_for_node(cfg: Config, node: str) -> tuple[str, ...]:
    layout = homestack_storage_layout_name(node)
    if layout is None:
        return ()
    return cfg.storage_layouts.get(layout, ())


def resolve_homestack_storage(
    cfg: Config,
    node: str,
    requested: str | None = None,
) -> str:
    storages = homestack_storage_ids_for_node(cfg, node)
    if not storages:
        layout = homestack_storage_layout_name(node)
        raise AppError(
            f"No HomeStack storages are assigned to node {node!r}"
            + (f" via {layout}" if layout else "")
        )
    if requested is None:
        return storages[0]
    storage = requested.strip()
    if storage not in storages:
        raise AppError(
            f"Storage {storage!r} is not assigned to node {node!r}; "
            f"allowed HomeStack storages: {', '.join(storages)}"
        )
    return storage


def homestack_volume_identity(volid: str) -> dict[str, Any] | None:
    if ":" not in volid:
        return None
    _, volume_path = volid.split(":", 1)
    basename = volume_path.rsplit("/", 1)[-1]
    for extension in (".raw", ".qcow2", ".vmdk"):
        if basename.endswith(extension):
            basename = basename[: -len(extension)]
            break
    match = HOMESTACK_VOLUME_RE.fullmatch(basename)
    if not match:
        return None
    return {
        "volid": volid,
        "vmid": int(match.group("vmid")),
        "role": match.group("role"),
        "name": match.group("name"),
    }


def orphaned_homestack_volumes(
    session: Transport,
    cfg: Config,
    attached_volumes: set[str],
    *,
    progress: Callable[[str, float], None] | None = None,
    node_statuses: list[dict[str, Any]] | None = None,
) -> tuple[list[dict[str, Any]], list[str], bool]:
    found: dict[str, dict[str, Any]] = {}
    scan_warnings: list[str] = []
    scan_complete = True

    statuses = node_statuses if node_statuses is not None else cluster_node_statuses(session)
    node_count = max(1, len(statuses))
    for node_index, node_info in enumerate(statuses):
        node = str(node_info["node"])
        if progress is not None:
            progress(f"Scan storage inventory on {node}", node_index / node_count)
        assigned_storages = set(homestack_storage_ids_for_node(cfg, node))
        if not node_info.get("online"):
            if assigned_storages:
                scan_complete = False
            continue
        try:
            storages = session.run_json_value(
                f"pvesh get /nodes/{shlex.quote(node)}/storage --output-format json",
                timeout=30,
            )
        except AppError as exc:
            scan_warnings.append(f"Cannot inspect storages on {node}: {exc}")
            continue
        if not isinstance(storages, list):
            scan_warnings.append(f"Storage inventory on {node} did not return a JSON array")
            continue

        if not assigned_storages:
            continue

        image_storages: list[str] = []
        seen_storages: set[str] = set()
        for storage_info in storages:
            if not isinstance(storage_info, dict):
                continue
            storage = str(storage_info.get("storage") or "").strip()
            content = {
                token.strip()
                for token in str(storage_info.get("content") or "").split(",")
                if token.strip()
            }
            if not storage or storage not in assigned_storages or "images" not in content:
                continue
            if integer_value(storage_info.get("enabled")) == 0:
                continue
            if integer_value(storage_info.get("active")) == 0:
                continue
            image_storages.append(storage)
            seen_storages.add(storage)

        missing_storages = sorted(assigned_storages - seen_storages)
        for storage in missing_storages:
            scan_warnings.append(
                f"Assigned HomeStack storage {storage!r} is unavailable for image scanning on {node}"
            )

        storage_count = max(1, len(image_storages))
        for storage_index, storage in enumerate(image_storages):
            if progress is not None:
                fraction = (node_index + storage_index / storage_count) / node_count
                progress(f"Scan {storage} on {node}", fraction)
            try:
                entries = session.run_json_value(
                    "pvesh get "
                    f"/nodes/{shlex.quote(node)}/storage/{shlex.quote(storage)}/content "
                    "--content images --output-format json",
                    timeout=60,
                )
            except AppError as exc:
                scan_warnings.append(
                    f"Cannot inspect image volumes on {storage} ({node}): {exc}"
                )
                continue
            if not isinstance(entries, list):
                scan_warnings.append(
                    f"Storage content on {storage} ({node}) did not return a JSON array"
                )
                continue

            for entry in entries:
                if not isinstance(entry, dict):
                    continue
                volid = str(entry.get("volid") or "").strip()
                identity = homestack_volume_identity(volid)
                if identity is None or volid in attached_volumes:
                    continue
                size = integer_value(entry.get("size"))
                identity["size_bytes"] = size
                identity["storage"] = storage
                identity["node"] = node
                found.setdefault(volid, identity)

    if progress is not None:
        progress("Detached-volume scan complete", 1.0)
    return (
        sorted(found.values(), key=lambda item: str(item["volid"])),
        scan_warnings,
        scan_complete and not scan_warnings,
    )


def storage_capacity(name: str, data: dict[str, Any]) -> dict[str, Any]:
    total = integer_value(data.get("total"))
    used = integer_value(data.get("used"))
    available = integer_value(data.get("avail"))
    if available is None:
        available = integer_value(data.get("available"))
    percent = (100.0 * used / total) if total and used is not None else None
    return {
        "name": name,
        "total_bytes": total,
        "used_bytes": used,
        "available_bytes": available,
        "percent": percent,
    }


def shutdown_vm_on_node(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    timeout: int = 90,
) -> None:
    status = qm_status_on_node(session, cfg, node, vmid)
    if status == "stopped":
        return
    if status != "running":
        raise AppError(f"VM {vmid} has unexpected status {status!r}; refusing shutdown")
    result = node_run(
        session,
        cfg,
        node,
        f"qm shutdown {vmid} --timeout {timeout}",
        check=False,
        timeout=timeout + 30,
    )
    final_status = qm_status_on_node(session, cfg, node, vmid)
    if result.returncode != 0 or final_status != "stopped":
        raise AppError(
            f"VM {vmid} did not shut down cleanly on {node}. Current status: {final_status}. "
            "HomeStack will not force-stop it automatically."
        )


def check_remote_requirements(session: Transport, cfg: Config, node: str) -> None:
    required = ("qm", "pvesh", "pvesm", "perl", "base64", "ssh", "scp", "timeout")
    missing = [
        name for name in required if not command_exists_on_node(session, cfg, node, name)
    ]
    if missing:
        raise AppError("Missing required commands on Proxmox node: " + ", ".join(missing))


def home_label(vmid: int) -> str:
    label = f"{HOME_LABEL_PREFIX}{vmid}"
    if len(label.encode("utf-8")) > 16:
        raise AppError(f"Home filesystem label is too long for ext4: {label}")
    return label


def parse_home_size(size: str) -> tuple[str, int]:
    text = size.strip().upper()
    match = re.fullmatch(r"([1-9][0-9]*)([GT])", text)
    if not match:
        raise AppError("Home size must be an integer number of GiB or TiB, for example 20G or 1T")
    value = int(match.group(1))
    gib = value if match.group(2) == "G" else value * 1024
    return text, gib


def root_volume_name(vmid: int) -> str:
    return f"vm-{vmid}-hs-root-default"


def home_volume_name(vmid: int, user_name: str) -> str:
    if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]{0,31}", user_name):
        raise AppError(f"Cannot derive persistent home volume name from user {user_name!r}")
    return f"vm-{vmid}-hs-home-{user_name}"


def named_volume_id(storage: str, volume_name: str) -> str:
    return f"{storage}:{volume_name}"


def allocate_named_raw_volume(
    session: Transport,
    cfg: Config,
    node: str,
    storage: str,
    vmid: int,
    volume_name: str,
    size: str,
) -> str:
    result = node_run(
        session,
        cfg,
        node,
        shlex.join(
            [
                "pvesm",
                "alloc",
                storage,
                str(vmid),
                volume_name,
                size,
                "--format",
                "raw",
            ]
        ),
        timeout=600,
    )
    expected = named_volume_id(storage, volume_name)
    matches = re.findall(r"'([^']+:[^']+)'", result.output)
    return matches[-1] if matches else expected


def pve_rename_volume(
    session: Transport,
    cfg: Config,
    node: str,
    source_volume: str,
    vmid: int,
    target_volume_name: str,
) -> str:
    perl = (
        "my $cfg=PVE::Storage::config(); "
        "my $new=PVE::Storage::rename_volume("
        "$cfg,$ARGV[0],int($ARGV[1]),$ARGV[2]); "
        'print "$new\\n";'
    )
    result = node_run(
        session,
        cfg,
        node,
        shlex.join(
            [
                "perl",
                "-MPVE::Storage",
                "-e",
                perl,
                source_volume,
                str(vmid),
                target_volume_name,
            ]
        ),
        timeout=600,
    )
    lines = [line.strip() for line in result.output.splitlines() if ":" in line]
    if not lines:
        raise AppError(
            f"Proxmox storage layer renamed {source_volume!r} but did not return a volume ID"
        )
    return lines[-1]


def replace_disk_volume(disk_config: str, volume: str) -> str:
    parts = disk_config.split(",")
    if not parts or ":" not in parts[0]:
        raise AppError(f"Cannot replace volume in disk configuration: {disk_config!r}")
    parts[0] = volume
    return ",".join(parts)


def cleanup_renamed_volume_unused_refs(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    source_volume: str,
) -> list[str]:
    """Remove only stale unusedN references created while renaming one VM volume."""
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    matching = sorted(
        key
        for key, value in vm_cfg.items()
        if re.fullmatch(r"unused[0-9]+", key)
        and str(value).split(",", 1)[0].strip() == source_volume
    )
    if not matching:
        return []

    if ":" not in source_volume:
        raise AppError(f"Cannot verify renamed source volume {source_volume!r}")
    storage, _ = source_volume.split(":", 1)
    inventory = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/storage/{shlex.quote(storage)}/content "
        "--content images --output-format json",
        timeout=30,
    )
    if not isinstance(inventory, list):
        raise AppError(
            f"Storage inventory for {storage!r} did not return a JSON array while "
            f"checking stale references for VM {vmid}"
        )

    if any(
        isinstance(item, dict) and str(item.get("volid") or "") == source_volume
        for item in inventory
    ):
        raise AppError(
            f"Refusing to remove VM {vmid} stale-reference candidate for "
            f"{source_volume!r}: the source volume still exists"
        )

    for key in matching:
        node_run(
            session,
            cfg,
            node,
            shlex.join(["qm", "set", str(vmid), "--delete", key]),
            timeout=120,
        )

    remaining_cfg = qm_config_on_node(session, cfg, node, vmid)
    remaining = [
        key
        for key, value in remaining_cfg.items()
        if re.fullmatch(r"unused[0-9]+", key)
        and str(value).split(",", 1)[0].strip() == source_volume
    ]
    if remaining:
        raise AppError(
            f"VM {vmid} still contains stale references to renamed volume "
            f"{source_volume!r}: {', '.join(sorted(remaining))}"
        )
    return matching


def rename_attached_disk_volume(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    disk: str,
    target_volume_name: str,
) -> str:
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    disk_cfg = vm_cfg.get(disk, "")
    if not disk_cfg:
        raise AppError(f"VM {vmid} has no {disk}")
    source_volume = disk_cfg.split(",", 1)[0].strip()
    source_name = source_volume.rsplit(":", 1)[-1].rsplit("/", 1)[-1]
    if source_name == target_volume_name:
        return source_volume

    new_volume = pve_rename_volume(
        session, cfg, node, source_volume, vmid, target_volume_name
    )
    new_spec = replace_disk_volume(disk_cfg, new_volume)
    try:
        node_run(
            session,
            cfg,
            node,
            shlex.join(["qm", "set", str(vmid), f"--{disk}", new_spec]),
            timeout=600,
        )
    except Exception as exc:
        raise AppError(
            f"Volume was renamed to {new_volume!r}, but VM {vmid} {disk} could not be "
            f"updated. Manual recovery: qm set {vmid} --{disk} {shlex.quote(new_spec)}. "
            f"Original error: {exc}"
        ) from exc

    updated = qm_config_on_node(session, cfg, node, vmid).get(disk, "")
    actual = updated.split(",", 1)[0].strip()
    if actual != new_volume:
        raise AppError(
            f"VM {vmid} {disk} volume naming failed: expected {new_volume!r}, got {actual!r}"
        )
    cleanup_renamed_volume_unused_refs(session, cfg, node, vmid, source_volume)
    return new_volume


def disk_option(disk_config: str, key: str) -> str | None:
    for item in disk_config.split(",")[1:]:
        if "=" not in item:
            continue
        item_key, value = item.split("=", 1)
        if item_key == key:
            return value
    return None


def node_shell_command(cfg: Config, node: str, command: str) -> str:
    if node == cfg.control_node:
        return command
    target = shlex.quote(f"root@{node}")
    return f"ssh -o BatchMode=yes {target} {shlex.quote(command)}"


def node_run(
    session: Transport,
    cfg: Config,
    node: str,
    command: str,
    *,
    check: bool = True,
    timeout: int = 120,
) -> RemoteResult:
    return session.run(node_shell_command(cfg, node, command), check=check, timeout=timeout)


def cluster_vm_resource(session: Transport, vmid: int) -> dict[str, Any] | None:
    for item in cluster_vm_resources(session):
        if integer_value(item.get("vmid")) == vmid and item.get("type") == "qemu":
            return item
    return None


def cluster_vm_resources(session: Transport) -> list[dict[str, Any]]:
    """Return the read-only cluster QEMU inventory with normalized VMIDs."""
    data = session.run_json_value(
        "pvesh get /cluster/resources --type vm --output-format json",
        timeout=30,
    )
    if not isinstance(data, list):
        raise AppError("Proxmox VM inventory did not return a JSON array")
    resources: list[dict[str, Any]] = []
    for item in data:
        if not isinstance(item, dict) or item.get("type") != "qemu":
            continue
        vmid = integer_value(item.get("vmid"))
        if vmid is None:
            continue
        resources.append({**item, "vmid": vmid})
    resources.sort(key=lambda item: int(item["vmid"]))
    return resources


def cluster_storage_definitions(session: Transport) -> list[dict[str, Any]]:
    """Return cluster-wide storage configuration without changing it."""
    data = session.run_json_value("pvesh get /storage --output-format json", timeout=30)
    if not isinstance(data, list):
        raise AppError("Proxmox storage configuration did not return a JSON array")
    return [dict(item) for item in data if isinstance(item, dict) and item.get("storage")]


def node_storage_inventory(session: Transport, node: str) -> list[dict[str, Any]]:
    """Return storage currently reported by one online node."""
    data = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/storage --output-format json",
        timeout=30,
    )
    if not isinstance(data, list):
        raise AppError(f"Proxmox storage inventory on {node} did not return a JSON array")
    return [dict(item) for item in data if isinstance(item, dict) and item.get("storage")]


def node_network_inventory(session: Transport, node: str) -> list[dict[str, Any]]:
    """Return the read-only network configuration reported by one PVE node."""
    data = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/network --output-format json",
        timeout=30,
    )
    if not isinstance(data, list):
        raise AppError(f"Proxmox network inventory on {node} did not return a JSON array")
    return [dict(item) for item in data if isinstance(item, dict) and item.get("iface")]


def node_dns_config(session: Transport, node: str) -> dict[str, Any]:
    """Return the read-only DNS configuration reported by one PVE node."""
    data = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/dns --output-format json",
        timeout=30,
    )
    if not isinstance(data, dict):
        raise AppError(f"Proxmox DNS configuration on {node} did not return a JSON object")
    return dict(data)


def cluster_nodes(session: Transport) -> list[str]:
    return [item["node"] for item in cluster_node_statuses(session)]


def cluster_node_statuses(session: Transport) -> list[dict[str, Any]]:
    """Return cluster nodes with the online state reported by Proxmox."""
    data = session.run_json_value("pvesh get /nodes --output-format json", timeout=30)
    if not isinstance(data, list):
        raise AppError("Proxmox node inventory did not return a JSON array")
    nodes: list[dict[str, Any]] = []
    for item in data:
        if not isinstance(item, dict) or not item.get("node"):
            continue
        state = str(item.get("status") or "unknown").strip().lower()
        nodes.append(
            {
                "node": str(item["node"]),
                "status": state,
                "online": state != "offline",
            }
        )
    nodes.sort(key=lambda item: item["node"])
    if not nodes:
        raise AppError("Proxmox cluster returned no nodes")
    return nodes


def qm_config_on_node(session: Transport, cfg: Config, node: str, vmid: int) -> dict[str, str]:
    data = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/qemu/{vmid}/config --output-format json",
        timeout=30,
    )
    if not isinstance(data, dict):
        raise AppError(f"Proxmox config for VM {vmid} on {node} did not return a JSON object")
    return {
        str(key): "" if value is None else str(value)
        for key, value in data.items()
    }


def qm_status_on_node(session: Transport, cfg: Config, node: str, vmid: int) -> str:
    data = session.run_json_value(
        f"pvesh get /nodes/{shlex.quote(node)}/qemu/{vmid}/status/current --output-format json",
        check=False,
        timeout=30,
    )
    if data is None:
        return "absent"
    if not isinstance(data, dict):
        return "unknown"
    status = str(data.get("status") or "").strip()
    return status or "unknown"


def boot_order_contains_disk(boot_config: str, disk: str) -> bool:
    if not boot_config:
        return False
    fields = dict(
        item.split("=", 1)
        for item in boot_config.split(",")
        if "=" in item
    )
    order = fields.get("order", "")
    return disk in [token for token in order.split(";") if token]
