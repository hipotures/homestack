"""Plain CLI adapter; catalog listing is dispatched without remote transport."""
from __future__ import annotations

import json
import sys
from rich.console import Console
from rich.prompt import Confirm
from rich.table import Table

from .models import AppError
from .setup import build_plan, execute_plan, inspect_workspace_state, resolve_target
from .setup_catalog import load_catalog, parse_assignments, save_snapshot, select_entries
from .setup_config import FileParams, effective_entries
from .transports import open_transport
from .workspace_ssh import WorkspaceSSH


def show_catalog(cfg, catalog):
    console = Console()
    table = Table(title=f"Setup catalog {catalog.snapshot_id}")
    for heading in ("Group", "Index", "ID", "Label", "Path / repository", "Availability / timestamps"):
        table.add_column(heading)
    for row in catalog.rows(cfg):
        table.add_row(row["group"], str(row["index"]), row["id"], row["label"], row["path_or_repository"],
                      row["availability"] + (f"; created {row.get('created_at')}; pushed {row.get('pushed_at')}" if "created_at" in row else ""))
    populated = {row["group"] for row in catalog.rows(cfg)}
    for group in cfg.setup.groups:
        if group.id not in populated:
            table.add_row(group.label, "—", "—", "No entries", "", "Not configured or discovery unavailable")
    console.print(table)
    if catalog.repository_error:
        console.print(catalog.repository_error, markup=False)
    console.print("Guest installation state is unknown. Numeric selectors reuse this snapshot; use stable IDs in long-lived scripts.")


def show_plan(console, plan):
    console.print(f"Setup: {plan.target['name']} (VM {plan.target['vmid']}) — guest state unknown", markup=False)
    table = Table()
    for heading in ("Group", "ID", "Action", "Paths / prerequisites", "Dependencies"):
        table.add_column(heading)
    for action in plan.public()["actions"]:
        location = action.get("destination") or action.get("repository") or action.get("profile") or ", ".join(action.get("prerequisites", ()))
        table.add_row(action["group"], action["id"], action["label"], location, ", ".join(action["depends_on"]) or "none")
    console.print(table)


def _timestamp_text(item: dict, key: str) -> str:
    values = [
        file.get(key)
        for file in item.get("files", [])
        if isinstance(file, dict) and isinstance(file.get(key), int)
    ]
    if not values:
        return "—"
    from datetime import datetime, timezone
    return datetime.fromtimestamp(max(values) / 1_000_000_000, timezone.utc).isoformat(
        timespec="seconds"
    ).replace("+00:00", "Z")


def show_workspace_setup_status(cfg, target, state):
    console = Console()
    console.print(f"Setup status: {target['name']} (VM {target['vmid']}) {target.get('ip', '')}", markup=False)
    console.print(f"Checked: {state['checked_at']}    Registry: {state['state_path']} ({'present' if state['registry_present'] else 'not created'})", markup=False)
    table = Table()
    for heading in ("Group", "Item", "State", "Installed", "First managed", "Last applied", "Created", "Modified"):
        table.add_column(heading)
    visible = 0
    for identity, item in state["items"].items():
        label = item.get("label") or identity
        empty = item.get("state") in {
            "absent",
            "not installed",
            "not configured",
            "shell missing",
            "unknown; no installation check",
        }
        if empty and not item.get("managed") and not item.get("exists"):
            continue
        visible += 1
        table.add_row(
            item.get("group", item.get("handler", "")),
            label,
            item.get("state", "unknown"),
            item.get("installed_at") or "—",
            item.get("first_managed_at") or "—",
            item.get("last_applied_at") or "—",
            _timestamp_text(item, "birthtime_ns"),
            _timestamp_text(item, "mtime_ns"),
        )
    if visible:
        console.print(table)
    else:
        console.print("No installed, configured, modified, or HomeStack-managed setup items were detected.")


