"""Interactive HomeStack runtime configuration installer."""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import replace
import ipaddress
from pathlib import Path
import re
import sys
from typing import Any, Callable

from rich.panel import Panel
from rich.progress import (
    BarColumn,
    Progress,
    SpinnerColumn,
    TaskProgressColumn,
    TextColumn,
    TimeElapsedColumn,
)
from rich.prompt import Confirm, IntPrompt, Prompt
from rich.table import Table

from .config import (
    Config,
    WorkspaceSSHConfig,
    config_to_toml,
    load_config,
    publish_config,
    validate_config_text,
    validate_sync_path_spec,
)
from .discovery import (
    NetworkCandidate,
    discover_environment,
    discover_hardware_identities,
    discover_workspace_networks,
    show_discovery_report,
    verified_sessions,
)
from .models import AppError, GOLD_TAG, integer_value
from .proxmox import (
    cluster_node_statuses,
    cluster_storage_definitions,
    cluster_vm_resources,
    has_tag,
    homestack_storage_layout_name,
    node_storage_inventory,
    parse_home_size,
    qm_config_on_node,
    require_gold_tag,
)
from .transports import open_transport
from .transports.base import Transport
from .transports.herdr import HerdrCandidate
from .ui import console


def _install_progress() -> Progress:
    return Progress(
        SpinnerColumn(),
        TextColumn("[bold]{task.description}"),
        BarColumn(),
        TaskProgressColumn(),
        TimeElapsedColumn(),
        console=console,
        refresh_per_second=8,
    )


def _discover_environment_with_progress() -> dict[str, Any]:
    with _install_progress() as progress:
        task = progress.add_task("Scan Herdr SSH sessions", total=1)

        def update(description: str, completed: int, total: int) -> None:
            progress.update(
                task,
                description=description,
                completed=completed,
                total=total,
            )

        return discover_environment(progress=update)


@contextmanager
def _open_transport_with_progress(cfg: Config):
    progress = _install_progress()
    task = progress.add_task(
        "Verify selected Herdr administrative session",
        total=1,
    )
    progress.start()
    try:
        with open_transport(cfg) as session:
            progress.update(
                task,
                completed=1,
                description="Herdr administrative session verified",
            )
            progress.stop()
            yield session
    finally:
        progress.stop()


