"""GitHub repository provisioning for HomeStack workspaces."""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import Any
import json
import re
import shlex
import shutil
import subprocess
import uuid

from rich.progress import (
    BarColumn,
    Progress,
    SpinnerColumn,
    TaskProgressColumn,
    TextColumn,
    TimeElapsedColumn,
)

from .config import Config, validate_repository_spec
from .lifecycle import resolve_workspace_target
from .models import AppError, validate_name
from .proxmox import (
    cluster_vm_resource,
    qm_config_on_node,
    qm_status_on_node,
    require_workspace_tag,
)
from .transports.base import Transport, run_local
from .ui import console
from .workspace_ssh import WorkspaceSSH
from .guest import derive_ip


def _repository_progress(*, quiet: bool = False) -> Progress:
    return Progress(
        SpinnerColumn(),
        TextColumn("[bold]{task.description}"),
        BarColumn(),
        TaskProgressColumn(),
        TimeElapsedColumn(),
        console=console,
        refresh_per_second=8,
        disable=quiet,
    )


def repository_setup_steps(actions: tuple[str, ...]) -> tuple[str, ...]:
    steps: list[str] = []
    key_steps = {
        "generate-key": "Generate deploy key",
        "derive-public-key": "Restore public deploy key",
        "replace-missing-private-key": "Replace incomplete deploy key",
    }
    for action, label in key_steps.items():
        if action in actions:
            steps.append(label)
            break
    steps.append("Reconcile GitHub deploy key")
    if "clone" in actions:
        steps.append("Clone repository")
    elif "set-origin" in actions:
        steps.append("Configure Git remote")
    if "clone" in actions or "set-ssh-command" in actions:
        steps.append("Configure repository SSH")
    steps.append("Verify Git access")
    steps.append("Refresh repository status")
    return tuple(steps)


def resolve_repository_argument(
    cfg: Config, explicit: str | None, workspace_name: str
) -> str:
    if explicit is not None:
        return validate_repository_spec(explicit)
    if not cfg.repo_owner:
        raise AppError(
            "Repository was not specified and [repo] owner is not configured"
        )
    return validate_repository_spec(f"{cfg.repo_owner}/{workspace_name}")


def repository_paths(cfg: Config, repository: str) -> tuple[str, str, str]:
    owner, name = validate_repository_spec(repository).split("/", 1)
    home = PurePosixPath("/home") / cfg.user_name
    checkout = home / cfg.repo_checkout_root[2:].rstrip("/") / name
    key = home / ".ssh" / "homestack" / "github" / f"{owner}-{name}"
    return str(checkout), str(key), str(key) + ".pub"


def repository_from_remote(url: str | None) -> str | None:
    value = str(url or "").strip()
    for pattern in (
        r"^git@github\.com:(.+?/.+?)(?:\.git)?$",
        r"^ssh://git@github\.com/(.+?/.+?)(?:\.git)?$",
        r"^https://github\.com/(.+?/.+?)(?:\.git)?/?$",
    ):
        match = re.fullmatch(pattern, value)
        if match:
            return match.group(1)
    return None


def _identity(key: str) -> str | None:
    parts = key.strip().split()
    return f"{parts[0]} {parts[1]}" if len(parts) >= 2 else None


def _stored_ssh(key: str) -> str:
    return f"ssh -i {key} -o IdentitiesOnly=yes"


def _bootstrap_ssh(key: str) -> str:
    return (
        _stored_ssh(key)
        + " -o StrictHostKeyChecking=accept-new -o BatchMode=yes"
    )


def _gh(args: list[str]) -> subprocess.CompletedProcess[str]:
    if shutil.which("gh") is None:
        raise AppError("Required local command 'gh' was not found")
    result = run_local(["gh", "api", "--hostname", "github.com", *args], check=False)
    if result.returncode:
        raise AppError("Desktop GitHub request failed; check gh authentication and repository administration permissions")
    return result


def _github_json(args: list[str], expected: type) -> Any:
    try:
        value = json.loads(_gh(args).stdout)
    except json.JSONDecodeError as exc:
        raise AppError("GitHub API returned invalid JSON") from exc
    if not isinstance(value, expected):
        raise AppError("GitHub API returned an unexpected response")
    return value