def run_setup(args, cfg, *, json_mode: bool, assume_yes: bool) -> int:
    console = Console(stderr=json_mode)
    emit = lambda data: print(json.dumps(data, indent=2))
    is_sync = args.command == "sync"
    tokens = getattr(args, "selectors", [])
    target_arg = args.target
    if target_arg == "status" and not is_sync:
        if len(tokens) != 1 or assume_yes or args.dry_run or args.non_interactive or args.catalog:
            raise AppError("Usage: homestack setup status VMID|NAME [--json]")
        status_target = tokens[0]
        catalog = load_catalog(cfg, repositories=True)
        with open_transport(cfg) as session:
            target = resolve_target(session, cfg, status_target)
        if not json_mode:
            console.print("SSH: authenticate once to inspect workspace setup state; the connection closes after status.")
        with WorkspaceSSH.configured(cfg, target) as workspace:
            state = inspect_workspace_state(workspace, cfg, target, catalog.entries)
        result = {"ok": True, "command": "setup status", "target": target, **state}
        if json_mode:
            emit(result)
        else:
            show_workspace_setup_status(cfg, target, state)
        return 0
    if target_arg == "list" and not is_sync:
        if tokens or assume_yes or args.dry_run or args.non_interactive or args.catalog:
            raise AppError("setup list is targetless discovery; execution selectors/options are not accepted")
        catalog = load_catalog(cfg, repositories=True)
        save_snapshot(cfg, catalog)
        if json_mode:
            emit({"ok": True, "command": "setup list", "catalog": catalog.snapshot_id,
                  "items": catalog.rows(cfg), "groups": [{"id": g.id, "label": g.label} for g in cfg.setup.groups], "repository_error": catalog.repository_error, "guest_state": "unknown"})
        else:
            show_catalog(cfg, catalog)
        return 0
    if not target_arg:
        raise AppError("An explicit VMID or exact workspace name is required; use setup list for discovery")
    if not tokens and args.catalog:
        raise AppError("--catalog requires explicit selectors; use setup list to inspect a catalog")
    unattended = bool(json_mode or getattr(args, "non_interactive", False) or not sys.stdin.isatty())
    if is_sync and not tokens:
        entries = tuple(e for e in effective_entries(cfg) if isinstance(e.params, FileParams))
        catalog_id = None
        if not entries:
            raise AppError("No Files configured; add [[setup.items]] file entries or legacy [sync] paths")
    elif tokens:
        assignments = parse_assignments(tokens, cfg)
        if is_sync and set(assignments) != {"files"}:
            raise AppError("sync accepts only Files selections; use setup for other groups")
        entries, catalog_id = select_entries(cfg, tokens, catalog_id=args.catalog)
    else:
        if unattended or getattr(args, "dry_run", False):
            raise AppError("No actions selected. Run setup list, then specify env=bash, app=codex or another selector")
        entries, catalog_id = (), None
    # Local selection validation always precedes even read-only target resolution.
    with open_transport(cfg) as session:
        target = resolve_target(session, cfg, target_arg)
    if not entries:
        from .setup_tui import SetupApp
        catalog = load_catalog(cfg, repositories=True)
        save_snapshot(cfg, catalog)
        console.print("SSH: authenticate once; this session remains open until setup exits.")
        with WorkspaceSSH.configured(cfg, target) as workspace:
            state = inspect_workspace_state(workspace, cfg, target, catalog.entries)
            result = SetupApp(cfg, target, catalog=catalog, workspace=workspace, state=state).run()
        return 0 if result is None or result.get("ok") else 1
    plan = build_plan(cfg, target, entries, catalog_id=catalog_id, unattended=unattended)
    if args.dry_run:
        result = {"ok": True, "dry_run": True, "plan": plan.public()}
        if json_mode:
            emit(result)
        else:
            show_plan(console, plan)
            console.print("Dry run: no guest SSH or remote changes.")
        return 0
    if not assume_yes:
        if json_mode or not sys.stdin.isatty() or args.non_interactive:
            emit({"ok": False, "confirmation_required": True, "plan": plan.public(),
                  "message": "No changes made. Re-run with --yes to accept this plan."})
            return 3
        show_plan(console, plan)
        if not Confirm.ask("Apply these selected actions?", default=False):
            console.print("Cancelled. No changes were made.")
            return 0
    if assume_yes and not json_mode:
        show_plan(console, plan)
    console.print("SSH: authenticate with the configured workspace identity; hardware authentication may require a touch.")
    result = execute_plan(cfg, plan, progress=lambda identity, state: console.print(f"{identity}: {state}", markup=False))
    if json_mode:
        emit(result)
    else:
        if result.get("snapshot", {}).get("created"):
            console.print(f"Snapshot: {result['snapshot']['path']}", markup=False)
        for item in result["results"]:
            console.print(f"{item['label']}: {item['status']} — {item['detail']}", markup=False)
    return 0 if result["ok"] else 1
