"""Transport contract used by HomeStack's remote operations."""

from __future__ import annotations

import shlex
import subprocess
from typing import TYPE_CHECKING, Any, Callable, Protocol, runtime_checkable

from ..models import AppError, RemoteResult

if TYPE_CHECKING:
    from ..config import Config


def run_local(cmd: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run a desktop command and capture its output."""
    proc = subprocess.run(cmd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and proc.returncode != 0:
        detail = (proc.stderr or proc.stdout).strip()
        raise AppError(
            f"Local command failed ({proc.returncode}): {shlex.join(cmd)}"
            + (f"\n{detail}" if detail else "")
        )
    return proc


def run_local_passthrough(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    """Run an interactive desktop command without capturing its streams."""
    return subprocess.run(cmd, text=True)


@runtime_checkable
class Transport(Protocol):
    """Operations required by the existing Proxmox workflow."""

    cfg: Config

    def run(
        self,
        command: str,
        *,
        check: bool = True,
        timeout: int = 120,
        output_lines: int = 4000,
    ) -> RemoteResult: ...

    def run_with_progress(
        self,
        command: str,
        on_output: Callable[[str], None],
        *,
        check: bool = True,
        timeout: int = 120,
        poll_interval: float = 0.5,
        output_lines: int = 4000,
    ) -> RemoteResult: ...

    def run_json_value(
        self, command: str, *, check: bool = True, timeout: int = 120
    ) -> Any: ...

    def run_json(
        self, command: str, *, check: bool = True, timeout: int = 120
    ) -> dict[str, Any]: ...

    def execution_info(self) -> dict[str, Any]: ...
