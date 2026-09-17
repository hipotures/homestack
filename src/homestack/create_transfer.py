"""Cross-node workspace creation helpers.

The transfer path deliberately keeps the archive stream on the control node.  The
source and target nodes never need to trust one another and no archive is staged
on the desktop or on either node.
"""

from __future__ import annotations

import re
import shlex
from typing import Any

from .config import Config
from .models import AppError
from .proxmox import (
    check_remote_requirements,
    cluster_node_statuses,
    command_exists_on_node,
    disk_option,
    node_network_inventory,
    node_storage_inventory,
    resolve_homestack_storage,
)
from .transports.base import Transport

_DISK_SLOT_RE = re.compile(r"(?:ide|sata|scsi|virtio)[0-9]+")
_EFI_SLOT_RE = re.compile(r"efidisk[0-9]+")
_TPM_SLOT_RE = re.compile(r"tpmstate[0-9]+")
_NET_SLOT_RE = re.compile(r"net[0-9]+")


def _configured_volume(value: str | None) -> str:
    return str(value or "").split(",", 1)[0].strip()


def _storage_is_active(item: dict[str, Any]) -> bool:
    for key in ("enabled", "active"):
        value = item.get(key)
        if value is None:
            continue
        if str(value).strip().lower() in {"0", "false", "no", "off"}:
            return False
    return True


def _storage_item(
    inventory: list[dict[str, Any]], storage: str, content: str
) -> dict[str, Any]:
    for item in inventory:
        if str(item.get("storage") or "") != storage:
            continue
        available = {
            token.strip()
            for token in str(item.get("content") or "").split(",")
            if token.strip()
        }
        if not _storage_is_active(item):
            raise AppError(
                f"Target storage {storage!r} is disabled or inactive on node"
            )
        if content not in available:
            raise AppError(
                f"Target storage {storage!r} does not support {content} content"
            )
        return item
    raise AppError(f"Target node does not provide storage {storage!r}")


def _cloud_init_slots(vm_cfg: dict[str, str]) -> list[str]:
    slots: list[str] = []
    for slot, value in vm_cfg.items():
        if _DISK_SLOT_RE.fullmatch(slot) is None:
            continue
        raw = str(value)
        if "cloudinit" in _configured_volume(raw) and disk_option(raw, "media") == "cdrom":
            slots.append(slot)
    return sorted(slots)


def _extra_data_disks(vm_cfg: dict[str, str], root_disk: str) -> list[str]:
    cloud_init_slots = set(_cloud_init_slots(vm_cfg))
    extras: list[str] = []
    for slot, value in vm_cfg.items():
        if _DISK_SLOT_RE.fullmatch(slot) is None:
            continue
        if slot == root_disk or slot in cloud_init_slots:
            continue
        if _configured_volume(value) not in {"", "none"}:
            extras.append(slot)
    return sorted(extras)


def _network_bridges(vm_cfg: dict[str, str]) -> dict[str, str]:
    bridges: dict[str, str] = {}
    for slot, value in vm_cfg.items():
        if _NET_SLOT_RE.fullmatch(slot) is None or not str(value).strip():
            continue
        bridge = disk_option(str(value), "bridge")
        if not bridge:
            raise AppError(f"Gold VM {slot} has no bridge configured")
        bridges[slot] = bridge
    return bridges


def _volume_records(vm_cfg: dict[str, str]) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    slot_re = re.compile(r"(?:ide|sata|scsi|virtio)[0-9]+|efidisk[0-9]+|tpmstate[0-9]+")
    for slot, value in sorted(vm_cfg.items()):
        if slot_re.fullmatch(slot) is None:
            continue
        volume = _configured_volume(value)
        if not volume or volume == "none":
            continue
        storage = volume.split(":", 1)[0] if ":" in volume else ""
        records.append({"slot": slot, "volume": volume, "storage": storage})
    return records


def _require_backed_up(
    vm_cfg: dict[str, str], slots: list[str], *, label: str
) -> None:
    for slot in slots:
        value = vm_cfg.get(slot)
        if not value:
            continue
        if disk_option(str(value), "backup") == "0":
            raise AppError(
                f"Gold VM {label} disk {slot} has backup=0; cross-node create "
                "requires it to be included in the streamed archive"
            )


def _target_bridges(inventory: list[dict[str, Any]]) -> set[str]:
    return {
        str(item.get("iface") or "")
        for item in inventory
        if str(item.get("type") or "").lower() in {"bridge", "ovsbridge"}
        and str(item.get("iface") or "")
    }


def capture_gold_metadata(
    gold_vmid: int,
    gold_cfg: dict[str, str],
    root_disk: str,
) -> dict[str, Any]:
    """Validate the source Gold contract and return non-sensitive invariants."""
    root_config = str(gold_cfg.get(root_disk) or "")
    if not root_config:
        raise AppError(f"Gold VM {gold_vmid} has no {root_disk} disk")
    cloud_init_slots = _cloud_init_slots(gold_cfg)
    if not cloud_init_slots:
        raise AppError(f"Gold VM {gold_vmid} has no Cloud-Init drive")
    extra_disks = _extra_data_disks(gold_cfg, root_disk)
    unused_disks = sorted(
        slot
        for slot, value in gold_cfg.items()
        if re.fullmatch(r"unused[0-9]+", slot)
        and _configured_volume(value) not in {"", "none"}
    )
    if extra_disks or unused_disks:
        raise AppError(
            f"Gold VM {gold_vmid} has unsupported extra data disks: "
            + ", ".join([*extra_disks, *unused_disks])
        )

    _require_backed_up(gold_cfg, [root_disk], label=str(gold_vmid))
    _require_backed_up(gold_cfg, cloud_init_slots, label=str(gold_vmid))
    _require_backed_up(
        gold_cfg,
        sorted(
            slot
            for slot in gold_cfg
            if _EFI_SLOT_RE.fullmatch(slot) or _TPM_SLOT_RE.fullmatch(slot)
        ),
        label=str(gold_vmid),
    )

    bridges = _network_bridges(gold_cfg)
    if "net0" not in bridges:
        raise AppError(f"Gold VM {gold_vmid} has no bridged net0 NIC")
    return {
        "gold_vmid": gold_vmid,
        "root_disk": root_disk,
        "root_config": root_config,
        "cloudinit_slots": cloud_init_slots,
        "bridges": bridges,
        "volumes": _volume_records(gold_cfg),
    }


