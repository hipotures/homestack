"""GitHub repository provisioning for HomeStack workspaces."""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import Any, Callable
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
    if "fetch-upstream" in actions:
        steps.append("Fetch repository upstream")
        steps.append("Inspect and update repository")
    if "set-origin" in actions:
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


def _upstream_counts(ws: WorkspaceSSH, checkout: str) -> tuple[int, int] | None:
    result = ws.run(
        f"git -C {shlex.quote(checkout)} rev-list --left-right --count "
        "HEAD...@{upstream}",
        check=False,
    )
    if result.returncode:
        return None
    fields = result.stdout.split()
    if len(fields) != 2:
        return None
    try:
        return int(fields[0]), int(fields[1])
    except ValueError:
        return None


def _repository_checkout_error(state: dict[str, Any]) -> AppError | None:
    if state.get("working_tree") == "modified":
        return AppError(
            "Checkout has uncommitted, staged, or untracked changes; "
            "refusing to update it"
        )
    if state.get("working_tree") != "clean":
        return AppError(
            "Could not inspect the checkout's working tree; refusing to update it"
        )
    if state.get("head_state") != "attached" or not state.get("branch"):
        return AppError(
            "Checkout is in detached HEAD state; refusing to update it"
        )
    if not state.get("upstream"):
        return AppError(
            "Current branch has no configured upstream; refusing to update it"
        )
    if (
        not state.get("upstream_remote")
        or state.get("upstream_remote") == "."
        or not state.get("upstream_merge")
        or not state.get("upstream_ref")
    ):
        return AppError(
            "Current branch does not track a fetchable remote branch; "
            "refusing to update it"
        )
    return None


def _repository_history_error(state: dict[str, Any]) -> AppError | None:
    counts = state.get("ahead"), state.get("behind")
    if not all(isinstance(value, int) and value >= 0 for value in counts):
        return AppError(
            "Could not determine the checkout's upstream history; "
            "refusing to update it"
        )
    ahead, behind = counts
    if ahead:
        if behind:
            return AppError(
                "Checkout history has diverged from its upstream; refusing to update it"
            )
        return AppError(
            "Checkout contains local commits ahead of its upstream; "
            "refusing to update it"
        )
    return None


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
    head_state = "unavailable"
    branch: str | None = None
    upstream: str | None = None
    upstream_remote: str | None = None
    upstream_merge: str | None = None
    upstream_ref: str | None = None
    ahead: int | None = None
    behind: int | None = None
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
                f"git -C {shlex.quote(checkout)} status --porcelain "
                "--untracked-files=all",
                check=False,
            )
            working_tree = (
                "unavailable"
                if status.returncode
                else ("modified" if status.stdout.strip() else "clean")
            )
            branch_result = ws.run(
                f"git -C {shlex.quote(checkout)} symbolic-ref --quiet --short HEAD",
                check=False,
            )
            if branch_result.returncode == 0 and branch_result.stdout.strip():
                head_state = "attached"
                branch = branch_result.stdout.strip()
                upstream_result = ws.run(
                    f"git -C {shlex.quote(checkout)} rev-parse "
                    "--abbrev-ref --symbolic-full-name @{upstream}",
                    check=False,
                )
                if upstream_result.returncode == 0 and upstream_result.stdout.strip():
                    upstream = upstream_result.stdout.strip()
                    upstream_remote = _output(
                        ws,
                        f"git -C {shlex.quote(checkout)} config --get "
                        f"{shlex.quote(f'branch.{branch}.remote')}",
                    ) or None
                    upstream_merge = _output(
                        ws,
                        f"git -C {shlex.quote(checkout)} config --get "
                        f"{shlex.quote(f'branch.{branch}.merge')}",
                    ) or None
                    upstream_ref = _output(
                        ws,
                        f"git -C {shlex.quote(checkout)} rev-parse "
                        "--symbolic-full-name @{upstream}",
                    ) or None
                    counts = _upstream_counts(ws, checkout)
                    if counts is not None:
                        ahead, behind = counts
            else:
                head_state = "detached"
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
        "head_state": head_state,
        "branch": branch,
        "upstream": upstream,
        "upstream_remote": upstream_remote,
        "upstream_merge": upstream_merge,
        "upstream_ref": upstream_ref,
        "ahead": ahead,
        "behind": behind,
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
        if error := _repository_checkout_error(state):
            raise error
        if not str(state.get("origin") or "").startswith("git@github.com:"):
            actions.append("set-origin")
        if state.get("ssh_config_state") != "ready":
            actions.append("set-ssh-command")
        actions.append("fetch-upstream")
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


def _fetch_upstream(
    ws: WorkspaceSSH,
    checkout: str,
    key: str,
    repository: str,
    upstream: str,
    remote: str,
    merge_ref: str,
    upstream_ref: str,
) -> None:
    configured_url = _output(
        ws,
        f"git -C {shlex.quote(checkout)} remote get-url {shlex.quote(remote)}",
    )
    if not configured_url:
        raise AppError(
            f"Configured upstream remote {remote!r} is missing; "
            "refusing to update it"
        )
    fetch_url = (
        f"git@github.com:{repository}.git"
        if repository_from_remote(configured_url) == repository
        else configured_url
    )
    refspec = f"{merge_ref}:{upstream_ref}"
    result = ws.run(
        f"GIT_SSH_COMMAND={shlex.quote(_bootstrap_ssh(key))} "
        f"git -C {shlex.quote(checkout)} -c core.hooksPath=/dev/null "
        "fetch --no-tags -- "
        f"{shlex.quote(fetch_url)} {shlex.quote(refspec)}",
        check=False,
    )
    if result.returncode:
        raise AppError(
            f"Could not fetch configured repository upstream {upstream}; "
            "the working tree was not modified"
        )


