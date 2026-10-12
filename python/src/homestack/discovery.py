"""Read-only discovery of local Herdr and Proxmox environments."""

from __future__ import annotations

from dataclasses import dataclass
import ipaddress
from pathlib import Path
import re
import shutil
from typing import Any, Callable

from rich.panel import Panel
from rich.table import Table

from .models import AppError, GOLD_TAG, WORKSPACE_TAG
from .proxmox import (
    cluster_node_statuses,
    cluster_storage_definitions,
    cluster_vm_resources,
    has_tag,
    node_dns_config,
    node_network_inventory,
    qm_config_on_node,
)
from .transports.herdr import (
    HerdrCandidate,
    discover_herdr_candidates,
    open_herdr_candidate,
)
from .ui import console


@dataclass(frozen=True)
class NetworkCandidate:
    cidr: str
    bridge: str | None
    gateway: str | None
    dns_servers: tuple[str, ...]
    source: str


def _config_option(value: str | None, key: str) -> str | None:
    if not value:
        return None
    for item in str(value).split(","):
        token = item.strip()
        if "=" not in token:
            continue
        item_key, item_value = token.split("=", 1)
        if item_key == key:
            return item_value.strip() or None
    return None


def _parse_dns_values(value: str | None) -> tuple[str, ...]:
    if not value:
        return ()
    result: list[str] = []
    for token in re.split(r"[\s,;]+", value.strip()):
        if not token:
            continue
        try:
            ipaddress.IPv4Address(token)
        except ValueError:
            continue
        if token not in result:
            result.append(token)
    return tuple(result)


def _mapping_compatible(network: ipaddress.IPv4Network) -> bool:
    return str(network.network_address).endswith(".0") and network.prefixlen <= 24


def _network_from_ipconfig(value: str | None) -> tuple[str, str | None] | None:
    ip_value = _config_option(value, "ip")
    if not ip_value or ip_value in {"dhcp", "manual"}:
        return None
    try:
        interface = ipaddress.IPv4Interface(ip_value)
    except ValueError:
        return None
    if not _mapping_compatible(interface.network):
        return None
    return str(interface.network), _config_option(value, "gw")


def _network_from_pve_interface(item: dict[str, Any]) -> str | None:
    address = str(item.get("address") or "").strip()
    cidr = item.get("cidr")
    netmask = str(item.get("netmask") or "").strip()

    try:
        if "/" in address:
            network = ipaddress.IPv4Interface(address).network
        elif address and cidr is not None and str(cidr).strip():
            cidr_text = str(cidr).strip()
            if "/" in cidr_text:
                network = ipaddress.IPv4Interface(cidr_text).network
            else:
                network = ipaddress.IPv4Interface(f"{address}/{cidr_text}").network
        elif address and netmask:
            network = ipaddress.IPv4Interface(f"{address}/{netmask}").network
        else:
            return None
    except ValueError:
        return None

    if not _mapping_compatible(network):
        return None
    return str(network)


def _pve_dns_servers(data: dict[str, Any]) -> tuple[str, ...]:
    values: list[str] = []
    for key in ("dns1", "dns2", "dns3"):
        value = str(data.get(key) or "").strip()
        if not value:
            continue
        try:
            ipaddress.IPv4Address(value)
        except ValueError:
            continue
        if value not in values:
            values.append(value)
    return tuple(values)


