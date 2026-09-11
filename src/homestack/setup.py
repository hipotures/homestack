"""Shared setup planning, complete preflight and ordered execution for CLI/TUI."""
from __future__ import annotations

from contextlib import ExitStack
from dataclasses import dataclass
from datetime import datetime, timezone
import base64
import hashlib
import json
import os
import stat
from pathlib import Path
import shlex
import shutil
from typing import Callable

from .config import Config
from .guest import derive_ip
from .lifecycle import resolve_workspace_target
from .models import AppError
from . import repo
from .setup_config import ApplicationParams, Entry, FileParams, EnvironmentParams, RepositoryParams
from .workspace_ssh import WorkspaceSSH

ORDER = {"environment": 0, "file": 1, "application": 2, "repository": 3}


@dataclass(frozen=True)
class Plan:
    target: dict
    entries: tuple[Entry, ...]
    catalog_id: str | None = None
    unattended: bool = False

    def public(self) -> dict:
        def describe(entry):
            p = entry.params
            extra = {}
            if isinstance(p, FileParams):
                extra = {"source": p.path, "destination": self.target.get("home", "") + "/" + p.path[2:]}
            elif isinstance(p, EnvironmentParams):
                extra = {"profile": p.profile}
            elif isinstance(p, RepositoryParams):
                extra = {"repository": p.repository}
            elif isinstance(p, ApplicationParams):
                extra = {"interpreter": p.interpreter, "interaction": p.interaction,
                         "prerequisites": p.prerequisites, "bin_dirs": p.bin_dirs,
                         "backup_paths": p.backup_paths,
                         "installation_check": "configured" if p.check else "unknown"}
            return {"id": entry.id, "group": entry.group, "label": entry.label,
                    "description": entry.description, "handler": entry.handler,
                    "depends_on": entry.depends_on, "remote_state": "unknown", **extra}
        return {"command": "setup", "target": self.target, "catalog": self.catalog_id,
                "remote_state": "unknown", "non_interactive": self.unattended,
                "actions": [describe(e) for e in self.entries]}


def resolve_target(session, cfg: Config, target: str) -> dict:
    vmid = resolve_workspace_target(session, cfg, target)
    info = repo.repository_workspace_info(session, cfg, vmid)
    if info["status"] != "running":
        raise AppError("Setup requires a running workspace; lifecycle operations must be performed separately")
    return {**info, "ip": derive_ip(cfg, vmid), "user": cfg.user_name, "home": f"/home/{cfg.user_name}"}