def _keys(repository: str) -> list[dict[str, Any]]:
    pages = _github_json(["--paginate", "--slurp", f"repos/{repository}/keys?per_page=100"], list)
    return [item for page in pages for item in page if isinstance(item, dict)]


def _matching_key(
    keys: list[dict[str, Any]], public_key: str | None
) -> dict[str, Any] | None:
    wanted = _identity(public_key or "")
    if not wanted:
        return None
    return next(
        (
            item
            for item in keys
            if _identity(str(item.get("key") or "")) == wanted
        ),
        None,
    )


def _add_key(repository: str, title: str, public_key: str) -> None:
    identity = _identity(public_key)
    if not identity:
        raise AppError("Workspace public deploy key is invalid")
    _gh(
        [
            "--method",
            "POST",
            f"repos/{repository}/keys",
            "-f",
            f"title={title}",
            "-f",
            f"key={identity}",
            "-F",
            "read_only=false",
        ]
    )


def _delete_key(repository: str, key_id: int) -> None:
    _gh(["--method", "DELETE", f"repos/{repository}/keys/{key_id}"])


def _output(ws: WorkspaceSSH, command: str) -> str:
    result = ws.run(command, check=False)
    return result.stdout.strip() if result.returncode == 0 else ""


def inspect_repository(
    cfg: Config,
    ws: WorkspaceSSH,
    vmid: int,
    name: str,
    repository: str,
    *, verify_access: bool = True,
) -> dict[str, Any]:
    repository = validate_repository_spec(repository)
    _github_json([f"repos/{repository}"], dict)
    deploy_keys = _keys(repository)
    checkout, key, public_key_path = repository_paths(cfg, repository)
    tools = {
        tool: ws.run(
            f"command -v {tool} >/dev/null 2>&1", check=False
        ).returncode
        == 0
        for tool in ("git", "ssh", "ssh-keygen")
    }

    exists = ws.run(
        f"test -e {shlex.quote(checkout)}", check=False
    ).returncode == 0
    checkout_state = "missing"
    origin: str | None = None
    working_tree = "unavailable"
    if exists and tools["git"]:
        inside = ws.run(
            f"git -C {shlex.quote(checkout)} rev-parse --is-inside-work-tree",
            check=False,
        )
        if inside.returncode != 0 or inside.stdout.strip() != "true":
            checkout_state = "not-a-repository"
        else:
            origin = (
                _output(
                    ws,
                    f"git -C {shlex.quote(checkout)} remote get-url origin",
                )
                or None
            )
            remote = repository_from_remote(origin)
            if remote == repository:
                checkout_state = "ready"
            elif remote:
                checkout_state = "different-repository"
            else:
                checkout_state = "repository-without-recognized-origin"
            status = ws.run(
                f"git -C {shlex.quote(checkout)} status --porcelain",
                check=False,
            )
            working_tree = (
                "clean"
                if status.returncode == 0 and not status.stdout.strip()
                else "modified"
            )
    elif exists:
        checkout_state = "uninspectable"

    private = ws.run(
        f"test -f {shlex.quote(key)}", check=False
    ).returncode == 0
    public = ws.run(
        f"test -f {shlex.quote(public_key_path)}", check=False
    ).returncode == 0
    public_key = (
        _output(ws, f"cat {shlex.quote(public_key_path)}") if public else ""
    )
    fingerprint = (
        _output(ws, f"ssh-keygen -lf {shlex.quote(public_key_path)}")
        if public and tools["ssh-keygen"]
        else ""
    )
    if private and public and tools["ssh-keygen"]:
        derived = _output(
            ws, f"ssh-keygen -y -f {shlex.quote(key)}"
        )
        key_state = (
            "ready"
            if _identity(derived) == _identity(public_key)
            else "mismatched"
        )
    elif private and public:
        key_state = "uninspectable"
    elif private:
        key_state = "private-only"
    elif public:
        key_state = "public-only"
    else:
        key_state = "missing"

    matching = _matching_key(deploy_keys, public_key)
    deploy_key_id = (
        matching.get("id")
        if matching and isinstance(matching.get("id"), int)
        else None
    )
    deploy_key_state = (
        "missing"
        if not matching
        else ("read-only" if matching.get("read_only") else "read-write")
    )

    expected_ssh = _stored_ssh(key)
    core_ssh = (
        _output(
            ws,
            f"git -C {shlex.quote(checkout)} "
            "config --local --get core.sshCommand",
        )
        if checkout_state == "ready"
        else ""
    )
    ssh_state = (
        "ready"
        if core_ssh == expected_ssh
        else ("different" if core_ssh else "missing")
    )

    access = "not-tested"
    if verify_access and tools["git"] and tools["ssh"] and private:
        remote_url = f"git@github.com:{repository}.git"
        result = ws.run(
            f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
            f"git ls-remote {shlex.quote(remote_url)} HEAD",
            check=False,
        )
        access = "working" if result.returncode == 0 else "failed"

    ready = (
        checkout_state == "ready"
        and key_state == "ready"
        and deploy_key_state == "read-write"
        and ssh_state == "ready"
        and access == "working"
        and all(tools.values())
    )
    untouched = (
        checkout_state == "missing"
        and key_state == "missing"
        and deploy_key_state == "missing"
    )
    return {
        "ok": True,
        "command": "repo",
        "vmid": vmid,
        "workspace": name,
        "repository": repository,
        "checkout": checkout,
        "checkout_state": checkout_state,
        "working_tree": working_tree,
        "origin": origin,
        "key_path": key,
        "key_state": key_state,
        "key_fingerprint": fingerprint or None,
        "deploy_key_state": deploy_key_state,
        "deploy_key_id": deploy_key_id,
        "core_ssh_command": core_ssh or None,
        "ssh_config_state": ssh_state,
        "access": access,
        "tools": tools,
        "ready": ready,
        "status": (
            "ready"
            if ready
            else ("not configured" if untouched else "needs setup")
        ),
    }


