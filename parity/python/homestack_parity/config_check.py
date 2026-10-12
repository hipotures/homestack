"""Python's verdict on configuration files, for the config parity corpus.

    python -m homestack_parity.config_check FILE...   (run from python/)

Prints one JSON object per file: ``{"file", "verdict", "message"}``. The
verdict is ``accepted``, the configuration field an ``AppError`` is about
(``repo.owner``, ``storage_layouts.homestack-storage-1``, ...), or
``crash:<ExceptionType>`` when ``load_config`` raises something other than
``AppError``. The Rust side reports the same field names
(``hs_model::config::ConfigError::field``).
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

from homestack.config import load_config
from homestack.models import AppError

# Messages that do not start with "[section] key", checked first.
SPECIAL = (
    (re.compile(r"^HomeStack configuration is incomplete"), "install_draft"),
    (re.compile(r"^Unsupported config version"), "version"),
    (re.compile(r"^Obsolete \[sync\]"), "sync"),
    (re.compile(r"^Invalid TOML configuration"), "toml"),
    (re.compile(r"^Missing configuration value \[([a-z_.]+)\] ([a-z_]+)$"), None),
    (re.compile(r"^Invalid HomeStack storage layout name"), "storage_layouts"),
    (re.compile(r"^Missing \[storage_layouts\] "), "storage_layouts"),
    (re.compile(r"^\[(storage_layouts)\] (homestack-storage-[1-9][0-9]*) "), None),
    (re.compile(r"^Unsupported transport type"), "transport.type"),
    (re.compile(r"^Invalid GitHub repository owner"), "repo.owner"),
    (re.compile(r"^Missing or invalid \[workspace_ssh\] configuration table"), "workspace_ssh"),
    (re.compile(r"^\[workspace_ssh\] user .* must match \[user\] name"), "workspace_ssh.user"),
    (re.compile(r"^Cannot derive HomeStack storage layout from node name"), "node.name"),
    (re.compile(r"^\[root\] disk and \[home\] disk must"), "root.disk"),
)
KEY = re.compile(r"^\[([a-z_.]+)\] ([a-z_]+) (?:must|contains|entries)\b")
SECTION = re.compile(r"^\[([a-z_.]+)\] (?:must|has)\b")


def field(message: str) -> str:
    for pattern, name in SPECIAL:
        match = pattern.search(message)
        if match is None:
            continue
        if name is not None:
            return name
        return ".".join(match.groups())
    for pattern in (KEY, SECTION):
        match = pattern.search(message)
        if match is not None:
            return ".".join(match.groups())
    return "unmapped: " + message


def verdict(path: Path) -> dict[str, str]:
    try:
        load_config(path)
    except AppError as exc:
        return {"file": str(path), "verdict": field(str(exc)), "message": str(exc)}
    except Exception as exc:  # noqa: BLE001 - the crash itself is the verdict
        return {"file": str(path), "verdict": f"crash:{type(exc).__name__}", "message": str(exc)}
    return {"file": str(path), "verdict": "accepted", "message": ""}


if __name__ == "__main__":
    for name in sys.argv[1:]:
        print(json.dumps(verdict(Path(name)), sort_keys=True))