def _digest_local_path(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    if path.is_file():
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest(), path.stat().st_size
    if not path.is_dir():
        raise AppError(f"Managed source path is neither a file nor directory: {path}")
    total = 0
    for root, directories, files in os.walk(path, followlinks=False):
        directories.sort()
        files.sort()
        base = Path(root)
        for name in directories:
            child = base / name
            if child.is_symlink():
                raise AppError(f"Managed source directory contains a symlink: {child}")
            digest.update(b"D\0" + child.relative_to(path).as_posix().encode() + b"\0")
        for name in files:
            child = base / name
            info = child.lstat()
            if child.is_symlink() or not stat.S_ISREG(info.st_mode):
                raise AppError(f"Managed source directory contains a symlink or special file: {child}")
            digest.update(b"F\0" + child.relative_to(path).as_posix().encode() + b"\0")
            with child.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                    digest.update(chunk)
                    total += len(chunk)
    return digest.hexdigest(), total


def local_metadata(path: Path) -> dict:
    digest, size = _digest_local_path(path)
    info = path.stat()
    metadata = {
        "exists": True,
        "type": "directory" if path.is_dir() else "file",
        "sha256": digest,
        "size": size,
        "mtime_ns": info.st_mtime_ns,
    }
    birthtime_ns = getattr(info, "st_birthtime_ns", None)
    if isinstance(birthtime_ns, int) and birthtime_ns > 0:
        metadata["birthtime_ns"] = birthtime_ns
    return metadata


def _remote_metadata(ws, cfg: Config, paths: list[str]) -> dict[str, dict]:
    if not paths:
        return {}
    result = guest(ws, cfg, "metadata", paths=list(dict.fromkeys(paths)))
    return {item["path"]: item for item in result.get("items", []) if isinstance(item, dict) and isinstance(item.get("path"), str)}


def inspect_workspace_state(ws, cfg: Config, target: dict, entries: tuple[Entry, ...]) -> dict:
    """Read current setup state without modifying the workspace."""
    require_tool(ws, cfg, "python3")
    guest(ws, cfg, "identity", user=cfg.user_name, uid=cfg.user_uid, gid=cfg.user_gid,
          name=target["name"], vmid=target["vmid"])
    state_doc = guest(ws, cfg, "state-read", vmid=target["vmid"], name=target["name"])
    registry = state_doc.get("registry") or {}
    registered = registry.get("items", {}) if isinstance(registry, dict) else {}

    metadata_paths: list[str] = []
    repositories: list[str] = []
    for entry in entries:
        if isinstance(entry.params, (FileParams, EnvironmentParams)):
            metadata_paths.extend(write_paths(cfg, entry))
        elif isinstance(entry.params, ApplicationParams):
            metadata_paths.extend(path[2:].rstrip("/") for path in entry.params.backup_paths)
        elif isinstance(entry.params, RepositoryParams):
            repositories.append(entry.params.repository)
    remote_metadata = _remote_metadata(ws, cfg, metadata_paths)
    repo_states = {}
    if repositories:
        checkout_root = cfg.repo_checkout_root[2:].rstrip("/")
        repo_states = guest(ws, cfg, "repositories", checkout_root=checkout_root, repositories=repositories).get("repositories", {})

    items = {}
    for entry in entries:
        record = registered.get(entry.id, {}) if isinstance(registered, dict) else {}
        if not isinstance(record, dict):
            record = {}
        item = {
            "id": entry.id,
            "group": entry.group,
            "label": entry.label,
            "handler": entry.handler,
            "state": "unknown",
            "ready": False,
            "exists": False,
            "will_overwrite": False,
            "managed": bool(record),
            "first_managed_at": record.get("first_managed_at"),
            "last_applied_at": record.get("last_applied_at"),
            "installed_at": record.get("installed_at"),
            "last_snapshot": record.get("last_snapshot"),
            "files": [],
            "detail": "",
        }
        p = entry.params
        try:
            if isinstance(p, FileParams):
                source = source_item(cfg, entry)
                local = local_metadata(Path(source["local_path"]))
                remote = remote_metadata.get(source["relative"], {"path": source["relative"], "exists": False})
                item["files"] = [remote]
                item["exists"] = bool(remote.get("exists"))
                item["ready"] = bool(remote.get("exists") and remote.get("type") == local["type"] and remote.get("sha256") == local["sha256"])
                item["will_overwrite"] = item["exists"] and not item["ready"]
                item["state"] = "in sync" if item["ready"] else ("modified" if item["exists"] else "absent")
                item["source"] = local
            elif isinstance(p, EnvironmentParams):
                shell = ws.run(command_environment(cfg, "command -v " + shlex.quote(p.profile) + " >/dev/null", interpreter="sh", pipefail=False), check=False)
                existing = [remote_metadata[path] for path in write_paths(cfg, entry) if remote_metadata.get(path, {}).get("exists")]
                item["files"] = existing
                item["exists"] = bool(existing)
                # Existing shell files are an overwrite risk even if deeper
                # inspection later fails (for example a malformed managed block).
                item["will_overwrite"] = bool(existing)
                if shell.returncode:
                    item["state"] = "shell missing"
                    item["detail"] = f"Missing workspace executable {p.profile}"
                    item["will_overwrite"] = bool(existing)
                else:
                    desired = guest(ws, cfg, "environment", profile=p.profile, bins=all_bins(cfg))
                    changed = list(desired.get("changed", []))
                    item["ready"] = not changed
                    item["will_overwrite"] = any(remote_metadata.get(path, {}).get("exists") for path in changed)
                    item["state"] = "configured" if item["ready"] else ("needs update" if item["will_overwrite"] else "not configured")
                    item["managed_paths"] = list(desired.get("paths", []))
            elif isinstance(p, ApplicationParams):
                if p.check:
                    checked = ws.run(command_environment(cfg, p.check, interpreter=p.interpreter, bins=p.bin_dirs), check=False)
                    if checked.returncode == 255:
                        raise AppError("Workspace SSH transport failed during application inspection")
                    item["ready"] = checked.returncode == 0
                    item["exists"] = item["ready"]
                    item["state"] = "installed" if item["ready"] else "not installed"
                elif record:
                    item["state"] = "managed; verification unavailable"
                else:
                    item["state"] = "unknown; no installation check"
                backup = [remote_metadata[path[2:].rstrip("/")] for path in p.backup_paths
                          if remote_metadata.get(path[2:].rstrip("/"), {}).get("exists")]
                item["files"] = backup
                item["will_overwrite"] = item["ready"] or bool(backup)
                if p.backup_paths:
                    item["backup_paths"] = list(p.backup_paths)
            elif isinstance(p, RepositoryParams):
                live = repo_states.get(p.repository, {}) if isinstance(repo_states, dict) else {}
                item.update({k: live.get(k) for k in ("state", "ready", "exists", "detail", "remote", "key_pair") if k in live})
                if live.get("metadata"):
                    item["files"] = [live["metadata"]]
                item["will_overwrite"] = bool(live.get("exists"))
        except AppError as exc:
            item["state"] = "unavailable"
            item["detail"] = str(exc)
        items[entry.id] = item
    return {
        "ok": True,
        "checked_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "state_path": state_doc.get("state_path", "~/.local/state/homestack/setup.json"),
        "registry_present": bool(registry),
        "items": items,
    }


def backup_paths_for_entry(cfg: Config, entry: Entry, state: dict) -> tuple[str, ...]:
    p = entry.params
    if isinstance(p, FileParams):
        return write_paths(cfg, entry) if state.get("changed", True) else ()
    if isinstance(p, EnvironmentParams):
        return tuple(state.get("changed", ()))
    if isinstance(p, ApplicationParams):
        return tuple(path[2:].rstrip("/") for path in p.backup_paths)
    if isinstance(p, RepositoryParams):
        repository = state.get("repository", {})
        actions = state.get("actions", ())
        if repository.get("checkout_state") == "ready" and any(action in actions for action in ("set-origin", "set-ssh-command")):
            checkout = str(repository["checkout"]).removeprefix(f"/home/{cfg.user_name}/")
            return (checkout.rstrip("/") + "/.git/config",)
    return ()


def record_entry_state(ws, cfg: Config, plan: Plan, entry: Entry, state: dict, *, snapshot_id: str | None) -> dict:
    paths: tuple[str, ...] = ()
    if isinstance(entry.params, FileParams):
        paths = write_paths(cfg, entry)
    elif isinstance(entry.params, EnvironmentParams):
        paths = tuple(state.get("paths", ()))
    values = {
        "vmid": plan.target["vmid"],
        "name": plan.target["name"],
        "id": entry.id,
        "handler": entry.handler,
        "paths": list(paths),
        "snapshot": snapshot_id,
    }
    if isinstance(entry.params, ApplicationParams):
        values["installed"] = not bool(state.get("installed"))
    if isinstance(entry.params, RepositoryParams):
        values["repository"] = entry.params.repository
    return guest(ws, cfg, "state-record", **values)


def source_item(cfg: Config, entry: Entry) -> dict:
    from .sync import sync_plan_item
    item = sync_plan_item(cfg, entry.params.path)
    if item["status"] != "ready":
        raise AppError(f"Selected file {entry.id}: {item['status']} ({item['detail']})")
    source = Path(item["local_path"])
    home = Path.home()
    current = home
    for part in item["relative"].split("/"):
        current /= part
        if current.is_symlink():
            raise AppError(f"Selected file {entry.id}: source ancestor is a symlink")
    if not os.access(source, os.R_OK | (os.X_OK if item["is_directory"] else 0)):
        raise AppError(f"Selected file {entry.id}: source is not readable")
    if item["is_directory"]:
        import stat
        def unreadable(error):
            raise AppError(f"Selected file {entry.id}: source tree cannot be inspected") from error
        for root, directories, files in os.walk(source, followlinks=False, onerror=unreadable):
            for name in directories + files:
                child = Path(root) / name
                if child.is_symlink() or not (stat.S_ISREG(child.lstat().st_mode) or child.is_dir()):
                    raise AppError(f"Selected file {entry.id}: source tree contains a symlink or special file")
                if not os.access(child, os.R_OK | (os.X_OK if child.is_dir() else 0)):
                    raise AppError(f"Selected file {entry.id}: source tree is not readable")
    return item


def write_paths(cfg: Config, entry: Entry) -> tuple[str, ...]:
    p = entry.params
    if isinstance(p, FileParams):
        return (p.path[2:].rstrip("/"),)
    if isinstance(p, EnvironmentParams):
        return {"bash": (".bashrc", ".profile", ".bash_profile", ".bash_login"), "zsh": (".zshenv", ".zshrc"),
                "fish": (".config/fish/conf.d/homestack.fish",), "nu": (".config/nushell/env.nu", ".config/nushell/config.nu")}[p.profile]
    if isinstance(p, RepositoryParams):
        return tuple(path.removeprefix(f"/home/{cfg.user_name}/") for path in repo.repository_paths(cfg, p.repository))
    return ()


def build_plan(cfg: Config, target: dict, entries: tuple[Entry, ...], *, catalog_id: str | None = None, unattended: bool = False) -> Plan:
    if not entries or not target.get("vmid") or not target.get("name"):
        raise AppError("An explicit resolved workspace and at least one action are required")
    if cfg.user_uid <= 0 or cfg.user_gid <= 0 or cfg.user_name == "root":
        raise AppError("Setup requires an unprivileged configured workspace user")
    selected = {e.id: e for e in entries}
    if len(selected) != len(entries):
        entries = tuple(selected.values())
    ordered: list[Entry] = []
    visiting = set()
    def visit(entry):
        if entry in ordered:
            return
        if entry.id in visiting:
            raise AppError("Selected setup dependencies form a cycle")
        visiting.add(entry.id)
        for dependency in entry.depends_on:
            if dependency not in selected:
                raise AppError(f"{entry.id} requires explicit selection of {dependency}; no actions were added")
            visit(selected[dependency])
        visiting.remove(entry.id)
        ordered.append(entry)
    for entry in sorted(entries, key=lambda e: ORDER[e.handler]):
        visit(entry)
    writes = []
    for entry in ordered:
        p = entry.params
        if isinstance(p, ApplicationParams) and unattended and p.interaction == "interactive" and not p.non_interactive:
            raise AppError(f"{entry.id} requires an interactive terminal; configure a verified non_interactive recipe or omit it")
        if isinstance(p, FileParams):
            source_item(cfg, entry)
        for path in write_paths(cfg, entry):
            for previous, other in writes:
                if path == previous or path.startswith(previous + "/") or previous.startswith(path + "/"):
                    raise AppError(f"Overlapping selected writes: {other} and {entry.id} at ~/{path}")
            writes.append((path, entry.id))
    return Plan(target, tuple(ordered), catalog_id, unattended)


def command_environment(cfg: Config, command: str, *, interpreter: str = "bash", bins: tuple[str, ...] = (), pipefail: bool = True) -> str:
    home = f"/home/{cfg.user_name}"
    directories = list(dict.fromkeys([home + "/.local/bin", *(home + "/" + p[2:] for p in bins)]))
    path = ":".join([*directories, "/usr/local/bin", "/usr/bin", "/bin"])
    args = ["env", "-i", f"HOME={home}", f"USER={cfg.user_name}", f"LOGNAME={cfg.user_name}", f"PATH={path}",
            "TERM=xterm-256color", "LANG=C.UTF-8", "SHELL=/bin/" + interpreter, interpreter]
    if interpreter == "bash":
        args += ["--noprofile", "--norc"]
    elif interpreter == "zsh":
        args += ["-f"]
    if pipefail:
        args += ["-e", "-o", "pipefail"]
    args += ["-c", f"cd {shlex.quote(home)}\numask 077\n" + command]
    return shlex.join(args)


def guest(ws, cfg: Config, operation: str, **values) -> dict:
    source = Path(__file__).with_name("setup_guest.py").read_text()
    payload = json.dumps({"home": f"/home/{cfg.user_name}", "operation": operation, **values})
    code = "import base64; exec(base64.b64decode(" + repr(base64.b64encode(source.encode()).decode()) + ")); main(" + repr(payload) + ")"
    result = ws.run(command_environment(cfg, "python3 -c " + shlex.quote(code), interpreter="sh", pipefail=False))
    try:
        data = json.loads(result.stdout)
    except (ValueError, TypeError) as exc:
        raise AppError("Guest operation returned invalid data; output withheld") from exc
    if not isinstance(data, dict):
        raise AppError("Guest operation returned invalid data; output withheld")
    if not data.get("ok"):
        raise AppError(data.get("error", "Guest operation failed"))
    return data


def require_tool(ws, cfg: Config, tool: str, bins: tuple[str, ...] = ()) -> None:
    result = ws.run(command_environment(cfg, "command -v " + shlex.quote(tool) + " >/dev/null", interpreter="sh", bins=bins, pipefail=False), check=False)
    if result.returncode == 255:
        raise AppError("Workspace SSH transport failed during prerequisite inspection")
    if result.returncode:
        raise AppError(f"Missing workspace executable {tool}; prepare Gold and refresh separately")


def all_bins(cfg: Config) -> list[str]:
    return list(dict.fromkeys(["~/.local/bin", *(p for e in cfg.setup.items if isinstance(e.params, ApplicationParams) for p in e.params.bin_dirs)]))


def preflight_entry(ws, cfg: Config, plan: Plan, entry: Entry) -> dict:
    p = entry.params
    if isinstance(p, FileParams):
        if shutil.which("rsync") is None:
            raise AppError("Required desktop rsync was not found")
        item = source_item(cfg, entry)
        require_tool(ws, cfg, "rsync")
        guest(ws, cfg, "file", relative=item["relative"], directory=item["is_directory"])
        remote = _remote_metadata(ws, cfg, [item["relative"]]).get(item["relative"], {"exists": False})
        local = local_metadata(Path(item["local_path"]))
        changed = not (remote.get("exists") and remote.get("type") == local["type"] and remote.get("sha256") == local["sha256"])
        return {"file": item, "remote": remote, "local": local, "changed": changed}
    if isinstance(p, EnvironmentParams):
        require_tool(ws, cfg, p.profile)
        return guest(ws, cfg, "environment", profile=p.profile, bins=all_bins(cfg))
    if isinstance(p, ApplicationParams):
        require_tool(ws, cfg, p.interpreter, p.bin_dirs)
        installed = False
        if p.check:
            result = ws.run(command_environment(cfg, p.check, interpreter=p.interpreter, bins=p.bin_dirs), check=False)
            if result.returncode == 255:
                raise AppError("Workspace SSH transport failed during installation check")
            installed = result.returncode == 0
        guest(ws, cfg, "paths", paths=[{"relative": path[2:], "directory": True} for path in p.bin_dirs] +
              [{"relative": path[2:].rsplit("/", 1)[0], "directory": True} for path in p.requires_absent if "/" in path[2:]] +
              [{"relative": path[2:].rstrip("/"), "directory": path.endswith("/")} for path in p.backup_paths])
        if not installed:
            for path in p.requires_absent:
                result = ws.run(command_environment(cfg, "test ! -e " + shlex.quote(f"/home/{cfg.user_name}/" + path[2:]) + " && test ! -L " + shlex.quote(f"/home/{cfg.user_name}/" + path[2:]), interpreter="sh", pipefail=False), check=False)
                if result.returncode:
                    raise AppError(f"{entry.id}: existing installation state needs manual inspection; refusing to replace it")
        for tool in p.prerequisites:
            require_tool(ws, cfg, tool, p.bin_dirs)
        for index, command in enumerate(p.prerequisite_checks, 1):
            if ws.run(command_environment(cfg, command, interpreter=p.interpreter, bins=p.bin_dirs), check=False).returncode:
                raise AppError(f"{entry.id}: prerequisite check {index} failed; inspect the recipe requirements and prepare Gold separately")
        return {"installed": installed, "ready": installed}
    if isinstance(p, RepositoryParams):
        metadata = repo._github_json([f"repos/{p.repository}"], dict)
        if metadata.get("full_name", "").casefold() != p.repository.casefold() or not metadata.get("permissions", {}).get("admin") or metadata.get("archived") or metadata.get("disabled"):
            raise AppError(f"{entry.id}: repository identity or administration permission changed; reselect explicitly")
        checkout, private, public = repo.repository_paths(cfg, p.repository)
        relative_checkout = checkout.removeprefix(f"/home/{cfg.user_name}/")
        guest(ws, cfg, "paths", paths=[{"relative": x.removeprefix(f"/home/{cfg.user_name}/"), "directory": x == checkout} for x in (checkout, private, public)] +
              [{"relative": relative_checkout + "/.git", "directory": True},
               {"relative": relative_checkout + "/.git/config"}])
        state = repo.inspect_repository(cfg, ws, plan.target["vmid"], plan.target["name"], p.repository, verify_access=False)
        repo._require_tools(state)
        actions = repo.repository_setup_actions(state)
        if any(a in actions for a in ("replace-missing-private-key", "replace-read-only-key")):
            raise AppError(f"{entry.id}: key repair requires standalone repo maintenance; setup never rotates keys")
        return {"repository": state, "actions": repo.repository_setup_actions(state)}
    raise AppError("Unknown setup action handler")


def apply_entry(ws, cfg: Config, plan: Plan, entry: Entry, state: dict, terminal: Callable, *, progress=lambda identity, state: None):
    p = entry.params
    if isinstance(p, EnvironmentParams):
        result = guest(ws, cfg, "environment", profile=p.profile, bins=all_bins(cfg), apply=True)
        if not result["changed"]:
            return "already-ready", "Shell configuration already matches; no files changed"
        return "succeeded", "Shell configuration verified"
    if isinstance(p, FileParams):
        if not state.get("changed", True):
            return "already-ready", "Destination already matches the desktop source"
        item = source_item(cfg, entry)
        if cfg.sync_verbose:
            progress(entry.id, "preparing destination")
        guest(ws, cfg, "file", relative=item["relative"], directory=item["is_directory"], prepare=True)
        suffix = "/" if item["is_directory"] else ""
        if cfg.sync_verbose:
            progress(entry.id, "transferring files")
        ws.transfer(item["local_path"] + suffix, item["destination"])
        if cfg.sync_verbose:
            progress(entry.id, "verifying destination ownership")
        guest(ws, cfg, "file", relative=item["relative"], directory=item["is_directory"], verify=True)
        return "succeeded", "Transfer and destination ownership verified"
    if isinstance(p, ApplicationParams):
        interactive = p.interaction == "interactive" and not plan.unattended
        command = p.non_interactive if plan.unattended and p.non_interactive else p.command
        def install():
            return ws.run(command_environment(cfg, command, interpreter=p.interpreter, bins=p.bin_dirs), check=False, interactive=interactive)
        result = terminal(install) if interactive else install()
        if result.returncode:
            raise AppError(f"Installer failed (exit {result.returncode}); output withheld")
        if p.check and ws.run(command_environment(cfg, p.check, interpreter=p.interpreter, bins=p.bin_dirs), check=False).returncode:
            raise AppError("Installer exited successfully but installation verification failed")
        action = "updated" if state.get("installed") else "installed"
        return "succeeded", ((f"Application {action}; installation check passed; onboarding remains separate") if p.check else f"Application {action}; command exited successfully; no installation check is configured")
    state = repo.setup_repository(cfg, ws, state["repository"], quiet=True)
    if not state.get("ready"):
        raise AppError("Repository provisioning finished but verification failed")
    return ("succeeded" if state.get("changed") else "already-ready", "Repository Git access verified; working tree preserved")


def execute_plan(cfg: Config, plan: Plan, *, connection_factory=None, workspace=None, terminal=lambda operation: operation(), progress=lambda item, state: None) -> dict:
    factory = connection_factory or WorkspaceSSH.configured
    results = [{"id": e.id, "label": e.label, "group": e.group, "status": "not-run", "detail": ""} for e in plan.entries]
    current = None
    preflight_complete = False
    snapshot = {"created": False, "id": None, "path": None}
    try:
        plan = build_plan(cfg, plan.target, plan.entries, catalog_id=plan.catalog_id, unattended=plan.unattended)
        with ExitStack() as stack:
            ws = workspace or terminal(lambda: stack.enter_context(factory(cfg, plan.target)))
            require_tool(ws, cfg, "python3")
            guest(ws, cfg, "identity", user=cfg.user_name, uid=cfg.user_uid, gid=cfg.user_gid,
                  name=plan.target["name"], vmid=plan.target["vmid"])
            states = {}
            for entry, current in zip(plan.entries, results):
                progress(entry.id, "checking")
                try:
                    states[entry.id] = preflight_entry(ws, cfg, plan, entry)
                    progress(entry.id, "preflight-ready")
                except AppError as exc:
                    current.update(status="blocked", detail=str(exc))
                    progress(entry.id, "blocked")
            if any(r["status"] == "blocked" for r in results):
                return {"ok": False, "plan": plan.public(), "results": results, "snapshot": snapshot}
            current = None

            backup_paths = []
            for entry in plan.entries:
                backup_paths.extend(backup_paths_for_entry(cfg, entry, states[entry.id]))
            if backup_paths:
                snapshot = guest(ws, cfg, "snapshot", paths=list(dict.fromkeys(backup_paths)),
                                 items=[entry.id for entry in plan.entries],
                                 vmid=plan.target["vmid"], name=plan.target["name"])
            preflight_complete = True

            for entry, current in zip(plan.entries, results):
                progress(entry.id, "running")
                status, detail = apply_entry(ws, cfg, plan, entry, states[entry.id], terminal, progress=progress)
                current.update(status=status, detail=detail)
                record_entry_state(ws, cfg, plan, entry, states[entry.id], snapshot_id=snapshot.get("id"))
                progress(entry.id, status)
    except (AppError, KeyboardInterrupt, OSError) as exc:
        status = "failed" if preflight_complete else "blocked"
        detail = "Cancelled; remaining actions were not run" if isinstance(exc, KeyboardInterrupt) else (str(exc) if isinstance(exc, AppError) else "Local transport or file operation failed; output withheld")
        if current is not None:
            current.update(status=status, detail=detail)
        else:
            for item in results:
                item.update(status="blocked", detail=detail)
    return {"ok": all(r["status"] in {"succeeded", "already-ready"} for r in results), "plan": plan.public(), "results": results, "snapshot": snapshot}
