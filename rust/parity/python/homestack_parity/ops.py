"""Translate HomeStack's shell commands into canonical operations.

The vocabulary is documented in rust/parity/ops.md and must stay identical to
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
    method: str
    path: str
    query: dict[str, str] = field(default_factory=dict)

    @property
    def op(self) -> str:
        return format_op(self.method, {**self.query, "path": self.path})


def format_op(name: str, params: dict[str, str]) -> str:
    return name + "{" + ",".join(f"{key}={params[key]}" for key in sorted(params)) + "}"


def translate(command: str) -> ApiCall:
    """The API call a shell command amounts to."""
    try:
        words = shlex.split(command)
    except ValueError as exc:
        raise UnsupportedCommand(command) from exc

    # proxmox.node_shell_command routes to other nodes through ssh; the API
    # proxies /nodes/{node}/... itself, so the operation is the same.
    if len(words) == 5 and words[:3] == ["ssh", "-o", "BatchMode=yes"] and words[3].startswith("root@"):
        return translate(words[4])

    if len(words) >= 3 and words[:2] == ["pvesh", "get"] and words[2].startswith("/"):
        return _pvesh_get(command, words[2], words[3:])
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
