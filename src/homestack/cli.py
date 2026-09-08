"""Cli support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any
import argparse
import json
import sys
from rich.panel import Panel
from rich.progress import BarColumn, Progress, SpinnerColumn, TaskProgressColumn, TextColumn, TimeElapsedColumn
from rich.prompt import Confirm, Prompt
from rich.table import Table

from .config import default_config_path, load_config
from .lifecycle import build_create_plan, build_destroy_plan, build_migrate_plan, build_refresh_plan, create_workspace, destroy_workspace, migrate_workspace, refresh_workspace, resolve_workspace_target
from .models import AppError
from .proxmox import parse_home_size
from .status import global_status, transport_status, workspace_status
from .sync import build_sync_plan, sync_workspace
from .transports import open_transport
from .ui import console, show_create_plan, show_destroy_plan, show_destroy_result, show_error, show_global_status, show_migrate_plan, show_refresh_plan, show_repository_menu, show_repository_status, show_status_result, show_sync_plan, show_sync_result, show_transport_result

def emit_json(obj: dict[str, Any]) -> None:
    print(json.dumps(obj, ensure_ascii=False, indent=2, sort_keys=True))


def show_help(cfg_path: Path) -> None:
    cmd = "homestack"

    commands = Table(title="Commands", show_header=True)
    commands.add_column("Command")
    commands.add_column("Description")
    commands.add_row(
        f"{cmd} discover",
        "Read-only discovery of local Herdr sessions and reachable Proxmox environments; no config required.",
    )
    commands.add_row(
        f"{cmd} install",
        "Interactively discover Proxmox and write the runtime configuration.",
    )
    commands.add_row(
        f"{cmd} create VMID NAME",
        "Create a full clone from Gold with persistent ext4 home disk.",
    )
    commands.add_row(
        f"{cmd} refresh VMID|NAME",
        "Replace only the VM root with a fresh full clone from Gold; preserve persistent home.",
    )
    commands.add_row(
        f"{cmd} destroy VMID|NAME",
        "Permanently delete the VM, snippets and persistent home disk.",
    )
    commands.add_row(
        f"{cmd} migrate VMID|NAME NODE --target-storage STORAGE",
        "Offline-migrate root, persistent home and cloud-init disk; copy snippets first.",
    )
    commands.add_row(
        f"{cmd} sync VMID|NAME",
        "Synchronize configured desktop files/directories into the workspace persistent home.",
    )
    commands.add_row(
        f"{cmd} repo VMID|NAME [OWNER/REPO]",
        "Inspect, set up, or rotate the GitHub repository deploy key for a workspace.",
    )
    commands.add_row(
        f"{cmd} status",
        "Show the global HomeStack VM and storage dashboard.",
    )
    commands.add_row(
        f"{cmd} status VMID|NAME",
        "Show VM, network, QGA, persistent home disk and SSH readiness.",
    )
    commands.add_row(
        f"{cmd} transport",
        "Verify the configured transport, authenticated root access, and Proxmox access.",
    )

    options = Table(title="Options", show_header=True)
    options.add_column("Option")
    options.add_column("Description")
    options.add_row("--home-size SIZE", "Override the configured persistent home disk size for create.")
    options.add_row(
        "--storage STORAGE",
        "Create root and persistent home on an allowed storage instead of the layout default.",
    )
    options.add_row("-y, --yes", "Skip interactive confirmation for create, refresh, migrate, sync or destroy.")
    options.add_row(
        "--json",
        "Return JSON. Commands requiring confirmation return the resolved plan without --yes.",
    )
    options.add_row("--config PATH", f"Configuration file. Default: {cfg_path}")
    options.add_row("-h, --help", "Show this help screen.")

    examples = (
        f"{cmd} discover\n"
        f"{cmd} transport\n"
        f"{cmd} create 200 example-workspace\n"
        f"{cmd} create 200 example-workspace --storage example-storage\n"
        f"{cmd} refresh 200\n"
        f"{cmd} migrate 200 pve-example-2 --target-storage example-storage\n"
        f"{cmd} sync 200\n"
        f"{cmd} sync example-workspace\n"
        f"{cmd} repo example-workspace\n"
        f"{cmd} destroy 200\n"
        f"{cmd} destroy 200 --json\n"
        f"{cmd} status\n"
        f"{cmd} status 200"
    )

    console.print(Panel.fit("HOMESTACK", title="HELP"))
    console.print(commands)
    console.print(options)
    console.print(Panel(examples, title="Examples"))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("-h", "--help", action="store_true")
    parser.add_argument("--config", type=Path, default=default_config_path())
    parser.add_argument("--json", action="store_true", dest="global_json")
    parser.add_argument("-y", "--yes", action="store_true", dest="global_yes")

    sub = parser.add_subparsers(dest="command")

    discover = sub.add_parser("discover", add_help=False)
    discover.add_argument("--json", action="store_true")
    discover.add_argument("-h", "--help", action="store_true", dest="sub_help")

    install = sub.add_parser("install", add_help=False)
    install.add_argument("-h", "--help", action="store_true", dest="sub_help")

    create = sub.add_parser("create", add_help=False)
    create.add_argument("vmid", type=int)
    create.add_argument("name")
    create.add_argument("--home-size")
    create.add_argument("--storage")
    create.add_argument("--json", action="store_true")
    create.add_argument("-y", "--yes", action="store_true")
    create.add_argument("-h", "--help", action="store_true", dest="sub_help")

    refresh = sub.add_parser("refresh", add_help=False)
    refresh.add_argument("target")
    refresh.add_argument("--json", action="store_true")
    refresh.add_argument("-y", "--yes", action="store_true")
    refresh.add_argument("-h", "--help", action="store_true", dest="sub_help")

    destroy = sub.add_parser("destroy", add_help=False)
    destroy.add_argument("target")
    destroy.add_argument("--json", action="store_true")
    destroy.add_argument("-y", "--yes", action="store_true")
    destroy.add_argument("-h", "--help", action="store_true", dest="sub_help")

    migrate = sub.add_parser("migrate", add_help=False)
    migrate.add_argument("target")
    migrate.add_argument("target_node")
    migrate.add_argument("--target-storage", required=True)
    migrate.add_argument("--json", action="store_true")
    migrate.add_argument("-y", "--yes", action="store_true")
    migrate.add_argument("-h", "--help", action="store_true", dest="sub_help")

    sync = sub.add_parser("sync", add_help=False)
    sync.add_argument("target")
    sync.add_argument("--json", action="store_true")
    sync.add_argument("-y", "--yes", action="store_true")
    sync.add_argument("-h", "--help", action="store_true", dest="sub_help")

    repo = sub.add_parser("repo", add_help=False)
    repo.add_argument("target")
    repo.add_argument("repository", nargs="?")
    repo.add_argument("--json", action="store_true")
    repo.add_argument("-h", "--help", action="store_true", dest="sub_help")

    status = sub.add_parser("status", add_help=False)
    status.add_argument("target", nargs="?")
    status.add_argument("--json", action="store_true")
    status.add_argument("-h", "--help", action="store_true", dest="sub_help")

    transport = sub.add_parser("transport", add_help=False)
    transport.add_argument("--json", action="store_true")
    transport.add_argument("-h", "--help", action="store_true", dest="sub_help")

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    if args.help or args.command is None or getattr(args, "sub_help", False):
        show_help(args.config)
        return 0

    json_mode = bool(args.global_json or getattr(args, "json", False))

    try:
        if args.command == "discover":
            from .discovery import discover_environment, show_discovery_report

            result = discover_environment()
            if json_mode:
                emit_json(result)
            else:
                show_discovery_report(result)
            return 0 if result.get("ok") else 1

        if args.command == "install":
            from .install import run_installer

            return run_installer(args.config)

        cfg = load_config(args.config)
        with open_transport(cfg) as session:

            if args.command == "transport":
                result = transport_status(session, cfg)
                if json_mode:
                    emit_json(result)
                else:
                    show_transport_result(result)
                return 0

            if args.command == "create":
                home_size = args.home_size or cfg.default_home_size
                parse_home_size(home_size)

                plan = build_create_plan(
                    session,
                    cfg,
                    args.vmid,
                    args.name,
                    home_size,
                    storage=args.storage,
                )
                assume_yes = bool(args.global_yes or getattr(args, "yes", False))
                if not assume_yes:
                    if json_mode:
                        emit_json(
                            {
                                "ok": False,
                                "confirmation_required": True,
                                "message": "No changes made. Re-run with --yes to execute this plan.",
                                "plan": plan,
                            }
                        )
                        return 3
                    show_create_plan(plan)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive confirmation requires a TTY; use --yes for automation"
                        )
                    if not Confirm.ask("Create this workspace?", default=False):
                        console.print("[bold]Cancelled. No changes were made.[/bold]")
                        return 0

                result = create_workspace(session, cfg, plan, json_mode=json_mode)
                if json_mode:
                    emit_json(result)
                return 0

            if args.command == "refresh":
                vmid = resolve_workspace_target(session, cfg, args.target)
                plan = build_refresh_plan(session, cfg, vmid)
                assume_yes = bool(args.global_yes or getattr(args, "yes", False))
                if not assume_yes:
                    if json_mode:
                        emit_json(
                            {
                                "ok": False,
                                "confirmation_required": True,
                                "message": "No changes made. Re-run with --yes to replace the VM root.",
                                "plan": plan,
                            }
                        )
                        return 3
                    show_refresh_plan(plan)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive confirmation requires a TTY; use --yes for automation"
                        )
                    if not Confirm.ask("Refresh this workspace root?", default=False):
                        console.print("[bold]Cancelled. No changes were made.[/bold]")
                        return 0

                result = refresh_workspace(session, cfg, plan, json_mode=json_mode)
                if json_mode:
                    emit_json(result)
                return 0

            if args.command == "destroy":
                vmid = resolve_workspace_target(session, cfg, args.target)
                plan = build_destroy_plan(session, cfg, vmid)
                assume_yes = bool(args.global_yes or getattr(args, "yes", False))
                if not assume_yes:
                    if json_mode:
                        emit_json(
                            {
                                "ok": False,
                                "confirmation_required": True,
                                "message": (
                                    "No changes made. Re-run with --yes to permanently delete the "
                                    "workspace and persistent home."
                                ),
                                "plan": plan,
                            }
                        )
                        return 3
                    show_destroy_plan(plan)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive confirmation requires a TTY; use --yes for automation"
                        )
                    if not Confirm.ask(
                        "Permanently destroy this workspace INCLUDING its persistent home?",
                        default=False,
                    ):
                        console.print("[bold]Cancelled. No changes were made.[/bold]")
                        return 0

                result = destroy_workspace(session, cfg, plan)
                if json_mode:
                    emit_json(result)
                else:
                    show_destroy_result(result)
                return 0

            if args.command == "migrate":
                plan = build_migrate_plan(
                    session,
                    cfg,
                    resolve_workspace_target(session, cfg, args.target),
                    args.target_node,
                    args.target_storage,
                )
                assume_yes = bool(args.global_yes or getattr(args, "yes", False))
                if not assume_yes:
                    if json_mode:
                        emit_json(
                            {
                                "ok": False,
                                "confirmation_required": True,
                                "message": "No changes made. Re-run with --yes to execute migration.",
                                "plan": plan,
                            }
                        )
                        return 3
                    show_migrate_plan(plan)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive confirmation requires a TTY; use --yes for automation"
                        )
                    if not Confirm.ask("Migrate this workspace?", default=False):
                        console.print("[bold]Cancelled. No changes were made.[/bold]")
                        return 0

                result = migrate_workspace(session, cfg, plan, json_mode=json_mode)
                if json_mode:
                    emit_json(result)
                return 0

            if args.command == "sync":
                vmid = resolve_workspace_target(session, cfg, args.target)
                plan = build_sync_plan(session, cfg, vmid)
                assume_yes = bool(args.global_yes or getattr(args, "yes", False))
                if not assume_yes:
                    if json_mode:
                        emit_json(
                            {
                                "ok": False,
                                "confirmation_required": True,
                                "message": "No changes made. Re-run with --yes to execute sync.",
                                "plan": plan,
                            }
                        )
                        return 3
                    show_sync_plan(plan)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive confirmation requires a TTY; use --yes for automation"
                        )
                    if not Confirm.ask("Synchronize these workspace files?", default=False):
                        console.print("[bold]Cancelled. No changes were made.[/bold]")
                        return 0

                result = sync_workspace(cfg, plan, json_mode=json_mode)
                if json_mode:
                    emit_json(result)
                else:
                    show_sync_result(result)
                return 0 if result.get("ok") else 1

            if args.command == "repo":
                from .repo import (
                    inspect_repository,
                    open_repository_workspace,
                    resolve_repository_argument,
                    rotate_repository_key,
                    setup_repository,
                )

                repository = resolve_repository_argument(cfg, args.repository)
                vmid, workspace_name, workspace_connection = open_repository_workspace(
                    session, cfg, args.target
                )
                with workspace_connection as repo_workspace:
                    state = inspect_repository(
                        cfg, repo_workspace, vmid, workspace_name, repository
                    )
                    if json_mode:
                        emit_json(state)
                        return 0

                    show_repository_status(state)
                    if not sys.stdin.isatty():
                        raise AppError(
                            "Interactive repository action selection requires a TTY; "
                            "use --json for status-only output"
                        )
                    show_repository_menu()
                    action = Prompt.ask("Action", choices=["1", "2", "3"], default="1")
                    if action == "1":
                        return 0
                    if action == "2":
                        result = setup_repository(cfg, repo_workspace, state)
                    else:
                        result = rotate_repository_key(cfg, repo_workspace, state)
                    if result.get("message"):
                        console.print(f"[bold]{result['message']}[/bold]")
                    show_repository_status(result)
                    return 0 if result.get("ready") else 1

            if args.command == "status":
                if args.target is None:
                    if json_mode:
                        result = global_status(session, cfg)
                    else:
                        status_progress = Progress(
                            SpinnerColumn(),
                            TextColumn("[bold]{task.description}"),
                            BarColumn(),
                            TaskProgressColumn(),
                            TimeElapsedColumn(),
                            console=console,
                            transient=True,
                            refresh_per_second=4,
                        )
                        with status_progress:
                            status_task = status_progress.add_task(
                                "Load cluster inventory",
                                total=100,
                            )

                            def update_status_progress(description: str, fraction: float) -> None:
                                status_progress.update(
                                    status_task,
                                    description=description,
                                    completed=100.0 * max(0.0, min(1.0, fraction)),
                                )

                            result = global_status(
                                session,
                                cfg,
                                progress=update_status_progress,
                            )
                else:
                    vmid = resolve_workspace_target(session, cfg, args.target)
                    result = workspace_status(session, cfg, vmid)
                if json_mode:
                    emit_json(result)
                elif args.target is None:
                    show_global_status(result)
                else:
                    show_status_result(result)
                return 0

        raise AppError(f"Unknown command: {args.command}")

    except AppError as exc:
        if json_mode:
            emit_json({"ok": False, "command": args.command, "error": str(exc)})
        else:
            show_error(str(exc))
        return 1
    except KeyboardInterrupt:
        if json_mode:
            emit_json({"ok": False, "command": args.command, "error": "Interrupted"})
        else:
            console.print("\n[bold]Interrupted.[/bold]")
        return 130
