"""Models support for HomeStack."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any
import re

class AppError(RuntimeError):
    pass


@dataclass(frozen=True)
class RemoteResult:
    returncode: int
    output: str


GOLD_TAG = "homestack-gold"


WORKSPACE_TAG = "homestack-ws"


HOME_LABEL_PREFIX = "HS_HOME_"


def integer_value(value: Any) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def validate_name(name: str) -> None:
    if not re.fullmatch(r"[A-Za-z][A-Za-z0-9.-]{0,62}", name):
        raise AppError(
            "Invalid VM name. Use 1-63 characters: letters, digits, dot and hyphen; "
            "the first character must be a letter."
        )