def setup_repository(
    cfg: Config,
    ws: WorkspaceSSH,
    state: dict[str, Any],
    *,
    quiet: bool = False,
    activity: Callable[[str], None] | None = None,
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
    fast_forwarded = False

    with _repository_progress(quiet=quiet) as progress:
        task = progress.add_task(steps[0], total=len(steps))

        if "generate-key" in actions:
            if activity:
                activity("Generate deploy key")
            progress.update(task, description="Generate deploy key")
            _generate_key(ws, key, public_key_path, repository)
            progress.advance(task)
        elif "derive-public-key" in actions:
            if activity:
                activity("Restore public deploy key")
            progress.update(task, description="Restore public deploy key")
            ws.run(
                f"umask 077 && ssh-keygen -y -f {shlex.quote(key)} "
                f"> {shlex.quote(public_key_path)} && "
                f"chmod 644 {shlex.quote(public_key_path)}"
            )
            progress.advance(task)
        elif "replace-missing-private-key" in actions:
            if activity:
                activity("Replace incomplete deploy key")
            progress.update(task, description="Replace incomplete deploy key")
            if isinstance(state.get("deploy_key_id"), int):
                _delete_key(repository, int(state["deploy_key_id"]))
            ws.run(f"rm -f -- {shlex.quote(public_key_path)}")
            _generate_key(ws, key, public_key_path, repository)
            progress.advance(task)

        if activity:
            activity("Reconcile GitHub deploy key")
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
            if activity:
                activity("Clone repository")
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

        if "fetch-upstream" in actions:
            if activity:
                activity("Fetch repository upstream")
            progress.update(task, description="Fetch repository upstream")
            _fetch_upstream(
                ws,
                checkout,
                key,
                repository,
                str(state["upstream"]),
                str(state["upstream_remote"]),
                str(state["upstream_merge"]),
                str(state["upstream_ref"]),
            )
            progress.advance(task)

            status = ws.run(
                f"git -C {shlex.quote(checkout)} status --porcelain "
                "--untracked-files=all",
                check=False,
            )
            counts = _upstream_counts(ws, checkout)
            current_state = {
                **state,
                "working_tree": (
                    "unavailable"
                    if status.returncode
                    else ("modified" if status.stdout.strip() else "clean")
                ),
                "ahead": counts[0] if counts is not None else None,
                "behind": counts[1] if counts is not None else None,
            }
            if error := _repository_checkout_error(current_state):
                raise error
            if error := _repository_history_error(current_state):
                raise error
            ahead, behind = counts  # validated by _repository_history_error
            if behind:
                if activity:
                    activity("Fast-forward repository")
                progress.update(task, description="Fast-forward repository")
                merge_options = f"branch.{state['branch']}.mergeOptions="
                merged = ws.run(
                    f"git -C {shlex.quote(checkout)} "
                    f"-c {shlex.quote(merge_options)} "
                    "-c core.hooksPath=/dev/null "
                    "merge --ff-only --no-squash --no-autostash "
                    "--no-overwrite-ignore @{upstream}",
                    check=False,
                )
                if merged.returncode:
                    raise AppError(
                        "Repository could not be fast-forwarded; "
                        "no reset or cleanup was attempted"
                    )
                verified_counts = _upstream_counts(ws, checkout)
                status = ws.run(
                    f"git -C {shlex.quote(checkout)} status --porcelain "
                    "--untracked-files=all",
                    check=False,
                )
                if (
                    verified_counts != (0, 0)
                    or status.returncode != 0
                    or status.stdout.strip()
                ):
                    raise AppError(
                        "Repository fast-forward completed without a clean "
                        "up-to-date checkout"
                    )
                fast_forwarded = True
            else:
                if activity:
                    activity("Repository already up to date")
                progress.update(task, description="Repository already up to date")
            progress.advance(task)

        if "set-origin" in actions:
            if activity:
                activity("Configure Git remote")
            progress.update(task, description="Configure Git remote")
            ws.run(
                f"git -C {shlex.quote(checkout)} remote set-url origin "
                f"{shlex.quote(remote_url)}"
            )
            progress.advance(task)

        if "clone" in actions or "set-ssh-command" in actions:
            if activity:
                activity("Configure repository SSH")
            progress.update(task, description="Configure repository SSH")
            ws.run(
                f"git -C {shlex.quote(checkout)} config --local core.sshCommand "
                f"{shlex.quote(_stored_ssh(key))}"
            )
            progress.advance(task)

        if activity:
            activity("Verify Git access")
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

        if activity:
            activity("Refresh repository status")
        progress.update(task, description="Refresh repository status")
        refreshed = inspect_repository(cfg, ws, vmid, name, repository)
        progress.advance(task)

    configuration_changed = any(
        action not in {"verify-access", "fetch-upstream"}
        for action in actions
    )
    if fast_forwarded:
        message = "Repository fast-forwarded to its upstream."
    elif configuration_changed:
        message = "Repository setup completed."
    else:
        message = "Repository is already up to date."
    return {
        **refreshed,
        "changed": fast_forwarded or configuration_changed,
        "message": message,
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
