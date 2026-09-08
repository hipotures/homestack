"""Read-only discovery of local Herdr and Proxmox environments."""

from __future__ import annotations

from pathlib import Path
import shutil
from typing import Any

from rich.panel import Panel
from rich.table import Table

from .models import AppError, GOLD_TAG
from .proxmox import (
    cluster_node_statuses,
    cluster_storage_definitions,
    cluster_vm_resources,
    has_tag,
)
from .transports.herdr import (
    HerdrCandidate,
    discover_herdr_candidates,
    open_herdr_candidate,
)
from .ui import console


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


def discover_environment() -> dict[str, Any]:
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

    try:
        candidates = discover_herdr_candidates()
    except AppError as exc:
        report["errors"].append(str(exc))
        return report

    for candidate in candidates:
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

    report["ok"] = any(entry["verified"] for entry in report["sessions"])
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
    "discover_environment",
    "discover_hardware_identities",
    "show_discovery_report",
    "verified_sessions",
]