def discover_workspace_networks(
    session: Any,
    cfg: Any,
    resources: list[dict[str, Any]],
    gold_vmid: int,
    gold_node: str,
    *,
    progress: Callable[[str, int, int], None] | None = None,
) -> list[NetworkCandidate]:
    """Discover network profiles compatible with the Gold VM's inherited net0 bridge."""
    workspace_resources = [
        item for item in resources if has_tag(item.get("tags"), WORKSPACE_TAG)
    ]
    total_steps = len(workspace_resources) + 3
    if progress is not None:
        progress("Read Gold network configuration", 0, total_steps)

    gold_cfg = qm_config_on_node(session, cfg, gold_node, gold_vmid)
    gold_bridge = _config_option(gold_cfg.get("net0"), "bridge")
    if progress is not None:
        progress("Read PVE DNS configuration", 1, total_steps)

    try:
        dns_fallback = _pve_dns_servers(node_dns_config(session, gold_node))
    except AppError:
        dns_fallback = ()
    if progress is not None:
        progress("Inspect existing HomeStack workspace networks", 2, total_steps)

    candidates: list[NetworkCandidate] = []

    def add(candidate: NetworkCandidate) -> None:
        key = (candidate.cidr, candidate.bridge)
        for index, existing in enumerate(candidates):
            if (existing.cidr, existing.bridge) != key:
                continue
            candidates[index] = NetworkCandidate(
                cidr=existing.cidr,
                bridge=existing.bridge,
                gateway=existing.gateway or candidate.gateway,
                dns_servers=existing.dns_servers or candidate.dns_servers,
                source=existing.source,
            )
            return
        candidates.append(candidate)

    completed_steps = 2
    for resource in workspace_resources:
        vmid = int(resource["vmid"])
        node = str(resource.get("node") or "").strip()
        if not node:
            continue
        try:
            vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        except AppError:
            continue
        bridge = _config_option(vm_cfg.get("net0"), "bridge")
        if gold_bridge and bridge and bridge != gold_bridge:
            continue
        network_data = _network_from_ipconfig(vm_cfg.get("ipconfig0"))
        if network_data is None:
            continue
        cidr, gateway = network_data
        add(
            NetworkCandidate(
                cidr=cidr,
                bridge=bridge or gold_bridge,
                gateway=gateway,
                dns_servers=_parse_dns_values(vm_cfg.get("nameserver")) or dns_fallback,
                source=f"HomeStack workspace VM {vmid}",
            )
        )
        completed_steps += 1
        if progress is not None:
            progress(
                f"Inspect HomeStack workspace VM {vmid}",
                completed_steps,
                total_steps,
            )

    gold_network = _network_from_ipconfig(gold_cfg.get("ipconfig0"))
    if gold_network is not None:
        cidr, gateway = gold_network
        add(
            NetworkCandidate(
                cidr=cidr,
                bridge=gold_bridge,
                gateway=gateway,
                dns_servers=_parse_dns_values(gold_cfg.get("nameserver")) or dns_fallback,
                source=f"Gold VM {gold_vmid}",
            )
        )

    if progress is not None:
        progress("Read PVE bridge configuration", total_steps - 1, total_steps)
    try:
        interfaces = node_network_inventory(session, gold_node)
    except AppError:
        interfaces = []

    for item in interfaces:
        bridge = str(item.get("iface") or "").strip()
        item_type = str(item.get("type") or "").strip().lower()
        if item_type not in {"bridge", "ovsbridge"}:
            continue
        if gold_bridge and bridge != gold_bridge:
            continue
        cidr = _network_from_pve_interface(item)
        if cidr is None:
            continue
        gateway = str(item.get("gateway") or "").strip() or None
        add(
            NetworkCandidate(
                cidr=cidr,
                bridge=bridge or None,
                gateway=gateway,
                dns_servers=dns_fallback,
                source=f"PVE bridge {bridge} on {gold_node}",
            )
        )

    if progress is not None:
        progress("Workspace network discovery complete", total_steps, total_steps)
    return candidates


def discover_hardware_identities() -> list[str]:
    """Return likely local OpenSSH FIDO identity stubs without reading key contents."""
    ssh_dir = Path.home() / ".ssh"
    if not ssh_dir.is_dir():
        return []

    candidates: list[str] = []
    for path in sorted(ssh_dir.glob("id_*_sk*")):
        if not path.is_file() or path.name.endswith(".pub"):
            continue
        candidates.append(f"~/.ssh/{path.name}")
    return candidates


def _storage_summary(item: dict[str, Any]) -> dict[str, Any]:
    return {
        key: item.get(key)
        for key in ("storage", "type", "content", "nodes", "shared")
        if key in item
    }


def _gold_summary(item: dict[str, Any]) -> dict[str, Any]:
    return {
        key: item.get(key)
        for key in ("vmid", "name", "node", "status", "tags")
        if key in item
    }