def repository_setup_actions(state: dict[str, Any]) -> tuple[str, ...]:
    checkout = str(state.get("checkout_state") or "")
    if checkout in {
        "not-a-repository",
        "repository-without-recognized-origin",
        "different-repository",
    }:
        raise AppError(
            f"Checkout path is unsafe to modify: {checkout}. "
            "HomeStack will not clean, reset, or replace it."
        )
    key = str(state.get("key_state") or "missing")
    if key == "mismatched":
        raise AppError(
            "Repository deploy-key private/public files do not match; rotate the key"
        )
    actions: list[str] = []
    actions += {
        "missing": ["generate-key"],
        "private-only": ["derive-public-key"],
        "public-only": ["replace-missing-private-key"],
    }.get(key, [])
    actions += {
        "missing": ["register-key"],
        "read-only": ["replace-read-only-key"],
    }.get(str(state.get("deploy_key_state") or "missing"), [])
    if checkout == "missing":
        actions.append("clone")
    else:
        if not str(state.get("origin") or "").startswith("git@github.com:"):
            actions.append("set-origin")
        if state.get("ssh_config_state") != "ready":
            actions.append("set-ssh-command")
    if actions or state.get("access") != "working":
        actions.append("verify-access")
    return tuple(actions)


def _generate_key(
    ws: WorkspaceSSH, key: str, public_key: str, repository: str
) -> None:
    github_dir = str(PurePosixPath(key).parent)
    homestack_dir = str(PurePosixPath(github_dir).parent)
    ssh_dir = str(PurePosixPath(homestack_dir).parent)
    ws.run(
        f"install -d -m 700 {shlex.quote(ssh_dir)} "
        f"{shlex.quote(homestack_dir)} {shlex.quote(github_dir)} && "
        f"chmod 700 {shlex.quote(ssh_dir)} {shlex.quote(homestack_dir)} "
        f"{shlex.quote(github_dir)} && umask 077 && "
        f"ssh-keygen -q -t ed25519 -f {shlex.quote(key)} -N '' "
        f"-C {shlex.quote('homestack:' + repository)} && "
        f"chmod 600 {shlex.quote(key)} && "
        f"chmod 644 {shlex.quote(public_key)}"
    )


