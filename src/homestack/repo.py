"""GitHub repository provisioning for HomeStack workspaces."""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import Any
import json
import os
import re
import shlex
import shutil
import subprocess
import uuid

from .config import Config, validate_repository_spec
from .lifecycle import resolve_workspace_target
from .models import AppError
from .status import resolve_existing_workspace
from .transports.base import Transport, run_local, run_local_passthrough
from .ui import console


class WorkspaceRepoSSH:
    """One hardware-authenticated SSH connection reused for repository work."""

    def __init__(self, alias: str, vmid: int) -> None:
        self.alias = alias
        self.control = f"/tmp/hs-repo-{os.getuid()}-{vmid}-{uuid.uuid4().hex[:8]}"
        self.opened = False

    def __enter__(self) -> WorkspaceRepoSSH:
        if shutil.which("ssh") is None:
            raise AppError("Required local command 'ssh' was not found")
        console.print(
            "[dim]SSH: waiting for workspace authentication; "
            "touch the security key when requested.[/dim]"
        )
        result = run_local_passthrough(
            [
                "ssh",
                "-M",
                "-S",
                self.control,
                "-o",
                "ControlPersist=60",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-f",
                self.alias,
                "true",
            ]
        )
        if result.returncode != 0:
            raise AppError(f"Could not open SSH session to workspace {self.alias!r}")
        self.opened = True
        check = run_local(
            ["ssh", "-S", self.control, "-O", "check", self.alias],
            check=False,
        )
        if check.returncode != 0:
            self.__exit__(None, None, None)
            raise AppError(
                f"Workspace SSH ControlMaster is unavailable for {self.alias!r}"
            )
        return self

    def run(
        self, command: str, *, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        options = [
            "-S",
            self.control,
            "-o",
            "ControlMaster=no",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "GSSAPIAuthentication=no",
            "-o",
            "HostbasedAuthentication=no",
        ]
        return run_local(["ssh", *options, self.alias, command], check=check)

    def __exit__(self, *_: object) -> None:
        if self.opened:
            run_local(
                ["ssh", "-S", self.control, "-O", "exit", self.alias],
                check=False,
            )
            self.opened = False
        try:
            os.unlink(self.control)
        except OSError:
            pass


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
    return run_local(["gh", "api", *args])


def _github_json(args: list[str], expected: type) -> Any:
    try:
        value = json.loads(_gh(args).stdout)
    except json.JSONDecodeError as exc:
        raise AppError("GitHub API returned invalid JSON") from exc
    if not isinstance(value, expected):
        raise AppError("GitHub API returned an unexpected response")
    return value


def _keys(repository: str) -> list[dict[str, Any]]:
    value = _github_json([f"repos/{repository}/keys?per_page=100"], list)
    return [item for item in value if isinstance(item, dict)]


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


def _output(ws: WorkspaceRepoSSH, command: str) -> str:
    result = ws.run(command, check=False)
    return result.stdout.strip() if result.returncode == 0 else ""


def inspect_repository(
    cfg: Config,
    ws: WorkspaceRepoSSH,
    vmid: int,
    name: str,
    repository: str,
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
    if tools["git"] and tools["ssh"] and private:
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
    ws: WorkspaceRepoSSH, key: str, public_key: str, repository: str
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
    cfg: Config, ws: WorkspaceRepoSSH, state: dict[str, Any]
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

    if "generate-key" in actions:
        _generate_key(ws, key, public_key_path, repository)
    elif "derive-public-key" in actions:
        ws.run(
            f"umask 077 && ssh-keygen -y -f {shlex.quote(key)} "
            f"> {shlex.quote(public_key_path)} && "
            f"chmod 644 {shlex.quote(public_key_path)}"
        )
    elif "replace-missing-private-key" in actions:
        if isinstance(state.get("deploy_key_id"), int):
            _delete_key(repository, int(state["deploy_key_id"]))
        ws.run(f"rm -f -- {shlex.quote(public_key_path)}")
        _generate_key(ws, key, public_key_path, repository)

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
        for item in keys:
            same_title = str(item.get("title") or "") == title
            different_key = (
                _identity(str(item.get("key") or "")) != identity
            )
            if (
                same_title
                and different_key
                and isinstance(item.get("id"), int)
            ):
                _delete_key(repository, int(item["id"]))
        _add_key(repository, title, public_key)

    remote_url = f"git@github.com:{repository}.git"
    if "clone" in actions:
        ws.run(
            f"mkdir -p -- "
            f"{shlex.quote(str(PurePosixPath(checkout).parent))}"
        )
        ws.run(
            f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
            f"git clone -- {shlex.quote(remote_url)} {shlex.quote(checkout)}"
        )
    elif "set-origin" in actions:
        ws.run(
            f"git -C {shlex.quote(checkout)} remote set-url origin "
            f"{shlex.quote(remote_url)}"
        )

    if "clone" in actions or "set-ssh-command" in actions:
        ws.run(
            f"git -C {shlex.quote(checkout)} config --local core.sshCommand "
            f"{shlex.quote(_stored_ssh(key))}"
        )

    verify = ws.run(
        f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
        f"git ls-remote {shlex.quote(remote_url)} HEAD",
        check=False,
    )
    if verify.returncode != 0:
        raise AppError(
            "Repository setup completed but Git access verification failed"
        )
    return {
        **inspect_repository(cfg, ws, vmid, name, repository),
        "changed": True,
        "message": "Repository setup completed.",
    }


def rotate_repository_key(
    cfg: Config, ws: WorkspaceRepoSSH, state: dict[str, Any]
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

    try:
        ws.run(
            f"umask 077 && ssh-keygen -q -t ed25519 "
            f"-f {shlex.quote(temporary)} -N '' "
            f"-C {shlex.quote('homestack:' + repository)}"
        )
        _delete_key(repository, int(state["deploy_key_id"]))
        ws.run(
            f"mv -f -- {shlex.quote(temporary)} {shlex.quote(key)} && "
            f"mv -f -- {shlex.quote(temporary_pub)} "
            f"{shlex.quote(public_key_path)} && "
            f"chmod 600 {shlex.quote(key)} && "
            f"chmod 644 {shlex.quote(public_key_path)}"
        )
        _add_key(
            repository,
            f"HomeStack {name}",
            _output(ws, f"cat {shlex.quote(public_key_path)}"),
        )
    finally:
        ws.run(
            f"rm -f -- {shlex.quote(temporary)} "
            f"{shlex.quote(temporary_pub)}",
            check=False,
        )

    remote_url = f"git@github.com:{repository}.git"
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
    return {
        **inspect_repository(cfg, ws, vmid, name, repository),
        "changed": True,
        "message": "Repository deploy key rotated.",
    }


def open_repository_workspace(
    session: Transport, cfg: Config, target: str | int
) -> tuple[int, str, WorkspaceRepoSSH]:
    vmid = resolve_workspace_target(session, cfg, target)
    info = resolve_existing_workspace(
        session, cfg, vmid, require_network=False
    )
    if info["status"] != "running":
        raise AppError(
            f"Workspace VM {vmid} is {info['status']}; "
            "repository operations require a running VM."
        )
    name = str(info["name"])
    return vmid, name, WorkspaceRepoSSH(name, vmid)