def discover_environment(
    *,
    progress: Callable[[str, int, int], None] | None = None,
) -> dict[str, Any]:
    """Discover usable Herdr/Proxmox sessions without loading HomeStack config."""
    commands = {
        name: shutil.which(name) is not None
        for name in ("herdr", "ssh", "rsync")
    }
    report: dict[str, Any] = {
        "ok": False,
        "config_used": False,
        "read_only": True,
        "local": {
            "commands": commands,
            "hardware_ssh_identities": discover_hardware_identities(),
        },
        "sessions": [],
        "errors": [],
    }

    if not commands["herdr"]:
        report["errors"].append("Required local command 'herdr' was not found")
        return report

    if progress is not None:
        progress("Scan Herdr SSH sessions", 0, 1)
    try:
        candidates = discover_herdr_candidates()
    except AppError as exc:
        report["errors"].append(str(exc))
        if progress is not None:
            progress("Herdr session discovery failed", 1, 1)
        return report

    total_steps = max(1, len(candidates) + 1)
    if progress is not None:
        progress("Herdr SSH sessions discovered", 1, total_steps)

    for index, candidate in enumerate(candidates, 1):
        if progress is not None:
            progress(
                f"Verify {candidate.workspace}/{candidate.tab}",
                index,
                total_steps,
            )
        entry: dict[str, Any] = {
            "candidate": candidate.as_dict(),
            "verified": False,
            "error": None,
            "transport": None,
            "proxmox_version": None,
            "nodes": [],
            "gold_candidates": [],
            "storages": [],
        }
        try:
            with open_herdr_candidate(candidate) as session:
                version = session.run_json_value(
                    "pvesh get /version --output-format json",
                    timeout=10,
                )
                if not isinstance(version, dict):
                    raise AppError("Proxmox version query did not return a JSON object")

                nodes = cluster_node_statuses(session)
                resources = cluster_vm_resources(session)
                definitions = cluster_storage_definitions(session)
                entry.update(
                    {
                        "verified": True,
                        "transport": session.execution_info(),
                        "proxmox_version": version,
                        "nodes": nodes,
                        "gold_candidates": [
                            _gold_summary(item)
                            for item in resources
                            if has_tag(item.get("tags"), GOLD_TAG)
                        ],
                        "storages": [_storage_summary(item) for item in definitions],
                    }
                )
        except (AppError, OSError) as exc:
            entry["error"] = str(exc)
        report["sessions"].append(entry)
        if progress is not None:
            progress(
                f"Checked {candidate.workspace}/{candidate.tab}",
                index + 1,
                total_steps,
            )

    report["ok"] = any(entry["verified"] for entry in report["sessions"])
    if progress is not None:
        progress("Environment discovery complete", total_steps, total_steps)
    return report


def verified_sessions(report: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        entry
        for entry in report.get("sessions", [])
        if isinstance(entry, dict) and entry.get("verified")
    ]


def show_discovery_report(report: dict[str, Any]) -> None:
    console.print(Panel.fit("Read-only environment discovery", title="HOMESTACK DISCOVERY"))

    local = Table(title="Local environment")
    local.add_column("Component")
    local.add_column("State")
    for name, available in report.get("local", {}).get("commands", {}).items():
        local.add_row(str(name), "✓ found" if available else "✗ missing")
    identities = report.get("local", {}).get("hardware_ssh_identities", [])
    local.add_row("hardware SSH identities", str(len(identities)))
    console.print(local)

    sessions = report.get("sessions", [])
    table = Table(title="Detected Herdr SSH sessions")
    table.add_column("#", justify="right")
    table.add_column("State")
    table.add_column("Workspace")
    table.add_column("Tab")
    table.add_column("SSH target")
    table.add_column("PVE host")
    for index, entry in enumerate(sessions, 1):
        candidate = entry.get("candidate", {})
        transport = entry.get("transport") or {}
        table.add_row(
            str(index),
            "✓ Proxmox" if entry.get("verified") else "not verified",
            str(candidate.get("workspace") or "—"),
            str(candidate.get("tab") or "—"),
            str(candidate.get("ssh_target") or "—"),
            str(transport.get("host") or candidate.get("prompt_host") or "—"),
        )
    console.print(table)

    for index, entry in enumerate(sessions, 1):
        if entry.get("verified"):
            nodes = ", ".join(
                f"{item.get('node')} ({item.get('status')})"
                for item in entry.get("nodes", [])
            ) or "none"
            gold = ", ".join(
                f"VM {item.get('vmid')} on {item.get('node')}"
                for item in entry.get("gold_candidates", [])
            ) or "none"
            storages = ", ".join(
                str(item.get("storage"))
                for item in entry.get("storages", [])
                if item.get("storage")
            ) or "none"
            console.print(
                Panel.fit(
                    f"Nodes: {nodes}\nGold candidates: {gold}\nStorage definitions: {storages}",
                    title=f"Proxmox session {index}",
                )
            )
        elif entry.get("error"):
            console.print(f"[yellow]Session {index}: {entry['error']}[/yellow]")

    for error in report.get("errors", []):
        console.print(f"[yellow]{error}[/yellow]")

    console.print(
        "[dim]Discovery did not read or write HomeStack configuration and did not mutate Proxmox resources.[/dim]"
    )


__all__ = [
    "NetworkCandidate",
    "discover_environment",
    "discover_hardware_identities",
    "discover_workspace_networks",
    "show_discovery_report",
    "verified_sessions",
]
