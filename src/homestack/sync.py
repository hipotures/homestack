"""Sync support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .config import Config, validate_sync_path_spec

def sync_plan_item(cfg: Config, configured_path: str) -> dict[str, Any]:
    relative, is_directory = validate_sync_path_spec(configured_path)
    source = Path.home() / relative
    destination = Path("/home") / cfg.user_name / relative
    expected_type = "directory" if is_directory else "file"

    status = "ready"
    detail = ""
    if source.is_symlink():
        status = "type mismatch"
        detail = "symbolic links are not supported"
    elif not source.exists():
        status = "missing"
        detail = "source does not exist"
    elif is_directory and not source.is_dir():
        status = "type mismatch"
        detail = "configured with trailing '/', but source is not a directory"
    elif not is_directory and not source.is_file():
        status = "type mismatch"
        detail = "configured as a file, but source is not a regular file"

    return {
        "path": configured_path,
        "relative": relative,
        "type": expected_type,
        "is_directory": is_directory,
        "local_path": str(source),
        "destination": str(destination) + ("/" if is_directory else ""),
        "status": status,
        "detail": detail,
    }
