"""Translate HomeStack's shell commands into canonical operations.

The vocabulary is documented in parity/ops.md and must stay identical to
the Rust implementation in crates/pve-sim/src/ops.rs. A command this module
does not know raises UnsupportedCommand, so a parity run can never silently
skip a call; later migration phases extend the translation.
"""

from __future__ import annotations

import shlex
from dataclasses import dataclass, field


class UnsupportedCommand(Exception):
    """A command with no translation yet. Deliberately not an AppError, so the
    HomeStack CLI cannot turn it into an ordinary error result."""


@dataclass(frozen=True)
class ApiCall:
    """`pvesh get PATH`."""

    method: str
    path: str
    query: dict[str, str] = field(default_factory=dict)
    node: str | None = None

    @property
    def op(self) -> str:
        params = {("query_path" if key == "path" else key): value for key, value in self.query.items()}
        return format_op(self.method, {**params, "path": self.path})

    @property
    def ops(self) -> list[str]:
        return [self.op]


@dataclass(frozen=True)
class GuestPing:
    """`qm guest cmd VMID ping`: `POST .../agent/ping`."""

    vmid: int
    node: str | None = None

    @property
    def ops(self) -> list[str]:
        return [format_op("guest_ping", {"vmid": str(self.vmid)})]


@dataclass(frozen=True)
class GuestExec:
    """`qm guest exec VMID -- /bin/sh -lc CMD`: `POST .../agent/exec`, then
    `GET .../agent/exec-status` until the command has finished."""

    vmid: int
    shell: str
    node: str | None = None

    @property
    def ops(self) -> list[str]:
        return [
            format_op("guest_exec", {"cmd": self.shell, "vmid": str(self.vmid)}),
            format_op("guest_exec_status", {"vmid": str(self.vmid)}),
        ]


Command = ApiCall | GuestPing | GuestExec

_ESCAPES = {"%": "%25", ",": "%2C", "=": "%3D", "{": "%7B", "}": "%7D"}


def _escape(value: str) -> str:
    return "".join(_ESCAPES.get(char, char) for char in value)


def format_op(name: str, params: dict[str, str]) -> str:
    return name + "{" + ",".join(f"{key}={_escape(params[key])}" for key in sorted(params)) + "}"


def translate(command: str, node: str | None = None) -> Command:
    """The API call a shell command amounts to, and the node it runs on
    (`None`: the control node)."""
    try:
        words = shlex.split(command)
    except ValueError as exc:
        raise UnsupportedCommand(command) from exc

    # proxmox.node_shell_command routes to other nodes through ssh; the API
    # proxies /nodes/{node}/... itself, so the operation is the same.
    if len(words) == 5 and words[:3] == ["ssh", "-o", "BatchMode=yes"] and words[3].startswith("root@"):
        return translate(words[4], words[3].removeprefix("root@"))

    # guest.qga_ping_command: `timeout -k 2s 5s qm guest cmd VMID ping`.
    if len(words) >= 4 and words[0] == "timeout" and words[1] == "-k":
        words = words[4:]

    if len(words) >= 3 and words[:2] == ["pvesh", "get"] and words[2].startswith("/"):
        call = _pvesh_get(command, words[2], words[3:])
        return ApiCall(call.method, call.path, call.query, node)
    if len(words) == 5 and words[:3] == ["qm", "guest", "cmd"] and words[4] == "ping" and words[3].isdigit():
        return GuestPing(int(words[3]), node)
    if len(words) == 8 and words[:3] == ["qm", "guest", "exec"] and words[3].isdigit() and words[4:7] == ["--", "/bin/sh", "-lc"]:
        return GuestExec(int(words[3]), words[7], node)
    raise UnsupportedCommand(command)


def _pvesh_get(command: str, path: str, options: list[str]) -> ApiCall:
    query: dict[str, str] = {}
    output_format = None
    index = 0
    while index < len(options):
        key = options[index]
        if not key.startswith("--") or index + 1 >= len(options):
            raise UnsupportedCommand(command)
        value = options[index + 1]
        if key == "--output-format":
            output_format = value
        else:
            query[key[2:]] = value
        index += 2
    if output_format != "json":
        raise UnsupportedCommand(command)
    return ApiCall("get", path, query)
