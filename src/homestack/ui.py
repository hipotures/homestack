"""Ui support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any
from rich.console import Console, Group
from rich.markup import escape
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .models import AppError, integer_value
from .status import free_percent

console = Console()


GLOBAL_STATUS_NARROW_WIDTH = 80


def show_error(message: str) -> None:
    """Render an application error without interpreting its text as Rich markup."""
    console.print(Panel(escape(message), title="ERROR"))


def ui_value(value: Any, *, placeholder: str = "—") -> str:
    if value is None or value == "":
        return f"[dim]{placeholder}[/dim]"
    return str(value)


def ui_check(value: bool | None) -> str:
    if value is True:
        return "[green]✓ yes[/green]"
    if value is False:
        return "[dim]no[/dim]"
    return "[dim]unknown[/dim]"


def ui_vm_status(value: Any) -> str:
    text = str(value or "unknown")
    if text == "running":
        return "[green]● running[/green]"
    if text == "stopped":
        return "[dim]○ stopped[/dim]"
    if text == "absent":
        return "[dim]absent[/dim]"
    return text


def human_bytes(value: int) -> str:
    size = float(value)
    units = ("B", "KiB", "MiB", "GiB", "TiB", "PiB")
    unit = units[0]
    for unit in units:
        if size < 1024.0 or unit == units[-1]:
            break
        size /= 1024.0
    if unit == "B":
        return f"{int(size)} {unit}"
    return f"{size:.1f} {unit}"


def show_kv_panel(title: str, sections: list[list[tuple[str, str]]]) -> None:
    all_rows = [row for section in sections for row in section]
    if not all_rows:
        return

    key_width = max(len(label) for label, _ in all_rows)
    value_width = 0
    for _, value in all_rows:
        value_lines = value.splitlines() or [""]
        value_width = max(
            value_width,
            *(Text.from_markup(line, style="white").cell_len for line in value_lines),
        )
    content_width = key_width + 3 + value_width

    def make_row(label: str, value: str) -> Text:
        lines = value.splitlines() or [""]
        rendered = Text()
        rendered.append(label.ljust(key_width), style="grey70")
        rendered.append(" : ", style="grey70")
        rendered.append_text(Text.from_markup(lines[0], style="white"))
        continuation_indent = " " * (key_width + 3)
        for value_line in lines[1:]:
            rendered.append("\n")
            rendered.append(continuation_indent)
            rendered.append_text(Text.from_markup(value_line, style="white"))
        return rendered

    separator = Text("-" * content_width, style="grey35")
    body: list[Any] = []
    for index, section in enumerate(sections):
        body.extend(make_row(label, value) for label, value in section)
        if index != len(sections) - 1:
            body.append(separator.copy())

    console.print(
        Panel.fit(
            Group(*body),
            title=Text(title, style="dim bright_blue"),
            title_align="center",
            border_style="grey35",
            padding=(1, 2),
        )
    )


def show_create_plan(plan: dict[str, Any]) -> None:
    disk = (
        f'{plan["root_disk_gb"]:g}G [dim]inherited from Gold[/dim]'
        if plan.get("root_disk_gb") is not None
        else "[dim]inherited from Gold[/dim]"
    )
    key_lines = "\n".join(escape(str(label)) for label in plan.get("ssh_public_keys", []))
    sections = [
        [
            ("VMID", str(plan["vmid"])),
            ("Name", str(plan["name"])),
            ("Node", str(plan["node"])),
            ("Source", f'Gold VM {plan["gold_vmid"]}'),
            ("Clone", "FULL [dim]independent RW root[/dim]"),
        ],
        [
            ("IP", f'{plan["ip"]}/{plan["cidr"]}'),
            ("Gateway", str(plan["gateway"])),
        ],
        [
            ("Root storage", str(plan["root_storage"])),
            ("Root disk", disk),
            ("Root volume", str(plan["root_volume_name"])),
        ],
        [
            ("Persistent home", f'{plan["home_disk"]} on {plan["home_storage"]}'),
            ("Home size", str(plan["home_size"])),
            ("Filesystem label", str(plan["home_label"])),
            ("Home volume", str(plan["home_volume_name"])),
        ],
        [
            (
                "Stale snippets",
                (
                    f'[yellow]{len(plan.get("stale_snippets", []))} unreferenced → replace[/yellow]'
                    if plan.get("stale_snippets")
                    else "[dim]none[/dim]"
                ),
            ),
        ],
        [
            ("SSH public keys", key_lines or "[dim]none[/dim]"),
            ("Key source", f'[dim]{escape(str(plan["ssh_key_source"]))}[/dim]'),
        ],
        [
            ("User", f'{plan["user"]} [dim]({plan["uid"]}:{plan["gid"]})[/dim]'),
            ("Transport", str(plan["transport"])),
        ],
    ]
    console.print()
    show_kv_panel("CREATE PLAN — NO CHANGES MADE YET", sections)
    console.print()


def format_sync_lines(items: list[dict[str, Any]], *, result: bool) -> list[str]:
    if not items:
        return []

    path_width = max(4, *(len(str(item.get("path") or "—")) for item in items))
    type_width = max(4, *(len(str(item.get("type") or "—")) for item in items))
    status_title = "RESULT" if result else "PREFLIGHT"
    status_values = [str(item.get("result") if result else item.get("status") or "—") for item in items]
    status_width = max(len(status_title), *(len(value) for value in status_values))

    lines = [
        f"{'PATH':<{path_width}}  {'TYPE':<{type_width}}  {status_title:<{status_width}}"
    ]
    for item, status in zip(items, status_values):
        path = str(item.get("path") or "—")
        item_type = str(item.get("type") or "—")
        line = f"{path:<{path_width}}  {item_type:<{type_width}}  {status:<{status_width}}"
        if status in {"ready", "synced"}:
            line = f"[green]{escape(line)}[/green]"
        elif status in {"missing", "type mismatch", "failed"}:
            line = f"[red]{escape(line)}[/red]"
        else:
            line = escape(line)
        detail = str(item.get("detail") or "").strip()
        if detail:
            line += f"  [dim]{escape(detail)}[/dim]"
        lines.append(line)
    return lines


def format_sync_command_lines(items: list[dict[str, Any]]) -> list[str]:
    if not items:
        return []

    command_width = max(7, *(len(str(item["command"])) for item in items))
    result_width = max(6, *(len(str(item["result"])) for item in items))
    lines = [f"{'COMMAND':<{command_width}}  {'RESULT':<{result_width}}"]
    for item in items:
        command = str(item["command"])
        status = str(item["result"])
        line = f"{command:<{command_width}}  {status:<{result_width}}"
        if status == "succeeded":
            line = f"[green]{escape(line)}[/green]"
        elif status == "failed":
            line = f"[red]{escape(line)}[/red]"
        else:
            line = escape(line)
        detail = str(item.get("detail") or "").strip()
        if detail:
            line += f"  [dim]{escape(detail)}[/dim]"
        lines.append(line)
    return lines


def show_sync_plan(plan: dict[str, Any]) -> None:
    lines = format_sync_lines(
        [item for item in plan.get("items", []) if isinstance(item, dict)],
        result=False,
    )
    sections = [
        [
            ("VMID", str(plan["vmid"])),
            ("Name", str(plan["name"])),
            ("Status", ui_vm_status(plan.get("status"))),
        ],
        [
            ("Target", f'{plan["user"]}@{plan["ip"]}:{plan["target_home"]}'),
            ("Transfer", "RSYNC OVER SSH"),
            ("Verbose", "[green]YES[/green]" if plan.get("verbose") else "[dim]no[/dim]"),
            ("Delete", "[green]NO[/green] [dim]— target-only files are preserved[/dim]"),
        ],
        [
            ("Paths", "\n".join(lines)),
            (
                "Preflight",
                f'{plan["ready"]}/{plan["configured"]} ready'
                + (
                    f' · [red]{plan["preflight_failed"]} issue(s)[/red]'
                    if plan["preflight_failed"]
                    else " · [green]all sources available[/green]"
                ),
            ),
        ],
    ]
    commands = [str(command) for command in plan.get("commands", [])]
    if commands:
        sections.append(
            [
                ("Commands", "\n".join(escape(command) for command in commands)),
                ("Configured commands", str(len(commands))),
            ]
        )
    console.print()
    show_kv_panel("SYNC PLAN — NO CHANGES MADE YET", sections)
    console.print()


def show_sync_result(result: dict[str, Any]) -> None:
    items = [item for item in result.get("items", []) if isinstance(item, dict)]
    lines = format_sync_lines(items, result=True)
    sections = [
        [
            ("VMID", str(result["vmid"])),
            ("Name", str(result["name"])),
            ("Target", f'{result["user"]}@{result["ip"]}:{result["target_home"]}'),
        ],
        [
            ("Paths", "\n".join(lines)),
        ],
        [
            ("Configured", str(result["configured"])),
            ("Synced", f'[green]{result["synced"]}[/green]'),
            (
                "Failed",
                f'[red]{result["failed"]}[/red]' if result["failed"] else "[green]0[/green]",
            ),
        ],
    ]
    command_items = [
        item for item in result.get("command_items", []) if isinstance(item, dict)
    ]
    if command_items:
        sections.append(
            [
                ("Commands", "\n".join(format_sync_command_lines(command_items))),
                ("Configured commands", str(len(command_items))),
                (
                    "Commands succeeded",
                    f'[green]{result["commands_succeeded"]}[/green]',
                ),
                (
                    "Commands failed",
                    f'[red]{result["commands_failed"]}[/red]'
                    if result["commands_failed"]
                    else "[green]0[/green]",
                ),
                ("Commands skipped", str(result["commands_skipped"])),
            ]
        )
    show_kv_panel("SYNC RESULT", sections)


def show_create_result(result: dict[str, Any]) -> None:
    disk = (
        f'{result["root_disk_gb"]:g}G [dim]inherited from Gold[/dim]'
        if result.get("root_disk_gb") is not None
        else "[dim]inherited from Gold[/dim]"
    )
    key_lines: list[str] = []
    for item in result.get("ssh_public_keys", []):
        if isinstance(item, dict):
            label = escape(str(item.get("label") or "unknown"))
            key_lines.append(f"[green]✓[/green] {label}" if item.get("verified") else f"[red]✗[/red] {label}")
        else:
            key_lines.append(escape(str(item)))
    sections = [
        [("VMID", str(result["vmid"])), ("Name", str(result["name"])), ("Status", ui_vm_status(result.get("status")))],
        [("IP", f'{result["ip"]}/{result["cidr"]}'), ("MAC", str(result["mac"]))],
        [("Clone", f'FULL [dim]from Gold {result["gold_vmid"]}[/dim]'), ("Root", f'{disk} on {result["root_storage"]}')],
        [
            ("Persistent home", f'{result["home_disk"]} on {result["home_storage"]}'),
            ("Home size", str(result["home_size"])),
            ("Filesystem label", str(result["home_label"])),
        ],
        [
            ("SSH public keys", "\n".join(key_lines) or "[dim]none[/dim]"),
            ("Key source", f'[dim]{escape(str(result["ssh_key_source"]))}[/dim]'),
            ("Root login", str(result["ssh"]["root"])),
            ("User login", str(result["ssh"]["user"])),
            (
                "SSH config",
                str(result["ssh_config_path"]).replace(str(Path.home()), "~", 1),
            ),
            *(
                [("Sync files", str(result["sync_command"]))]
                if result.get("sync_command")
                else []
            ),
        ],
    ]
    console.print()
    show_kv_panel("WORKSPACE CREATED", sections)


def gib_value(value: Any) -> str:
    if not isinstance(value, (int, float)):
        return "—"
    return f"{float(value):g} GiB"


def show_destroy_plan(plan: dict[str, Any]) -> None:
    vm_table = Table(box=None, show_header=True, pad_edge=False, collapse_padding=True)
    vm_table.add_column("VMID", style="white", no_wrap=True)
    vm_table.add_column("NAME", style="white", no_wrap=True)
    vm_table.add_column("NODE", style="grey70", no_wrap=True)
    vm_table.add_column("STATUS", no_wrap=True)
    vm_table.add_row(
        str(plan["vmid"]),
        str(plan["name"]),
        str(plan["node"]),
        ui_vm_status(plan.get("status")),
    )

    disk_table = Table(box=None, show_header=True, pad_edge=False, collapse_padding=True)
    disk_table.add_column("RESOURCE", style="grey70", no_wrap=True)
    disk_table.add_column("DISK", style="white", no_wrap=True)
    disk_table.add_column("STORAGE", style="white", no_wrap=True)
    disk_table.add_column("SIZE", style="white", no_wrap=True, justify="right")
    disk_table.add_column("DATA USED", style="white", no_wrap=True, justify="right")
    disk_table.add_column("ACTION", no_wrap=True)
    disk_table.add_row(
        "root",
        str(plan["root_disk"]),
        str(plan["root_storage"]),
        gib_value(plan.get("root_size_gib")),
        "—",
        Text("DELETE", style="red"),
    )
    disk_table.add_row(
        "home",
        str(plan["home_disk"]),
        str(plan["home_storage"]),
        gib_value(plan.get("home_size_gib")),
        byte_value(plan.get("home_used_bytes")),
        Text("DELETE PERMANENTLY", style="red"),
    )

    storage_table = Table(box=None, show_header=True, pad_edge=False, collapse_padding=True)
    storage_table.add_column("STORAGE", style="white", no_wrap=True)
    storage_table.add_column("FREE NOW", style="green", no_wrap=True, justify="right")
    storage_table.add_column("TOTAL", style="white", no_wrap=True, justify="right")
    for item in plan.get("storage_status", []):
        if not isinstance(item, dict):
            continue
        storage_table.add_row(
            str(item.get("name") or "—"),
            byte_value(item.get("available_bytes")),
            byte_value(item.get("total_bytes")),
        )

    body: list[Any] = [
        vm_table,
        Text(""),
        disk_table,
        Text(""),
        storage_table,
    ]
    console.print()
    console.print(
        Panel.fit(
            Group(*body),
            title=Text("DESTROY PLAN", style="dim bright_blue"),
            title_align="center",
            border_style="grey35",
            padding=(1, 2),
        )
    )
    console.print()


def show_destroy_result(result: dict[str, Any]) -> None:
    sections = [
        [
            ("VMID", str(result["vmid"])),
            ("Name", str(result["name"])),
            ("Status", "[dim]absent[/dim]"),
        ],
        [
            ("VM deleted", ui_check(result.get("vm_deleted"))),
            ("Persistent home deleted", ui_check(result.get("home_deleted"))),
            (
                "Local SSH config",
                (
                    f"[green]✓ {len(result.get('ssh_config_removed', []))} removed[/green]"
                    if result.get("ssh_config_removed")
                    else "[dim]no stale entry[/dim]"
                ),
            ),
            (
                "Local known_hosts",
                (
                    f"[green]✓ {len(result.get('ssh_known_hosts_removed', []))} removed[/green]"
                    if result.get("ssh_known_hosts_removed")
                    else "[dim]no stale entry[/dim]"
                ),
            ),
        ],
    ]
    console.print()
    show_kv_panel("WORKSPACE DESTROYED", sections)


def show_migrate_plan(plan: dict[str, Any]) -> None:
    volumes = [item for item in plan.get("volumes", []) if isinstance(item, dict)]
    volume_lines = format_volume_lines(volumes)
    known_sizes = [
        int(item["size_bytes"])
        for item in volumes
        if isinstance(item.get("size_bytes"), int)
    ]
    total_size = human_bytes(sum(known_sizes)) if known_sizes else "—"
    initial_status = str(plan.get("status") or "unknown")

    sections = [
        [
            ("VMID", str(plan["vmid"])),
            ("Name", str(plan["name"])),
            ("Status", ui_vm_status(plan.get("status"))),
        ],
        [
            ("Source node", str(plan["source_node"])),
            ("Target node", str(plan["target_node"])),
            ("Target storage", str(plan["target_storage"])),
        ],
    ]
    if volume_lines:
        sections.append(
            [
                ("Volumes", "\n".join(volume_lines)),
                ("Data to migrate", total_size),
            ]
        )
    sections.append(
        [
            ("Persistent home", f'{plan["home_disk"]} · {plan["home_label"]}'),
            ("Migration", "[yellow]OFFLINE[/yellow] [dim]— root + home + cloud-init[/dim]"),
            ("Power state", f"[green]PRESERVE[/green] [dim]— {escape(initial_status)} → {escape(initial_status)}[/dim]"),
            ("Snippets", "[green]COPY BEFORE MIGRATION[/green]"),
        ]
    )
    console.print()
    show_kv_panel("MIGRATE PLAN — NO CHANGES MADE YET", sections)
    console.print()


def show_refresh_plan(plan: dict[str, Any]) -> None:
    disk = (
        f'{plan["root_disk_gb"]:g}G [dim]from Gold[/dim]'
        if plan.get("root_disk_gb") is not None
        else "[dim]from Gold[/dim]"
    )
    sections = [
        [("VMID", str(plan["vmid"])), ("Name", str(plan["name"])), ("Status", ui_vm_status(plan.get("status")))],
        [
            ("Source", f'Gold VM {plan["gold_vmid"]}'),
            ("Root action", "[yellow]DELETE + FULL CLONE[/yellow]"),
            ("Root disk", f'{disk} on {plan["root_storage"]}'),
        ],
        [("IP", f'{plan["ip"]}/{plan["cidr"]}'), ("Gateway", str(plan["gateway"]))],
        [
            ("Persistent home", f'{plan["home_disk"]} on {plan["home_storage"]}'),
            ("Filesystem label", str(plan["home_label"])),
            ("Home policy", "[green]PRESERVE[/green] [dim]— never format during refresh[/dim]"),
            (
                "Power state",
                f'[green]PRESERVE[/green] [dim]— {escape(str(plan["status"]))} → {escape(str(plan["status"]))}[/dim]',
            ),
        ],
    ]
    console.print()
    show_kv_panel("REFRESH PLAN — ROOT WILL BE REPLACED", sections)
    console.print()


def show_refresh_result(result: dict[str, Any]) -> None:
    sections = [
        [("VMID", str(result["vmid"])), ("Name", str(result["name"])), ("Status", ui_vm_status(result.get("status")))],
        [
            ("Root", f'[green]refreshed[/green] from Gold {result["gold_vmid"]}'),
            ("IP", f'{result["ip"]}/{result["cidr"]}'),
            ("MAC", str(result["mac"])),
        ],
        [
            ("Persistent home", f'[green]✓ preserved[/green] {result["home_disk"]}'),
            ("Filesystem label", str(result["home_label"])),
            ("User authorized_keys", ui_check(result.get("user_authorized_keys_present"))),
            (
                "Power state",
                f'[green]✓ preserved[/green] {escape(str(result.get("status") or "unknown"))}',
            ),
        ],
        [("Root login", str(result["ssh"]["root"])), ("User login", str(result["ssh"]["user"]))],
    ]
    console.print()
    show_kv_panel("WORKSPACE REFRESHED", sections)


def byte_value(value: Any) -> str:
    parsed = integer_value(value)
    return human_bytes(parsed) if parsed is not None else "—"


def byte_value_in_unit(value: Any, unit: str, decimals: int = 0) -> str:
    parsed = integer_value(value)
    if parsed is None:
        return "—"
    factors = {
        "KiB": 1024,
        "MiB": 1024**2,
        "GiB": 1024**3,
        "TiB": 1024**4,
        "PiB": 1024**5,
    }
    factor = factors.get(unit)
    if factor is None:
        raise AppError(f"Unsupported storage display unit: {unit!r}")
    return f"{parsed / factor:.{decimals}f}"


def free_percent_color(value: float | None) -> str:
    if value is None:
        return "white"
    used_percent = 100.0 - value
    if used_percent <= 70.0:
        return "green"
    if used_percent <= 90.0:
        return "yellow"
    return "red"


def free_percent_text(value: float | None, *, decimals: int = 1) -> Text:
    if value is None:
        return Text("—", style="white")
    return Text(f"{value:.{decimals}f}%", style=free_percent_color(value))


def gib_text(value: Any, *, decimals: int = 1) -> str:
    parsed = integer_value(value)
    if parsed is None:
        return "—"
    return f"{parsed / 1024**3:.{decimals}f}"


def global_status_rows(result: dict[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    gold = result.get("gold")
    if isinstance(gold, dict):
        rows.append(gold)
    rows.extend(item for item in result.get("workspaces", []) if isinstance(item, dict))
    return rows


def global_summary_lines(result: dict[str, Any], *, narrow: bool) -> list[Text]:
    summary = result["summary"]
    homes = summary["homes"]
    count = int(summary["workspace_count"])
    running = int(summary["running"])
    stopped = int(summary["stopped"])

    if narrow:
        values = [("WS", f"{count} ({running} running)")]
    else:
        values = [("Workspaces", f"{count} ({running} running / {stopped} stopped)")]

    nodes = [item for item in summary.get("nodes", []) if isinstance(item, dict)]
    if nodes:
        node_states = ", ".join(
            f'{item.get("node") or "—"} {item.get("status") or "unknown"}'
            for item in nodes
        )
        values.append(("Nodes" if narrow else "Cluster nodes", node_states))

    orphaned = summary.get("orphaned_volumes") or []
    if orphaned:
        label = "Detached HS volumes" if not narrow else "Detached"
        values.append((label, str(len(orphaned))))

    if count:
        free_percent = homes.get("free_percent")
        free_text = f"{float(free_percent):.1f}%" if free_percent is not None else "unknown"
        used_text = byte_value(homes.get("used_bytes"))
        allocated_text = byte_value(homes.get("quota_bytes"))
        if narrow:
            values.extend(
                [
                    ("Homes", f"{used_text} used / {allocated_text} allocated"),
                    ("Home free", free_text),
                ]
            )
        else:
            values.extend(
                [
                    ("Persistent homes", f"{used_text} used / {allocated_text} allocated"),
                    ("Home filesystem free", free_text),
                ]
            )

    label_width = max(len(label) for label, _ in values)
    lines: list[Text] = []
    for label, value in values:
        line = Text()
        line.append(label.ljust(label_width), style="grey70")
        line.append(" : ", style="grey70")
        line.append(value, style="white")
        lines.append(line)
    missing = int(homes.get("missing_count") or 0)
    if missing:
        lines.append(Text(f"Missing home disks: {missing}", style="yellow"))
    return lines


def global_storage_rows(result: dict[str, Any]) -> list[dict[str, Any]]:
    summary = result.get("summary") or {}
    return [
        item
        for item in summary.get("storage_layouts", [])
        if isinstance(item, dict)
    ]


def build_global_storage_table(result: dict[str, Any], *, narrow: bool) -> Table | None:
    rows = global_storage_rows(result)
    if not rows:
        return None
    summary = result.get("summary") or {}
    unit = str(summary.get("storage_unit") or "GiB")
    decimals = int(summary.get("storage_decimals") or 0)

    table = Table(
        box=None,
        show_header=True,
        pad_edge=False,
        collapse_padding=True,
    )
    if narrow:
        table.add_column("NODE", style="grey70", no_wrap=True)
        table.add_column("STATE", no_wrap=True)
        table.add_column("STORAGE", style="white", no_wrap=True)
        table.add_column("FREE %", no_wrap=True, justify="right")
    else:
        table.add_column("LAYOUT", style="grey70", no_wrap=True)
        table.add_column("NODE", style="grey70", no_wrap=True)
        table.add_column("STATE", no_wrap=True)
        table.add_column("STORAGE", style="white", no_wrap=True)
        table.add_column(f"USED {unit}", style="white", no_wrap=True, justify="right")
        table.add_column(f"FREE {unit}", style="white", no_wrap=True, justify="right")
        table.add_column("FREE %", no_wrap=True, justify="right")

    for item in rows:
        node = str(item.get("node") or "—")
        node_status = str(item.get("node_status") or "unknown")
        if node_status == "online":
            node_status_value = Text("online", style="green")
        elif node_status == "offline":
            node_status_value = Text("offline", style="yellow")
        else:
            node_status_value = Text(node_status, style="yellow")
        storage = str(item.get("storage") or "—")
        free_bytes = item.get("available_bytes")
        free = byte_value_in_unit(free_bytes, unit, decimals)
        free_pct = free_percent(item.get("total_bytes"), free_bytes)
        if narrow:
            table.add_row(node, node_status_value, storage, free_percent_text(free_pct))
        else:
            table.add_row(
                str(item.get("layout") or "—"),
                node,
                node_status_value,
                storage,
                byte_value_in_unit(item.get("used_bytes"), unit, decimals),
                free,
                free_percent_text(free_pct),
            )
    return table


def show_global_status(result: dict[str, Any]) -> None:
    narrow = console.width < GLOBAL_STATUS_NARROW_WIDTH
    rows = global_status_rows(result)
    table = Table(
        box=None,
        show_header=not narrow,
        pad_edge=False,
        collapse_padding=True,
    )
    table.add_column("VMID", style="white", no_wrap=True, width=5)
    table.add_column("ROLE", style="grey70", no_wrap=True, width=5)
    name_width = max(8, console.width - (21 if narrow else 60))
    table.add_column("NAME", style="white", no_wrap=True, overflow="ellipsis", max_width=name_width)
    if narrow:
        table.add_column("", no_wrap=True, width=1)
    else:
        table.add_column("STATUS", no_wrap=True, width=10)
        table.add_column("IP", style="white", no_wrap=True, width=16)
        table.add_column("HOME GiB", style="white", no_wrap=True, justify="right")
        table.add_column("FREE %", no_wrap=True, justify="right")

    for item in rows:
        status = str(item.get("status") or "unknown")
        if status == "running":
            status_text = Text("●" if narrow else "● running", style="green")
        elif status == "stopped":
            status_text = Text("○" if narrow else "○ stopped", style="dim")
        else:
            status_text = Text("!" if narrow else status, style="yellow")
        name = str(item.get("name") or "—")
        if item.get("warning"):
            name = f"{name} !"
        values: list[Any] = [str(item["vmid"]), str(item["role"]), name, status_text]
        if not narrow:
            home = item.get("home")
            if isinstance(home, dict) and home.get("size_bytes") is not None:
                home_size = gib_text(home.get("size_bytes"), decimals=1)
                home_free = home.get("free_percent")
                home_free_value = float(home_free) if home_free is not None else None
            else:
                home_size = "—"
                home_free_value = None
            values.extend(
                [
                    str(item.get("ip") or "—"),
                    home_size,
                    free_percent_text(home_free_value),
                ]
            )
        table.add_row(*values)

    body: list[Any] = [table]
    body.append(Text("─" * min(max(20, console.width - 8), 72), style="grey35"))
    body.extend(global_summary_lines(result, narrow=narrow))
    storage_table = build_global_storage_table(result, narrow=narrow)
    if storage_table is not None:
        body.append(Text(""))
        body.append(storage_table)
    console.print(
        Panel.fit(
            Group(*body),
            title=Text("HOMESTACK", style="dim bright_blue"),
            title_align="center",
            border_style="grey35",
            padding=(1, 2),
        )
    )
    warnings = result.get("warnings") or []
    if warnings:
        console.print(
            Panel.fit(
                Group(*(Text(f"! {warning}", style="yellow") for warning in warnings)),
                title=Text("WARNINGS", style="yellow"),
                border_style="yellow",
                padding=(0, 1),
            )
        )


def format_volume_lines(volumes: list[dict[str, Any]]) -> list[str]:
    if not volumes:
        return []

    slot_width = max(4, *(len(str(item.get("slot") or "—")) for item in volumes))
    role_width = max(4, *(len(str(item.get("role") or "disk")) for item in volumes))
    volume_width = max(6, *(len(str(item.get("volume") or "—")) for item in volumes))

    size_texts: list[str] = []
    for item in volumes:
        size = item.get("size")
        if size not in (None, ""):
            size_texts.append(str(size))
            continue
        size_bytes = item.get("size_bytes")
        if isinstance(size_bytes, int):
            size_texts.append(human_bytes(size_bytes))
        else:
            size_texts.append("—")
    size_width = max(4, *(len(text) for text in size_texts))

    lines = [
        f"{'SLOT':<{slot_width}}  {'ROLE':<{role_width}}  {'VOLUME':<{volume_width}}  {'SIZE':>{size_width}}"
    ]
    for item, size in zip(volumes, size_texts):
        slot = str(item.get("slot") or "—")
        role = str(item.get("role") or "disk")
        volume = str(item.get("volume") or "—")
        line = (
            f"{slot:<{slot_width}}  {role:<{role_width}}  "
            f"{volume:<{volume_width}}  {size:>{size_width}}"
        )
        if role == "unused":
            line = f"[yellow]{escape(line)}[/yellow]"
        else:
            line = escape(line)
        lines.append(line)
    return lines


def show_status_result(result: dict[str, Any]) -> None:
    if not result.get("ok"):
        console.print(
            Panel(
                f"VM {result['vmid']} does not exist.",
                title=Text("WORKSPACE STATUS", style="dim bright_blue"),
                title_align="center",
                border_style="grey35",
                padding=(1, 2),
            )
        )
        return

    ubuntu = result.get("ubuntu_user_present")
    if ubuntu is True:
        ubuntu_text = "[red]✗ present[/red]"
    elif ubuntu is False:
        ubuntu_text = "[dim]— absent[/dim]"
    else:
        ubuntu_text = "[dim]unknown[/dim]"

    sections = [
        [
            ("Name", ui_value(result.get("name"))),
            ("Role", ui_value(result.get("role"))),
            ("Role tag", ui_check(result.get("role_tag_ok"))),
            ("Node", ui_value(result.get("node"))),
            ("Status", ui_vm_status(result.get("status"))),
        ],
        [
            ("Configured IP", ui_value(result.get("configured_ip"))),
            ("Actual hostname", ui_value(result.get("actual_hostname"))),
            ("Actual IP", ui_value(result.get("actual_ip"))),
            ("QEMU Guest Agent", ui_check(result.get("qga"))),
        ],
        [
            ("Expected label", ui_value(result.get("home_label"))),
            ("Disk identity", ui_check(result.get("home_identity_ok"))),
            ("Guest home", ui_value(result.get("home_mount"))),
            ("Mounted label", ui_value(result.get("mounted_home_label"))),
            ("Mount identity", ui_check(result.get("home_mount_ok"))),
            ("Ubuntu user", ubuntu_text),
        ],
        [
            ("Root login", ui_value(result.get("ssh", {}).get("root"))),
            ("User login", ui_value(result.get("ssh", {}).get("user"))),
        ],
    ]
    if result.get("role_warning"):
        sections.insert(1, [("Warning", f"[yellow]{escape(str(result['role_warning']))}[/yellow]")])

    volumes = [item for item in result.get("volumes", []) if isinstance(item, dict)]
    volume_lines = format_volume_lines(volumes)
    if volume_lines:
        sections.insert(-1, [("Volumes", "\n".join(volume_lines))])

    show_kv_panel(f"WORKSPACE STATUS — VM {result['vmid']}", sections)


def show_transport_result(result: dict[str, Any]) -> None:
    transport = result["transport"]
    pve = result["pve"]
    uid = int(transport.get("uid", -1))
    uid_text = f"[green]✓ {uid}[/green] [dim](root)[/dim]" if uid == 0 else f"[red]✗ {uid}[/red]"
    metadata = [
        (str(key).replace("_", " ").title(), escape(str(value)))
        for key, value in transport.items()
        if key not in {"type", "host", "uid"}
    ]
    sections = [
        [
            ("Transport", escape(str(transport.get("type") or "unknown"))),
            *metadata,
        ],
        [
            ("Remote host", f'[green]✓[/green] {escape(str(transport.get("host") or "—"))}'),
            ("Remote uid", uid_text),
        ],
        [
            ("PVE version", ui_value(pve.get("version"))),
            ("Kernel", f'[dim]{ui_value(pve.get("kernel"))}[/dim]'),
            ("Uptime", ui_value(pve.get("uptime"))),
        ],
    ]
    show_kv_panel("HOMESTACK TRANSPORT", sections)