def _require_tools(state: dict[str, Any]) -> None:
    missing = [
        tool
        for tool in ("git", "ssh", "ssh-keygen")
        if not (state.get("tools") or {}).get(tool)
    ]
    if missing:
        raise AppError(
            "Workspace is missing required repository tools: "
            + ", ".join(missing)
        )


def setup_repository(
    cfg: Config, ws: WorkspaceSSH, state: dict[str, Any], *, quiet: bool = False
) -> dict[str, Any]:
    _require_tools(state)
    actions = repository_setup_actions(state)
    if not actions:
        return {
            **state,
            "changed": False,
            "message": "Repository is already configured.",
        }

    repository = str(state["repository"])
    name = str(state["workspace"])
    vmid = int(state["vmid"])
    checkout, key, public_key_path = repository_paths(cfg, repository)
    title = f"HomeStack {name}"
    steps = repository_setup_steps(actions)

    with _repository_progress(quiet=quiet) as progress:
        task = progress.add_task(steps[0], total=len(steps))

        if "generate-key" in actions:
            progress.update(task, description="Generate deploy key")
            _generate_key(ws, key, public_key_path, repository)
            progress.advance(task)
        elif "derive-public-key" in actions:
            progress.update(task, description="Restore public deploy key")
            ws.run(
                f"umask 077 && ssh-keygen -y -f {shlex.quote(key)} "
                f"> {shlex.quote(public_key_path)} && "
                f"chmod 644 {shlex.quote(public_key_path)}"
            )
            progress.advance(task)
        elif "replace-missing-private-key" in actions:
            progress.update(task, description="Replace incomplete deploy key")
            if isinstance(state.get("deploy_key_id"), int):
                _delete_key(repository, int(state["deploy_key_id"]))
            ws.run(f"rm -f -- {shlex.quote(public_key_path)}")
            _generate_key(ws, key, public_key_path, repository)
            progress.advance(task)

        progress.update(task, description="Reconcile GitHub deploy key")
        public_key = _output(ws, f"cat {shlex.quote(public_key_path)}")
        identity = _identity(public_key)
        if not identity:
            raise AppError("Workspace did not produce a valid public deploy key")

        keys = _keys(repository)
        matching = _matching_key(keys, public_key)
        if matching and matching.get("read_only"):
            if not isinstance(matching.get("id"), int):
                raise AppError("GitHub returned a deploy key without a numeric ID")
            _delete_key(repository, int(matching["id"]))
            matching = None

        if not matching:
            _add_key(repository, title, public_key)
        progress.advance(task)

        remote_url = f"git@github.com:{repository}.git"
        if "clone" in actions:
            progress.update(task, description="Clone repository")
            ws.run(
                f"mkdir -p -- "
                f"{shlex.quote(str(PurePosixPath(checkout).parent))}"
            )
            ws.run(
                f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
                f"git clone -- {shlex.quote(remote_url)} {shlex.quote(checkout)}"
            )
            progress.advance(task)
        elif "set-origin" in actions:
            progress.update(task, description="Configure Git remote")
            ws.run(
                f"git -C {shlex.quote(checkout)} remote set-url origin "
                f"{shlex.quote(remote_url)}"
            )
            progress.advance(task)

        if "clone" in actions or "set-ssh-command" in actions:
            progress.update(task, description="Configure repository SSH")
            ws.run(
                f"git -C {shlex.quote(checkout)} config --local core.sshCommand "
                f"{shlex.quote(_stored_ssh(key))}"
            )
            progress.advance(task)

        progress.update(task, description="Verify Git access")
        verify = ws.run(
            f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
            f"git ls-remote {shlex.quote(remote_url)} HEAD",
            check=False,
        )
        if verify.returncode != 0:
            raise AppError(
                "Repository setup completed but Git access verification failed"
            )
        progress.advance(task)

        progress.update(task, description="Refresh repository status")
        refreshed = inspect_repository(cfg, ws, vmid, name, repository)
        progress.advance(task)

    return {
        **refreshed,
        "changed": any(action != "verify-access" for action in actions),
        "message": "Repository setup completed.",
    }