def _load_proxmox_inventory_with_progress(
    session: Transport,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    with _install_progress() as progress:
        task = progress.add_task("Load Proxmox node inventory", total=3)
        statuses = cluster_node_statuses(session)
        progress.update(
            task,
            advance=1,
            description="Load Proxmox VM inventory",
        )
        resources = cluster_vm_resources(session)
        progress.update(
            task,
            advance=1,
            description="Load Proxmox storage inventory",
        )
        definitions = cluster_storage_definitions(session)
        progress.update(
            task,
            advance=1,
            description="Proxmox inventory loaded",
        )
    return statuses, resources, definitions


def _discover_networks_with_progress(
    session: Transport,
    cfg: Config,
    resources: list[dict[str, Any]],
    gold_vmid: int,
    gold_node: str,
) -> list[NetworkCandidate]:
    with _install_progress() as progress:
        task = progress.add_task("Read Gold network configuration", total=1)

        def update(description: str, completed: int, total: int) -> None:
            progress.update(
                task,
                description=description,
                completed=completed,
                total=total,
            )

        return discover_workspace_networks(
            session,
            cfg,
            resources,
            gold_vmid,
            gold_node,
            progress=update,
        )


def _fresh_config(path: Path) -> Config:
    """Create current-schema defaults; discovered values replace placeholders."""
    return Config(
        path=path,
        transport_type="herdr",
        node="pve1",
        control_node="pve1",
        gold_vmid=100,
        storage_layouts={"homestack-storage-1": ("local-lvm",)},
        root_storage="local-lvm",
        root_disk="scsi0",
        home_storage="local-lvm",
        home_disk="scsi1",
        default_home_size="20G",
        network_prefix="192.0.2",
        network_cidr=24,
        gateway="192.0.2.1",
        dns_servers=("192.0.2.53",),
        snippet_storage="local",
        snippet_dir=Path("/var/lib/vz/snippets"),
        user_name="user",
        user_uid=1000,
        user_gid=1000,
        workspace_ssh=WorkspaceSSHConfig(
            user="user",
            identity_files=(),
            identities_only=True,
            log_level="FATAL",
        ),
        herdr_workspace="pve",
        herdr_tab="pve1",
        herdr_debug=True,
        storage_display_unit="GiB",
        storage_display_decimals=0,
    )


def _ask_text(label: str, default: str | None = None) -> str:
    while True:
        value = Prompt.ask(label, default=default).strip()
        if value:
            return value
        console.print("[yellow]A value is required.[/yellow]")


def _ask_int(label: str, default: int) -> int:
    return int(IntPrompt.ask(label, default=default))


def _ask_list(label: str, default: tuple[str, ...] = ()) -> tuple[str, ...]:
    default_text = ", ".join(default)
    while True:
        raw = (
            Prompt.ask(label, default=default_text)
            if default_text
            else Prompt.ask(label)
        ).strip()
        values = tuple(item.strip() for item in raw.split(",") if item.strip())
        if values:
            return values
        console.print("[yellow]Enter at least one value.[/yellow]")


def _existing_action(path: Path) -> str:
    console.print(Panel.fit(f"Configuration already exists:\n{path}", title="HOMESTACK INSTALL"))
    console.print("  1. Abort")
    console.print("  2. Reconfigure using current values")
    console.print("  3. Replace configuration from scratch")
    return Prompt.ask("Choose an action", choices=["1", "2", "3"], default="1")


def _configure_transport_manual(base: Config, *, use_defaults: bool) -> Config:
    console.print(Panel.fit("Manual Herdr configuration", title="TRANSPORT"))
    workspace = _ask_text(
        "Herdr workspace",
        base.herdr_workspace if use_defaults else None,
    )
    tab = _ask_text(
        "Herdr tab",
        base.herdr_tab if use_defaults else None,
    )
    control = _ask_text(
        "Expected/control PVE node",
        base.control_node if use_defaults else None,
    )
    return replace(
        base,
        transport_type="herdr",
        control_node=control,
        herdr_workspace=workspace,
        herdr_tab=tab,
    )


def _configure_transport(
    base: Config,
    report: dict[str, Any],
    *,
    prefer_existing: bool,
) -> Config:
    verified = verified_sessions(report)
    if not verified:
        show_discovery_report(report)
        console.print(
            "[yellow]No verified Proxmox session was discovered in Herdr.[/yellow]\n"
            "Open a Herdr tab, SSH to the Proxmox node as root, leave it at the root "
            "shell prompt, and retry."
        )
        if not Confirm.ask("Enter Herdr connection manually?", default=False):
            raise AppError("No verified Herdr Proxmox session is available")
        return _configure_transport_manual(base, use_defaults=prefer_existing)

    preferred_index: int | None = None
    if prefer_existing:
        for index, entry in enumerate(verified):
            candidate = entry.get("candidate", {})
            transport = entry.get("transport") or {}
            if (
                candidate.get("workspace") == base.herdr_workspace
                and candidate.get("tab") == base.herdr_tab
                and transport.get("host") == base.control_node
            ):
                preferred_index = index
                break

    if len(verified) == 1:
        selected = verified[0]
        candidate = selected["candidate"]
        transport = selected.get("transport") or {}
        console.print(
            Panel.fit(
                f"Workspace : {candidate.get('workspace')}\n"
                f"Tab       : {candidate.get('tab')}\n"
                f"SSH       : {candidate.get('ssh_target')}\n"
                f"PVE node  : {transport.get('host') or candidate.get('prompt_host')}",
                title="Detected Proxmox session",
            )
        )
        if not Confirm.ask("Use this detected session?", default=True):
            return _configure_transport_manual(base, use_defaults=prefer_existing)
    else:
        table = Table(title="Verified Proxmox sessions")
        table.add_column("#", justify="right")
        table.add_column("Workspace")
        table.add_column("Tab")
        table.add_column("SSH target")
        table.add_column("PVE node")
        for index, entry in enumerate(verified, 1):
            candidate = entry["candidate"]
            transport = entry.get("transport") or {}
            table.add_row(
                str(index),
                str(candidate.get("workspace") or "—"),
                str(candidate.get("tab") or "—"),
                str(candidate.get("ssh_target") or "—"),
                str(transport.get("host") or candidate.get("prompt_host") or "—"),
            )
        console.print(table)
        default_index = (preferred_index + 1) if preferred_index is not None else 1
        while True:
            choice = IntPrompt.ask("Select the HomeStack control session", default=default_index)
            if 1 <= choice <= len(verified):
                selected = verified[choice - 1]
                break
            console.print(f"[yellow]Choose a number between 1 and {len(verified)}.[/yellow]")

    candidate = HerdrCandidate.from_dict(selected["candidate"])
    transport = selected.get("transport") or {}
    control_node = str(
        transport.get("host") or candidate.prompt_host or candidate.ssh_host
    )
    return replace(
        base,
        transport_type="herdr",
        control_node=control_node,
        herdr_workspace=candidate.workspace,
        herdr_tab=candidate.tab,
    )


def _validated_gold(
    session: Transport,
    cfg: Config,
    resources: list[dict[str, Any]],
    vmid: int,
) -> tuple[dict[str, Any], dict[str, str]]:
    resource = next((item for item in resources if int(item["vmid"]) == vmid), None)
    if resource is None:
        raise AppError(f"VM {vmid} was not found in the Proxmox environment")
    node = str(resource.get("node") or "").strip()
    if not node:
        raise AppError(f"Proxmox did not report a node for VM {vmid}")
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    require_gold_tag(vmid, vm_cfg)
    if not vm_cfg.get(cfg.root_disk):
        raise AppError(f"Gold VM {vmid} has no {cfg.root_disk} disk")
    return resource, vm_cfg


def _choose_gold(
    session: Transport,
    cfg: Config,
    resources: list[dict[str, Any]],
    *,
    preferred_vmid: int | None = None,
) -> tuple[int, str]:
    tagged = [item for item in resources if has_tag(item.get("tags"), GOLD_TAG)]
    valid: list[tuple[dict[str, Any], dict[str, str]]] = []
    for item in tagged:
        try:
            valid.append(_validated_gold(session, cfg, resources, int(item["vmid"])))
        except AppError as exc:
            console.print(f"[yellow]Cannot use tagged VM {item['vmid']}: {exc}[/yellow]")

    if len(valid) == 1:
        resource, vm_cfg = valid[0]
        console.print(Panel.fit(
            f"VMID : {resource['vmid']}\n"
            f"Name : {resource.get('name') or vm_cfg.get('name') or '-'}\n"
            f"Node : {resource.get('node')}",
            title="Gold VM detected",
        ))
        if Confirm.ask("Use this Gold VM?", default=True):
            return int(resource["vmid"]), str(resource["node"])

    if len(valid) > 1:
        table = Table(title="Gold VM candidates")
        table.add_column("#", justify="right")
        table.add_column("VMID", justify="right")
        table.add_column("Name")
        table.add_column("Node")
        valid_vmids = [int(resource["vmid"]) for resource, _ in valid]
        for index, (resource, vm_cfg) in enumerate(valid, 1):
            table.add_row(
                str(index),
                str(resource["vmid"]),
                str(resource.get("name") or vm_cfg.get("name") or "-"),
                str(resource.get("node") or "-"),
            )
        console.print(table)
        default_index = (
            valid_vmids.index(preferred_vmid) + 1
            if preferred_vmid in valid_vmids
            else None
        )
        while True:
            choice = (
                IntPrompt.ask("Select Gold VM", default=default_index)
                if default_index is not None
                else IntPrompt.ask("Select Gold VM")
            )
            if 1 <= choice <= len(valid):
                resource, _ = valid[choice - 1]
                return int(resource["vmid"]), str(resource["node"])
            console.print(f"[yellow]Choose a number between 1 and {len(valid)}.[/yellow]")

    if resources:
        table = Table(title="Available virtual machines")
        table.add_column("VMID", justify="right")
        table.add_column("Name")
        table.add_column("Node")
        table.add_column("Gold tag")
        for item in resources:
            table.add_row(
                str(item["vmid"]),
                str(item.get("name") or "-"),
                str(item.get("node") or "-"),
                "yes" if has_tag(item.get("tags"), GOLD_TAG) else "no",
            )
        console.print(table)
    valid_vmids = {int(resource["vmid"]) for resource, _ in valid}
    default_vmid = preferred_vmid if preferred_vmid in valid_vmids else None
    while True:
        vmid = int(
            IntPrompt.ask("Gold VMID", default=default_vmid)
            if default_vmid is not None
            else IntPrompt.ask("Gold VMID")
        )
        try:
            resource, _ = _validated_gold(session, cfg, resources, vmid)
            return vmid, str(resource["node"])
        except AppError as exc:
            console.print(f"[yellow]{exc}[/yellow]")


def _storage_nodes(raw: Any) -> set[str] | None:
    if raw is None or not str(raw).strip():
        return None
    return {item.strip() for item in str(raw).split(",") if item.strip()}


def _has_content(item: dict[str, Any], content: str) -> bool:
    return content in {part.strip() for part in str(item.get("content") or "").split(",")}


def _storage_options(
    session: Transport,
    statuses: list[dict[str, Any]],
    definitions: list[dict[str, Any]],
) -> tuple[dict[str, list[str]], set[str]]:
    options: dict[str, list[str]] = {}
    unverified: set[str] = set()
    for status in statuses:
        node = str(status["node"])
        if status.get("online"):
            inventory = node_storage_inventory(session, node)
            candidates = [
                str(item["storage"])
                for item in inventory
                if _has_content(item, "images")
                and integer_value(item.get("enabled")) != 0
                and integer_value(item.get("active")) != 0
            ]
        else:
            unverified.add(node)
            candidates = []
            for item in definitions:
                nodes = _storage_nodes(item.get("nodes"))
                if _has_content(item, "images") and (nodes is None or node in nodes):
                    candidates.append(str(item["storage"]))
        options[node] = list(dict.fromkeys(candidates))
    return options, unverified


def _select_numbered(
    label: str,
    options: list[str],
    defaults: tuple[str, ...] = (),
) -> tuple[str, ...]:
    if not options:
        return ()
    if len(options) == 1:
        console.print(f"  [1] {options[0]}")
        return (options[0],)
    for index, value in enumerate(options, 1):
        console.print(f"  [{index}] {value}")
    default_indices = [str(options.index(value) + 1) for value in defaults if value in options]
    default_text = ",".join(default_indices)
    while True:
        raw = (
            Prompt.ask(label, default=default_text)
            if default_text
            else Prompt.ask(label)
        ).strip()
        try:
            indices = [int(item.strip()) for item in raw.split(",") if item.strip()]
        except ValueError:
            indices = []
        if indices and len(set(indices)) == len(indices) and all(1 <= item <= len(options) for item in indices):
            return tuple(options[item - 1] for item in indices)
        console.print("[yellow]Enter one or more comma-separated item numbers.[/yellow]")


def _configure_storage(
    session: Transport,
    cfg: Config,
    statuses: list[dict[str, Any]],
    definitions: list[dict[str, Any]],
) -> tuple[dict[str, tuple[str, ...]], set[str]]:
    options, unverified = _storage_options(session, statuses, definitions)
    layouts: dict[str, tuple[str, ...]] = {}
    for status in statuses:
        node = str(status["node"])
        layout = homestack_storage_layout_name(node)
        if layout is None:
            raise AppError(
                f"Node {node!r} cannot use the current storage_layouts schema; "
                "PVE node names must end in a positive numeric suffix"
            )
        console.print(Panel.fit(f"Node {node} ({status['status']})", title="STORAGE"))
        available = options[node]
        if not available and not status.get("online") and cfg.storage_layouts.get(layout):
            available = list(cfg.storage_layouts[layout])
            console.print(
                "[yellow]Node-local storage checks are unavailable; preserving the current "
                "layout as an unverified choice.[/yellow]"
            )
        if not available:
            console.print("[yellow]No usable VM-image storage could be discovered for this node.[/yellow]")
            if node == cfg.node:
                raise AppError(f"No VM-image storage is available for active node {node}")
            continue
        selected = _select_numbered(
            "Select HomeStack workspace storage(s); first is the default",
            available,
            cfg.storage_layouts.get(layout, ()),
        )
        layouts[layout] = selected
        console.print(f"Default on {node}: [bold]{selected[0]}[/bold]")
    return layouts, unverified


def _configure_snippets(cfg: Config, definitions: list[dict[str, Any]]) -> tuple[str, Path]:
    candidates = []
    for item in definitions:
        restricted_nodes = _storage_nodes(item.get("nodes"))
        if _has_content(item, "snippets") and (
            restricted_nodes is None or cfg.node in restricted_nodes
        ):
            candidates.append(item)
    names = [str(item["storage"]) for item in candidates]
    if names:
        defaults = (cfg.snippet_storage,) if cfg.snippet_storage in names else ()
        selected = _select_numbered("Snippet-capable storage", names, defaults)[0]
        definition = next(item for item in candidates if str(item["storage"]) == selected)
    else:
        selected = _ask_text("Snippet storage ID", cfg.snippet_storage)
        definition = {}
    storage_path = str(definition.get("path") or "").rstrip("/")
    derived = Path(storage_path) / "snippets" if storage_path else cfg.snippet_dir
    snippet_dir = Path(_ask_text("Snippet directory", str(derived)))
    if not snippet_dir.is_absolute():
        raise AppError("Snippet directory must be an absolute path on the PVE node")
    return selected, snippet_dir


def _validate_network_values(
    network: ipaddress.IPv4Network,
    gateway: str,
    dns: tuple[str, ...],
) -> tuple[str, int, str, tuple[str, ...]]:
    octets = str(network.network_address).split(".")
    if octets[3] != "0" or network.prefixlen > 24:
        raise AppError(
            "The current VMID mapping requires a network ending in .0 with at least "
            "the full .2-.254 host range."
        )
    try:
        gateway_ip = ipaddress.IPv4Address(gateway)
    except ValueError as exc:
        raise AppError("Gateway must be a valid IPv4 address.") from exc
    if gateway_ip not in network:
        raise AppError("Gateway must be inside the workspace network.")
    if not dns:
        raise AppError("At least one DNS server is required.")
    try:
        for value in dns:
            ipaddress.IPv4Address(value)
    except ValueError as exc:
        raise AppError(f"Invalid DNS address: {exc}") from exc
    return ".".join(octets[:3]), network.prefixlen, gateway, dns


def _configure_network_manual(
    cfg: Config,
    *,
    use_defaults: bool,
) -> tuple[str, int, str, tuple[str, ...]]:
    default_network = f"{cfg.network_prefix}.0/{cfg.network_cidr}" if use_defaults else None
    while True:
        raw = _ask_text("Workspace network CIDR", default_network)
        try:
            network = ipaddress.IPv4Network(raw, strict=True)
            if not str(network.network_address).endswith(".0") or network.prefixlen > 24:
                raise ValueError(
                    "the current VMID mapping requires a network ending in .0 "
                    "with at least the full .2-.254 host range"
                )
        except ValueError as exc:
            console.print(f"[yellow]Invalid workspace network: {exc}[/yellow]")
            continue
        break

    gateway_default = cfg.gateway if use_defaults else None
    while True:
        gateway = _ask_text("Gateway", gateway_default)
        try:
            gateway_ip = ipaddress.IPv4Address(gateway)
        except ValueError:
            console.print("[yellow]Gateway must be a valid IPv4 address.[/yellow]")
            continue
        if gateway_ip not in network:
            console.print("[yellow]Gateway must be inside the workspace network.[/yellow]")
            continue
        break

    dns_default = cfg.dns_servers if use_defaults else ()
    while True:
        dns = _ask_list("DNS servers (comma-separated)", dns_default)
        try:
            for value in dns:
                ipaddress.IPv4Address(value)
        except ValueError as exc:
            console.print(f"[yellow]Invalid DNS address: {exc}[/yellow]")
            continue
        break

    prefix = ".".join(str(network.network_address).split(".")[:3])
    console.print(f"Example mapping: VMID 200 → {prefix}.200")
    return prefix, network.prefixlen, gateway, dns


def _configure_network(
    cfg: Config,
    candidates: list[NetworkCandidate],
    *,
    prefer_existing: bool,
) -> tuple[str, int, str, tuple[str, ...]]:
    current_cidr = f"{cfg.network_prefix}.0/{cfg.network_cidr}"
    preferred_index: int | None = None
    if prefer_existing:
        for index, candidate in enumerate(candidates):
            if candidate.cidr == current_cidr and candidate.gateway == cfg.gateway:
                preferred_index = index
                break

    if not candidates:
        console.print("[yellow]No workspace network profile could be discovered automatically.[/yellow]")
        return _configure_network_manual(cfg, use_defaults=prefer_existing)

    table = Table(title="Detected workspace networks")
    table.add_column("#", justify="right")
    table.add_column("Network")
    table.add_column("Bridge")
    table.add_column("Gateway")
    table.add_column("DNS")
    table.add_column("Source")
    for index, candidate in enumerate(candidates, 1):
        table.add_row(
            str(index),
            candidate.cidr,
            candidate.bridge or "—",
            candidate.gateway or "not detected",
            ", ".join(candidate.dns_servers) if candidate.dns_servers else "not detected",
            candidate.source,
        )
    console.print(table)

    if len(candidates) == 1:
        selected = candidates[0]
    else:
        default_index = preferred_index + 1 if preferred_index is not None else None
        while True:
            choice = (
                IntPrompt.ask("Select workspace network", default=default_index)
                if default_index is not None
                else IntPrompt.ask("Select workspace network")
            )
            if 1 <= choice <= len(candidates):
                selected = candidates[choice - 1]
                break
            console.print(f"[yellow]Choose a number between 1 and {len(candidates)}.[/yellow]")

    network = ipaddress.IPv4Network(selected.cidr, strict=True)

    gateway = selected.gateway
    while gateway is None:
        value = _ask_text("Gateway")
        try:
            gateway_ip = ipaddress.IPv4Address(value)
        except ValueError:
            console.print("[yellow]Gateway must be a valid IPv4 address.[/yellow]")
            continue
        if gateway_ip not in network:
            console.print("[yellow]Gateway must be inside the workspace network.[/yellow]")
            continue
        gateway = value

    dns = selected.dns_servers
    while not dns:
        values = _ask_list("DNS servers (comma-separated)")
        try:
            for value in values:
                ipaddress.IPv4Address(value)
        except ValueError as exc:
            console.print(f"[yellow]Invalid DNS address: {exc}[/yellow]")
            continue
        dns = values

    prefix, cidr, gateway, dns = _validate_network_values(network, gateway, dns)
    console.print(
        Panel.fit(
            f"Network : {selected.cidr}\n"
            f"Bridge  : {selected.bridge or 'inherited from Gold'}\n"
            f"Gateway : {gateway}\n"
            f"DNS     : {', '.join(dns)}\n"
            f"VMID 200: {prefix}.200",
            title="Workspace network",
        )
    )
    if Confirm.ask("Use this network profile?", default=True):
        return prefix, cidr, gateway, dns
    return _configure_network_manual(cfg, use_defaults=prefer_existing)


def _configure_identities(cfg: Config) -> tuple[str, ...]:
    discovered = discover_hardware_identities()
    options = list(dict.fromkeys((*cfg.workspace_ssh.identity_files, *discovered)))
    if not options:
        while True:
            identities = _ask_list("Workspace SSH IdentityFile path(s), comma-separated")
            invalid = [
                value
                for value in identities
                if any(ch.isspace() for ch in value)
                or any(ch in value for ch in ("\x00", "\n", "\r"))
            ]
            if not invalid and len(set(identities)) == len(identities):
                return identities
            console.print("[yellow]Identity paths must be unique and contain no whitespace.[/yellow]")

    defaults = (
        cfg.workspace_ssh.identity_files
        if cfg.workspace_ssh.identity_files
        else tuple(options)
    )
    table = Table(title="Hardware-backed SSH identities")
    table.add_column("#", justify="right")
    table.add_column("IdentityFile")
    table.add_column("Selected")
    for index, value in enumerate(options, 1):
        table.add_row(str(index), value, "yes" if value in defaults else "no")
    console.print(table)

    selected = _select_numbered(
        "Select workspace SSH identities",
        options,
        defaults,
    )
    return selected


def _configure_sync(cfg: Config) -> tuple[tuple[str, ...], tuple[str, ...], bool]:
    current = bool(cfg.sync_paths or cfg.sync_commands)
    if not Confirm.ask("Configure workspace sync now?", default=current):
        return (), (), False
    paths: list[str] = []
    console.print("Enter sync paths in order. Leave blank when finished.")
    pending_paths = list(cfg.sync_paths)
    while True:
        existing = pending_paths.pop(0) if pending_paths else ""
        value = Prompt.ask("Sync path", default=existing).strip()
        if not value:
            break
        try:
            validate_sync_path_spec(value)
        except AppError as exc:
            console.print(f"[yellow]{exc}[/yellow]")
            continue
        if value in paths:
            console.print(f"[yellow]Duplicate sync path: {value}[/yellow]")
            continue
        paths.append(value)
    commands: list[str] = []
    console.print("Enter post-sync commands in order. Leave blank when finished.")
    pending_commands = list(cfg.sync_commands)
    while True:
        existing = pending_commands.pop(0) if pending_commands else ""
        value = Prompt.ask("Post-sync command", default=existing).strip()
        if not value:
            break
        commands.append(value)
    verbose = Confirm.ask("Verbose rsync output?", default=cfg.sync_verbose)
    return tuple(paths), tuple(commands), verbose


def _show_summary(
    cfg: Config,
    statuses: list[dict[str, Any]],
    gold_node: str,
    unverified_nodes: set[str],
) -> None:
    table = Table(title="HOMESTACK INSTALL", show_header=False)
    table.add_column("Setting", style="bold")
    table.add_column("Value")
    table.add_row("Configuration", str(cfg.path))
    table.add_row("Transport", "Herdr (verified)")
    table.add_row("Control node", cfg.control_node)
    table.add_row("Proxmox nodes", str(len(statuses)))
    for item in statuses:
        note = " (storage metadata not node-verified)" if item["node"] in unverified_nodes else ""
        table.add_row(f"  {item['node']}", f"{item['status']}{note}")
    table.add_row("Gold", f"VMID {cfg.gold_vmid} on {gold_node} (verified)")
    for item in statuses:
        layout = homestack_storage_layout_name(str(item["node"]))
        if layout in cfg.storage_layouts:
            table.add_row(f"Storage {item['node']}", ", ".join(cfg.storage_layouts[layout]))
    table.add_row("Network", f"{cfg.network_prefix}.0/{cfg.network_cidr}")
    table.add_row("Gateway", cfg.gateway)
    table.add_row("DNS", ", ".join(cfg.dns_servers))
    table.add_row("Workspace", f"{cfg.user_name} ({cfg.user_uid}:{cfg.user_gid}); home {cfg.default_home_size}")
    table.add_row(
        "SSH identities",
        f"{len(cfg.workspace_ssh.identity_files)} selected\n"
        + "\n".join(cfg.workspace_ssh.identity_files),
    )
    table.add_row("Snippets", f"{cfg.snippet_storage}: {cfg.snippet_dir}")
    table.add_row("Sync", f"{len(cfg.sync_paths)} paths; {len(cfg.sync_commands)} commands")
    console.print(table)


def run_installer(path: Path) -> int:
    """Run the interactive installer and write a validated runtime config."""
    if not sys.stdin.isatty():
        raise AppError("HomeStack installation is interactive and requires a TTY")

    existing = path.exists()
    action = "new"
    if existing:
        action = _existing_action(path)
        if action == "1":
            console.print("[bold]Aborted. Existing configuration was not changed.[/bold]")
            return 0
        base = load_config(path) if action == "2" else _fresh_config(path)
    else:
        base = _fresh_config(path)

    discovery_report = _discover_environment_with_progress()
    base = _configure_transport(
        base,
        discovery_report,
        prefer_existing=(action == "2"),
    )
    with _open_transport_with_progress(base) as session:
        statuses, resources, definitions = _load_proxmox_inventory_with_progress(session)
        gold_vmid, gold_node = _choose_gold(
            session,
            base,
            resources,
            preferred_vmid=base.gold_vmid if action == "2" else None,
        )
        base = replace(base, node=gold_node, gold_vmid=gold_vmid)
        layouts, unverified_nodes = _configure_storage(
            session, base, statuses, definitions
        )
        active_layout = homestack_storage_layout_name(gold_node)
        if active_layout is None or active_layout not in layouts:
            raise AppError(f"Gold node {gold_node!r} has no configured HomeStack storage")
        default_storage = layouts[active_layout][0]
        snippet_storage, snippet_dir = _configure_snippets(base, definitions)
        network_candidates = _discover_networks_with_progress(
            session,
            base,
            resources,
            gold_vmid,
            gold_node,
        )

    prefix, cidr, gateway, dns = _configure_network(
        base,
        network_candidates,
        prefer_existing=(action == "2"),
    )
    user_name = _ask_text("Workspace user", base.user_name)
    user_uid = _ask_int("Workspace UID", base.user_uid)
    user_gid = _ask_int("Workspace GID", base.user_gid)
    home_size, _ = parse_home_size(_ask_text("Default persistent home size", base.default_home_size))
    identities = _configure_identities(base)
    sync_paths, sync_commands, sync_verbose = _configure_sync(base)

    cfg = replace(
        base,
        path=path,
        storage_layouts=layouts,
        root_storage=default_storage,
        home_storage=default_storage,
        snippet_storage=snippet_storage,
        snippet_dir=snippet_dir,
        network_prefix=prefix,
        network_cidr=cidr,
        gateway=gateway,
        dns_servers=dns,
        user_name=user_name,
        user_uid=user_uid,
        user_gid=user_gid,
        default_home_size=home_size,
        workspace_ssh=replace(
            base.workspace_ssh,
            user=user_name,
            identity_files=identities,
        ),
        sync_paths=sync_paths,
        sync_commands=sync_commands,
        sync_verbose=sync_verbose,
    )
    validate_config_text(config_to_toml(cfg))
    _show_summary(cfg, statuses, gold_node, unverified_nodes)
    if not Confirm.ask("Write this configuration?", default=False):
        console.print("[bold]Cancelled. No configuration was written.[/bold]")
        return 0

    backup = publish_config(cfg)
    result = Table(title="HOMESTACK CONFIGURED", show_header=False)
    result.add_column("Item", style="bold")
    result.add_column("Result")
    result.add_row("Configuration", str(path))
    result.add_row("Transport", "verified")
    result.add_row("Proxmox", "verified")
    result.add_row("Gold", "verified")
    result.add_row("Storage", "configured")
    result.add_row("Config", "validated and written")
    if backup is not None:
        result.add_row("Backup", str(backup))
    console.print(result)
    console.print("Next: homestack transport   or   homestack status")
    return 0


__all__ = ["run_installer"]
