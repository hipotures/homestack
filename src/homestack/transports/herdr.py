"""Herdr support for HomeStack."""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterator
import fcntl
import json
import os
import re
import shlex
import shutil
import time
import uuid

from ..config import Config
from ..models import AppError, RemoteResult
from .base import run_local

@dataclass(frozen=True)
class HerdrTarget:
    workspace_id: str
    tab_id: str
    pane_id: str
    ssh_cmdline: str


ANSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def parse_json_response(text: str, context: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError as exc:
        raise AppError(f"Invalid JSON from {context}: {exc}\n{text[:1000]}") from exc
    if not isinstance(value, dict):
        raise AppError(f"Unexpected JSON value from {context}: expected object")
    return value


def herdr_json(args: list[str], *, check: bool = True) -> dict[str, Any]:
    proc = run_local(["herdr", *args], check=False)
    if proc.returncode != 0:
        if check:
            detail = (proc.stderr or proc.stdout).strip()
            raise AppError(
                f"Herdr command failed ({proc.returncode}): {shlex.join(['herdr', *args])}"
                + (f"\n{detail}" if detail else "")
            )
        return {"error": {"code": "local-command", "message": (proc.stderr or proc.stdout).strip()}}
    return parse_json_response(proc.stdout, f"herdr {' '.join(args)}")


def strip_ansi(text: str) -> str:
    return ANSI_RE.sub("", text).replace("\r", "")


def wrap_remote_command(command: str, token: str) -> str:
    qtoken = shlex.quote(token)
    return (
        f"hs_t={qtoken}; "
        "printf '__HS_BEGIN__%s\\n' \"$hs_t\"; "
        f"{command}; "
        "rc=$?; "
        "printf '\\n__HS_END__%s:%d\\n' \"$hs_t\" \"$rc\""
    )


def parse_remote_envelope(text: str, token: str) -> RemoteResult | None:
    cleaned = strip_ansi(text)
    begin = f"__HS_BEGIN__{token}"
    end_re = re.compile(rf"__HS_END__{re.escape(token)}:(\d+)")

    begin_pos = cleaned.rfind(begin)
    if begin_pos < 0:
        return None

    output_start = begin_pos + len(begin)
    if output_start < len(cleaned) and cleaned[output_start] == "\r":
        output_start += 1
    if output_start < len(cleaned) and cleaned[output_start] == "\n":
        output_start += 1

    match = end_re.search(cleaned, output_start)
    if match is None:
        return None

    output = cleaned[output_start:match.start()].strip()
    return RemoteResult(returncode=int(match.group(1)), output=output)


def parse_remote_returncode(text: str, token: str) -> int | None:
    cleaned = strip_ansi(text)
    match = re.search(rf"__HS_END__{re.escape(token)}:(\d+)", cleaned)
    return int(match.group(1)) if match else None


class HerdrSession:
    def __init__(self, cfg: Config) -> None:
        self.cfg = cfg
        self.target: HerdrTarget | None = None
        self._verification: dict[str, Any] | None = None

    @property
    def pane_id(self) -> str:
        if self.target is None:
            raise AppError("Herdr target has not been discovered")
        return self.target.pane_id

    def discover(self) -> HerdrTarget:
        workspaces = herdr_json(["workspace", "list"])
        workspace_items = workspaces.get("result", {}).get("workspaces", [])
        matches = [x for x in workspace_items if x.get("label") == self.cfg.herdr_workspace]
        if not matches:
            raise AppError(
                f"Herdr workspace {self.cfg.herdr_workspace!r} is not open. "
                "Open the PVE workspace before running HomeStack."
            )
        if len(matches) != 1:
            raise AppError(f"Multiple Herdr workspaces named {self.cfg.herdr_workspace!r} were found")
        workspace_id = str(matches[0].get("workspace_id") or "")
        if not workspace_id:
            raise AppError("Herdr returned a workspace without workspace_id")

        tabs = herdr_json(["tab", "list", "--workspace", workspace_id])
        tab_items = tabs.get("result", {}).get("tabs", [])
        tab_matches = [x for x in tab_items if x.get("label") == self.cfg.herdr_tab]
        if not tab_matches:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} is not open in workspace "
                f"{self.cfg.herdr_workspace!r}."
            )
        if len(tab_matches) != 1:
            raise AppError(f"Multiple Herdr tabs named {self.cfg.herdr_tab!r} were found")
        tab_id = str(tab_matches[0].get("tab_id") or "")
        if not tab_id:
            raise AppError("Herdr returned a tab without tab_id")

        panes = herdr_json(["pane", "list", "--workspace", workspace_id])
        pane_items = panes.get("result", {}).get("panes", [])
        pane_matches = [x for x in pane_items if str(x.get("tab_id") or "") == tab_id]
        if not pane_matches:
            raise AppError(f"Herdr tab {self.cfg.herdr_tab!r} has no pane")
        if len(pane_matches) != 1:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} has {len(pane_matches)} panes. "
                "HomeStack currently requires exactly one pane per PVE tab."
            )
        pane_id = str(pane_matches[0].get("pane_id") or "")
        if not pane_id:
            raise AppError("Herdr returned a pane without pane_id")

        process_info = herdr_json(["pane", "process-info", "--pane", pane_id])
        info = process_info.get("result", {}).get("process_info", {})
        foreground = info.get("foreground_processes", []) or []
        ssh_processes = [x for x in foreground if str(x.get("name") or "") == "ssh"]
        if not ssh_processes:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} is not currently running SSH. "
                "Log in to the Proxmox node first."
            )
        if len(ssh_processes) != 1:
            raise AppError(f"Unexpected multiple foreground SSH processes in pane {pane_id}")

        ssh = ssh_processes[0]
        argv = [str(x) for x in ssh.get("argv", [])]
        cmdline = str(ssh.get("cmdline") or " ".join(argv))
        expected = self.cfg.control_node
        target_ok = any(x == expected or x.endswith("@" + expected) for x in argv[1:])
        if not target_ok:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} is connected with {cmdline!r}, "
                f"not to expected node {expected!r}."
            )

        self.target = HerdrTarget(
            workspace_id=workspace_id,
            tab_id=tab_id,
            pane_id=pane_id,
            ssh_cmdline=cmdline,
        )
        return self.target

    def read(self, lines: int = 80) -> str:
        proc = run_local(
            [
                "herdr",
                "pane",
                "read",
                self.pane_id,
                "--source",
                "recent-unwrapped",
                "--lines",
                str(lines),
            ],
            check=False,
        )
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()
            raise AppError(
                f"Herdr pane read failed ({proc.returncode})"
                + (f"\n{detail}" if detail else "")
            )
        return proc.stdout

    def read_detection(self) -> str:
        proc = run_local(
            [
                "herdr",
                "pane",
                "read",
                self.pane_id,
                "--source",
                "detection",
            ],
            check=False,
        )
        if proc.returncode != 0:
            return ""
        return proc.stdout

    def _prompt_ready(self) -> bool:
        text = strip_ansi(self.read_detection())
        nonempty = [line.rstrip() for line in text.splitlines() if line.strip()]
        if not nonempty:
            return False
        last = nonempty[-1]
        pattern = rf"root@{re.escape(self.cfg.control_node)}(?::[^#\n]*)?#\s*$"
        return re.search(pattern, last) is not None

    def wait_for_prompt(self, timeout: float = 3.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self._prompt_ready():
                return
            time.sleep(0.1)
        raise AppError(
            f"Herdr tab {self.cfg.herdr_tab!r} is connected to SSH but is not at a "
            f"root@{self.cfg.control_node} shell prompt. Finish the interactive command in that tab first."
        )

    def clear_pane_history(self) -> None:
        if self.cfg.herdr_debug:
            return

        clear_command = r"printf '\033[3J\033[2J\033[H'"
        proc = run_local(
            ["herdr", "pane", "run", self.pane_id, clear_command],
            check=False,
        )
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()
            raise AppError(
                f"Failed to clear Herdr pane history ({proc.returncode})"
                + (f"\n{detail}" if detail else "")
            )
        self.wait_for_prompt(timeout=2.0)

    def run(
        self,
        command: str,
        *,
        check: bool = True,
        timeout: int = 120,
        output_lines: int = 4000,
    ) -> RemoteResult:
        self.wait_for_prompt()
        token = uuid.uuid4().hex
        end_prefix = f"__HS_END__{token}:"
        wrapped = wrap_remote_command(command, token)

        run_proc = run_local(["herdr", "pane", "run", self.pane_id, wrapped], check=False)
        if run_proc.returncode != 0:
            detail = (run_proc.stderr or run_proc.stdout).strip()
            raise AppError(
                f"Herdr pane run failed ({run_proc.returncode})"
                + (f"\n{detail}" if detail else "")
            )

        wait = herdr_json(
            [
                "pane",
                "wait-output",
                self.pane_id,
                "--match",
                end_prefix,
                "--source",
                "recent-unwrapped",
                "--timeout",
                str(timeout * 1000),
            ],
            check=False,
        )
        if wait.get("error"):
            message = str(wait.get("error", {}).get("message") or wait.get("error"))
            raise AppError(
                f"Timed out or failed while waiting for command on {self.cfg.control_node}: {message}\n"
                f"Command: {command}"
            )

        wait_text = str(wait.get("result", {}).get("read", {}).get("text") or "")
        result = parse_remote_envelope(wait_text, token)

        if result is None:
            pane_text = self.read(lines=output_lines)
            result = parse_remote_envelope(pane_text, token)

        if result is None:
            matched = str(wait.get("result", {}).get("matched_line") or "")
            rc = parse_remote_returncode(matched, token)
            if rc is not None:
                raise AppError(
                    "HomeStack saw the command completion marker but could not recover its "
                    f"output envelope. Command: {command}"
                )
            raise AppError(f"Could not parse HomeStack completion marker for command: {command}")

        remote_error: AppError | None = None
        if check and result.returncode != 0:
            detail = f"\n{result.output}" if result.output else ""
            remote_error = AppError(
                f"Remote command failed ({result.returncode}) on {self.cfg.control_node}: "
                f"{command}{detail}"
            )

        try:
            self.wait_for_prompt(timeout=2.0)
        except AppError:
            pass
        self.clear_pane_history()
        if remote_error is not None:
            raise remote_error
        return result

    def run_with_progress(
        self,
        command: str,
        on_output: Callable[[str], None],
        *,
        check: bool = True,
        timeout: int = 120,
        poll_interval: float = 0.5,
        output_lines: int = 4000,
    ) -> RemoteResult:
        self.wait_for_prompt()
        token = uuid.uuid4().hex
        begin = f"__HS_BEGIN__{token}"
        end_prefix = f"__HS_END__{token}:"
        wrapped = wrap_remote_command(command, token)

        run_proc = run_local(["herdr", "pane", "run", self.pane_id, wrapped], check=False)
        if run_proc.returncode != 0:
            detail = (run_proc.stderr or run_proc.stdout).strip()
            raise AppError(
                f"Herdr pane run failed ({run_proc.returncode})"
                + (f"\n{detail}" if detail else "")
            )

        deadline = time.monotonic() + timeout
        rc: int | None = None
        recent = ""
        while time.monotonic() < deadline:
            recent = strip_ansi(self.read(lines=160))
            try:
                on_output(recent)
            except Exception:
                pass

            for line in reversed(recent.splitlines()):
                text = line.strip()
                if not text.startswith(end_prefix):
                    continue
                suffix = text[len(end_prefix) :].strip()
                if suffix.isdigit():
                    rc = int(suffix)
                    break
            if rc is not None:
                break
            time.sleep(poll_interval)

        if rc is None:
            raise AppError(
                f"Timed out while waiting for command on {self.cfg.control_node} after {timeout}s\n"
                f"Command: {command}"
            )

        text = recent
        if begin not in text:
            text = strip_ansi(self.read(lines=output_lines))

        lines = text.splitlines()
        begin_index = -1
        end_index = -1
        for index, line in enumerate(lines):
            if line.strip() == begin:
                begin_index = index
        if begin_index >= 0:
            for index in range(begin_index + 1, len(lines)):
                if lines[index].strip().startswith(end_prefix):
                    end_index = index
                    break

        output = ""
        if begin_index >= 0 and end_index > begin_index:
            output = "\n".join(lines[begin_index + 1 : end_index]).strip()

        result = RemoteResult(returncode=rc, output=output)
        remote_error: AppError | None = None
        if check and rc != 0:
            detail = f"\n{output}" if output else ""
            remote_error = AppError(
                f"Remote command failed ({rc}) on {self.cfg.control_node}: {command}{detail}"
            )

        try:
            self.wait_for_prompt(timeout=2.0)
        except AppError:
            pass
        self.clear_pane_history()
        if remote_error is not None:
            raise remote_error
        return result


    def run_json_value(self, command: str, *, check: bool = True, timeout: int = 120) -> Any:
        result = self.run(command, check=check, timeout=timeout)
        if result.returncode != 0 and not check:
            return None

        try:
            value = json.loads(result.output.strip())
        except json.JSONDecodeError:
            value = None
        if isinstance(value, (dict, list)):
            return value

        candidates = [line.strip() for line in result.output.splitlines() if line.strip()]
        for line in reversed(candidates):
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, (dict, list)):
                return value
        raise AppError(f"Remote command did not return JSON data:\n{result.output}")

    def run_json(self, command: str, *, check: bool = True, timeout: int = 120) -> dict[str, Any]:
        value = self.run_json_value(command, check=check, timeout=timeout)
        if value is None and not check:
            return {}
        if not isinstance(value, dict):
            raise AppError(f"Remote command did not return a JSON object: {command}")
        return value

    def verify(self) -> dict[str, Any]:
        target = self.discover()
        self.wait_for_prompt()
        probe_command = (
            "printf '{\"ok\":true,\"host\":\"%s\",\"uid\":%s}\\n' "
            '"$(hostname -s)" "$(id -u)"'
        )
        probe = self.run_json(probe_command, timeout=5)
        host = str(probe.get("host") or "")
        uid = int(probe.get("uid", -1))
        if host != self.cfg.control_node:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} answered from host {host!r}, "
                f"expected {self.cfg.control_node!r}."
            )
        if uid != 0:
            raise AppError(
                f"Herdr tab {self.cfg.herdr_tab!r} is logged in with uid={uid}; root is required."
            )
        self._verification = {
            "type": "herdr",
            "workspace": self.cfg.herdr_workspace,
            "workspace_id": target.workspace_id,
            "tab": self.cfg.herdr_tab,
            "tab_id": target.tab_id,
            "pane": target.pane_id,
            "ssh": target.ssh_cmdline,
            "host": host,
            "uid": uid,
        }
        return dict(self._verification)

    def execution_info(self) -> dict[str, Any]:
        if self._verification is None:
            raise AppError("Herdr transport has not been verified")
        return dict(self._verification)


class PaneLock:
    def __init__(self, node: str) -> None:
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR") or "/tmp")
        self.path = runtime / f"homestack-{node}.lock"
        self.handle: Any = None

    def __enter__(self) -> "PaneLock":
        self.handle = self.path.open("a+")
        try:
            fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise AppError(
                f"Another HomeStack process is already using the {self.path.stem} Herdr session"
            ) from exc
        self.handle.seek(0)
        self.handle.truncate()
        self.handle.write(str(os.getpid()))
        self.handle.flush()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if self.handle is not None:
            try:
                fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
            finally:
                self.handle.close()


@contextmanager
def open_herdr_transport(cfg: Config) -> Iterator[HerdrSession]:
    if shutil.which("herdr") is None:
        raise AppError("Required local command 'herdr' was not found")
    with PaneLock(cfg.control_node):
        session = HerdrSession(cfg)
        session.verify()
        yield session