def validate_cross_node_create(
    session: Transport,
    cfg: Config,
    *,
    source_node: str,
    target_node: str,
    gold_vmid: int,
    gold_cfg: dict[str, str],
    root_disk: str,
    requested_storage: str | None,
) -> dict[str, Any]:
    """Validate and capture the source/target contract for a streamed create."""
    statuses = {
        str(item.get("node") or ""): item
        for item in cluster_node_statuses(session)
        if item.get("node")
    }
    if source_node not in statuses:
        raise AppError(f"Configured Gold node {source_node!r} is not in the Proxmox cluster")
    if target_node not in statuses:
        raise AppError(f"Target node {target_node!r} is not in the Proxmox cluster")
    if not statuses[source_node].get("online") or str(
        statuses[source_node].get("status") or ""
    ).lower() != "online":
        raise AppError(f"Configured Gold node {source_node!r} is offline")
    if not statuses[target_node].get("online") or str(
        statuses[target_node].get("status") or ""
    ).lower() != "online":
        raise AppError(f"Target node {target_node!r} is offline")

    check_remote_requirements(session, cfg, source_node)
    check_remote_requirements(session, cfg, target_node)
    if not command_exists_on_node(session, cfg, source_node, "vzdump"):
        raise AppError(f"Gold node {source_node!r} is missing required command 'vzdump'")
    if not command_exists_on_node(session, cfg, cfg.control_node, "bash"):
        raise AppError(
            f"Control node {cfg.control_node!r} is missing required command 'bash'"
        )

    source_metadata = capture_gold_metadata(gold_vmid, gold_cfg, root_disk)

    target_storage = resolve_homestack_storage(cfg, target_node, requested_storage)
    target_inventory = node_storage_inventory(session, target_node)
    _storage_item(target_inventory, target_storage, "images")
    _storage_item(target_inventory, cfg.snippet_storage, "snippets")

    target_network = node_network_inventory(session, target_node)
    available_bridges = _target_bridges(target_network)
    missing_bridges = sorted(
        set(dict(source_metadata["bridges"]).values()) - available_bridges
    )
    if missing_bridges:
        raise AppError(
            f"Target node {target_node!r} is missing Gold network bridge(s): "
            + ", ".join(missing_bridges)
        )

    return {
        "target_storage": target_storage,
        "source_metadata": source_metadata,
    }


def transfer_node_shell_command(cfg: Config, node: str, command: str) -> str:
    """Route one binary-safe transfer leg from the control node."""
    if node == cfg.control_node:
        return command
    return shlex.join(
        ["ssh", "-T", "-o", "BatchMode=yes", f"root@{node}", command]
    )


def build_stream_restore_command(
    cfg: Config,
    *,
    source_node: str,
    target_node: str,
    vmid: int,
    name: str,
    target_storage: str,
) -> str:
    source = shlex.join(
        [
            "vzdump",
            str(cfg.gold_vmid),
            "--stdout",
            "1",
            "--dumpdir",
            "/var/tmp",
            "--tmpdir",
            "/var/tmp",
            "--mode",
            "snapshot",
            "--compress",
            "0",
            "--fleecing",
            "enabled=0",
            "--script",
            "/bin/true",
            "--mailto",
            "",
            "--notification-mode",
            "legacy-sendmail",
            "--prune-backups",
            "keep-all=1",
            "--remove",
            "0",
        ]
    )
    target = shlex.join(
        [
            "qm",
            "create",
            str(vmid),
            "--archive",
            "-",
            "--storage",
            target_storage,
            "--unique",
            "1",
            "--start",
            "0",
            "--template",
            "0",
            "--name",
            name,
        ]
    )
    pipeline = (
        f"{transfer_node_shell_command(cfg, source_node, source)} | "
        f"{transfer_node_shell_command(cfg, target_node, target)}"
    )
    return shlex.join(["bash", "-o", "pipefail", "-c", pipeline])


def stream_gold_restore(
    session: Transport,
    cfg: Config,
    *,
    source_node: str,
    target_node: str,
    vmid: int,
    name: str,
    target_storage: str,
    timeout: int = 7200,
) -> None:
    """Stream a Gold archive through CONTROL and restore it on TARGET."""
    command = build_stream_restore_command(
        cfg,
        source_node=source_node,
        target_node=target_node,
        vmid=vmid,
        name=name,
        target_storage=target_storage,
    )
    try:
        result = session.run(
            command,
            check=False,
            timeout=max(7200, timeout),
        )
    except Exception as exc:
        raise AppError(f"Cross-node Gold restore pipeline failed: {exc}") from exc
    if result.returncode != 0:
        detail = result.output.strip()
        raise AppError(
            f"Cross-node Gold restore pipeline failed with exit code {result.returncode}"
            + (f": {detail}" if detail else "")
        )
