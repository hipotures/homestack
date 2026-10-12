"""Transports that run HomeStack against the Proxmox simulator.

FixtureTransport answers the commands HomeStack would run on a node by calling
pve-sim's HTTPS API, so the scenario state lives in one place and both
implementations see the same cluster. RecordingTransport wraps any transport
and keeps the commands it ran and their canonical operations.
"""

from __future__ import annotations

import json
import ssl
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable, Protocol

from homestack.models import AppError, RemoteResult

from . import ops

# pvesh exit codes, observed on PVE 9.2: an API error ends with 255, a path
# with no handler with 1.
PVESH_API_ERROR = 255
PVESH_NO_HANDLER = 1


class Api(Protocol):
    def get(self, path: str, query: dict[str, str]) -> tuple[int, Any]: ...


class HttpsApi:
    """GET requests to /api2/json with an API token, verifying the simulator's CA."""

    def __init__(self, base_url: str, ca_file: Path, token: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.context = ssl.create_default_context(cafile=str(ca_file))
        self.token = token

    def get(self, path: str, query: dict[str, str]) -> tuple[int, Any]:
        url = self.base_url + "/api2/json" + urllib.parse.quote(path)
        if query:
            url += "?" + urllib.parse.urlencode(sorted(query.items()))
        request = urllib.request.Request(url, headers={"Authorization": f"PVEAPIToken={self.token}"})
        try:
            with urllib.request.urlopen(request, context=self.context, timeout=30) as response:
                return response.status, _json_or_none(response.read())
        except urllib.error.HTTPError as error:
            return error.code, _json_or_none(error.read())


def _json_or_none(body: bytes) -> Any:
    try:
        return json.loads(body)
    except ValueError:
        return None


class FixtureTransport:
    """The Transport protocol, answered by the simulator."""

    def __init__(self, cfg: Any, api: Api, sim_url: str) -> None:
        self.cfg = cfg
        self.api = api
        self.sim_url = sim_url

    def run(self, command: str, *, check: bool = True, timeout: int = 120, output_lines: int = 4000) -> RemoteResult:
        call = ops.translate(command)
        status, body = self.api.get(call.path, call.query)
        if 200 <= status < 300:
            data = body.get("data") if isinstance(body, dict) else None
            result = RemoteResult(0, json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
        elif status == 501:
            result = RemoteResult(PVESH_NO_HANDLER, f"No '{call.method}' handler defined for '{call.path}'")
        else:
            message = body.get("message") if isinstance(body, dict) else None
            result = RemoteResult(PVESH_API_ERROR, str(message or status).rstrip("\n"))
        if check and result.returncode != 0:
            node = getattr(self.cfg, "control_node", "fixture")
            detail = f"\n{result.output}" if result.output else ""
            raise AppError(f"Remote command failed ({result.returncode}) on {node}: {command}{detail}")
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
        result = self.run(command, check=check, timeout=timeout)
        for line in result.output.splitlines():
            on_output(line)
        return result

    def run_json_value(self, command: str, *, check: bool = True, timeout: int = 120) -> Any:
        result = self.run(command, check=check, timeout=timeout)
        if result.returncode != 0 and not check:
            return None
        try:
            value = json.loads(result.output)
        except ValueError:
            value = None
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

    def execution_info(self) -> dict[str, Any]:
        return {"type": "fixture", "sim": self.sim_url}


class RecordingTransport:
    """Delegates to another transport and records what it was asked to run."""

    def __init__(self, inner: Any) -> None:
        self.inner = inner
        self.commands: list[str] = []
        self.ops: list[str] = []

    @property
    def cfg(self) -> Any:
        return self.inner.cfg

    def _record(self, command: str) -> None:
        self.commands.append(command)
        self.ops.append(ops.translate(command).op)

    def run(self, command: str, **kwargs: Any) -> RemoteResult:
        self._record(command)
        return self.inner.run(command, **kwargs)

    def run_with_progress(self, command: str, on_output: Callable[[str], None], **kwargs: Any) -> RemoteResult:
        self._record(command)
        return self.inner.run_with_progress(command, on_output, **kwargs)

    def run_json_value(self, command: str, **kwargs: Any) -> Any:
        self._record(command)
        return self.inner.run_json_value(command, **kwargs)

    def run_json(self, command: str, **kwargs: Any) -> dict[str, Any]:
        self._record(command)
        return self.inner.run_json(command, **kwargs)

    def execution_info(self) -> dict[str, Any]:
        return self.inner.execution_info()