def rotate_repository_key(
    cfg: Config, ws: WorkspaceSSH, state: dict[str, Any]
) -> dict[str, Any]:
    _require_tools(state)
    if (
        state.get("checkout_state") != "ready"
        or not isinstance(state.get("deploy_key_id"), int)
    ):
        raise AppError(
            "Repository deploy key is not configured; run Setup first"
        )

    repository = str(state["repository"])
    name = str(state["workspace"])
    vmid = int(state["vmid"])
    _, key, public_key_path = repository_paths(cfg, repository)
    temporary = key + f".rotate-{uuid.uuid4().hex[:8]}"
    temporary_pub = temporary + ".pub"

    with _repository_progress() as progress:
        task = progress.add_task("Generate replacement deploy key", total=6)
        try:
            ws.run(
                f"umask 077 && ssh-keygen -q -t ed25519 "
                f"-f {shlex.quote(temporary)} -N '' "
                f"-C {shlex.quote('homestack:' + repository)}"
            )
            progress.advance(task)

            progress.update(task, description="Remove previous GitHub deploy key")
            _delete_key(repository, int(state["deploy_key_id"]))
            progress.advance(task)

            progress.update(task, description="Install replacement deploy key")
            ws.run(
                f"mv -f -- {shlex.quote(temporary)} {shlex.quote(key)} && "
                f"mv -f -- {shlex.quote(temporary_pub)} "
                f"{shlex.quote(public_key_path)} && "
                f"chmod 600 {shlex.quote(key)} && "
                f"chmod 644 {shlex.quote(public_key_path)}"
            )
            progress.advance(task)

            progress.update(task, description="Register GitHub deploy key")
            _add_key(
                repository,
                f"HomeStack {name}",
                _output(ws, f"cat {shlex.quote(public_key_path)}"),
            )
            progress.advance(task)
        finally:
            ws.run(
                f"rm -f -- {shlex.quote(temporary)} "
                f"{shlex.quote(temporary_pub)}",
                check=False,
            )

        remote_url = f"git@github.com:{repository}.git"
        progress.update(task, description="Verify Git access")
        verify = ws.run(
            f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
            f"git ls-remote {shlex.quote(remote_url)} HEAD",
            check=False,
        )
        if verify.returncode != 0:
            raise AppError(
                "Deploy key was rotated but Git access verification failed. "
                "Re-run Setup to repair registration."
            )
        progress.advance(task)

        progress.update(task, description="Refresh repository status")
        refreshed = inspect_repository(cfg, ws, vmid, name, repository)
        progress.advance(task)

    return {
        **refreshed,
        "changed": True,
        "message": "Repository deploy key rotated.",
    }

def repository_workspace_info(
    session: Transport, cfg: Config, vmid: int
) -> dict[str, str | int]:
    """Resolve only the workspace identity needed for repository operations."""
    if vmid == cfg.gold_vmid:
        raise AppError(f"Refusing repository operation on Gold VM {cfg.gold_vmid}")

    resource = cluster_vm_resource(session, vmid)
    if resource is None:
        raise AppError(f"VMID {vmid} does not exist")
    node = str(resource.get("node") or "")
    if not node:
        raise AppError(f"VM {vmid} has no node in cluster inventory")

    vm_cfg = qm_config_on_node(session, cfg, node, vmid)
    require_workspace_tag(vmid, vm_cfg)
    name = str(vm_cfg.get("name") or "").strip()
    if not name:
        raise AppError(f"VM {vmid} has no name")
    validate_name(name)

    status = str(resource.get("status") or "").strip()
    if not status:
        status = qm_status_on_node(session, cfg, node, vmid)
    return {
        "vmid": vmid,
        "name": name,
        "node": node,
        "status": status,
    }


def open_repository_workspace(
    session: Transport, cfg: Config, target: str | int
) -> tuple[int, str, WorkspaceSSH]:
    vmid = resolve_workspace_target(session, cfg, target)
    info = repository_workspace_info(session, cfg, vmid)
    if info["status"] != "running":
        raise AppError(
            f"Workspace VM {vmid} is {info['status']}; "
            "repository operations require a running VM."
        )
    name = str(info["name"])
    return vmid, name, WorkspaceSSH.configured(cfg, {**info, "ip": derive_ip(cfg, vmid)})
