"""Guest support for HomeStack."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable
import json
import re
import shlex
import time

from .config import Config
from .models import AppError
from .proxmox import home_label, node_run
from .transports.base import Transport

def remote_path_exists_on_node(
    session: Transport, cfg: Config, node: str, path: Path
) -> bool:
    return node_run(
        session,
        cfg,
        node,
        f"test -e {shlex.quote(str(path))}",
        check=False,
    ).returncode == 0


def parse_qm_guest_exec(stdout: str) -> dict[str, Any]:
    text = stdout.strip()
    candidates = [line.strip() for line in text.splitlines() if line.strip()]
    for line in reversed(candidates):
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict):
            return obj

    first = text.find("{")
    last = text.rfind("}")
    if first >= 0 and last > first:
        try:
            obj = json.loads(text[first : last + 1])
            if isinstance(obj, dict):
                return obj
        except json.JSONDecodeError:
            pass

    raise AppError(f"Could not parse QEMU Guest Agent response:\n{text}")


def _complete_guest_exec(
    run: Callable[..., Any],
    vmid: int,
    shell_command: str,
    *,
    check: bool,
    timeout: int,
    context: str,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    command = f"qm guest exec {vmid} -- /bin/sh -lc {shlex.quote(shell_command)}"
    result = run(command, check=False, timeout=min(60, timeout))
    if result.returncode != 0:
        if check:
            raise AppError(f"QEMU Guest Agent exec failed for {context}: {result.output}")
        return {
            "exited": 1,
            "exitcode": result.returncode,
            "out-data": result.output,
            "err-data": "",
        }

    parsed = parse_qm_guest_exec(result.output)
    if "pid" in parsed and not parsed.get("exited"):
        try:
            pid = int(parsed["pid"])
        except (TypeError, ValueError) as exc:
            raise AppError(f"QEMU Guest Agent returned an invalid pid for {context}") from exc
        while time.monotonic() < deadline:
            status = run(
                f"qm guest exec-status {vmid} {pid}",
                check=False,
                timeout=min(30, max(1, int(deadline - time.monotonic()))),
            )
            if status.returncode != 0:
                if check:
                    raise AppError(
                        f"QEMU Guest Agent status failed for {context}: {status.output}"
                    )
                return {
                    "exited": 1,
                    "exitcode": status.returncode,
                    "out-data": status.output,
                    "err-data": "",
                }
            parsed = parse_qm_guest_exec(status.output)
            if parsed.get("exited"):
                break
            time.sleep(0.2)
        else:
            raise AppError(
                f"Guest command did not finish in {context} within {timeout}s: {shell_command}"
            )

    if "exited" not in parsed or not parsed.get("exited") or "exitcode" not in parsed:
        raise AppError(f"QEMU Guest Agent returned an incomplete response for {context}: {parsed}")
    try:
        exitcode = int(parsed["exitcode"])
    except (TypeError, ValueError) as exc:
        raise AppError(f"QEMU Guest Agent returned an invalid exit code for {context}") from exc
    exited = int(parsed["exited"] or 0)
    if check and (not exited or exitcode != 0):
        out = str(parsed.get("out-data", "")).strip()
        err = str(parsed.get("err-data", "")).strip()
        raise AppError(
            f"Guest command failed in {context} (exit={exitcode}): {shell_command}"
            + (f"\n{err or out}" if (err or out) else "")
        )
    return parsed


def qga_ping_command(vmid: int) -> str:
    return f"timeout -k 2s 5s qm guest cmd {vmid} ping"


def derive_ip(cfg: Config, vmid: int) -> str:
    if not 2 <= vmid <= 254:
        raise AppError(
            f"VMID {vmid} cannot be mapped to {cfg.network_prefix}.<vmid>; "
            "this addressing scheme requires VMID 2..254."
        )
    return f"{cfg.network_prefix}.{vmid}"


def parse_disk_size_gb(disk_config: str) -> float | None:
    match = re.search(r"(?:^|,)size=([0-9.]+)([KMGT])(?:,|$)", disk_config)
    if not match:
        return None
    value = float(match.group(1))
    unit = match.group(2)
    factors = {"K": 1 / (1024 * 1024), "M": 1 / 1024, "G": 1.0, "T": 1024.0}
    return value * factors[unit]


def extract_mac(net0: str) -> str:
    match = re.search(r"\b(?:virtio|e1000|rtl8139)=([0-9A-Fa-f:]{17})\b", net0)
    if not match:
        match = re.search(r"\b([0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5})\b", net0)
    if not match:
        raise AppError(f"Cannot parse MAC address from net0: {net0!r}")
    return match.group(1).upper()


def parse_ipconfig0(value: str) -> dict[str, Any]:
    fields: dict[str, str] = {}
    for item in value.split(","):
        if "=" not in item:
            continue
        key, raw = item.split("=", 1)
        fields[key.strip()] = raw.strip()

    ip_value = fields.get("ip", "")
    if not ip_value or ip_value == "dhcp" or "/" not in ip_value:
        raise AppError(f"Workspace ipconfig0 does not contain a static IPv4 address: {value!r}")
    address, cidr_text = ip_value.rsplit("/", 1)
    try:
        cidr = int(cidr_text)
    except ValueError as exc:
        raise AppError(f"Cannot parse CIDR from ipconfig0: {value!r}") from exc
    gateway = fields.get("gw")
    if not gateway:
        raise AppError(f"Workspace ipconfig0 does not contain a gateway: {value!r}")
    return {"ip": address, "cidr": cidr, "gateway": gateway}


def wait_for_qga_on_node(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    timeout: int = 300,
) -> None:
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        try:
            result = node_run(
                session,
                cfg,
                node,
                qga_ping_command(vmid),
                check=False,
                timeout=10,
            )
        except AppError as exc:
            last = str(exc)
        else:
            if result.returncode == 0:
                return
            last = result.output.strip() or f"exit={result.returncode}"
        time.sleep(2)
    raise AppError(
        f"VM {vmid} on {node} did not expose a working QEMU Guest Agent within {timeout}s"
        + (f": {last}" if last else "")
    )


def workspace_home_usage(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    status: str,
    disk_config: str,
) -> dict[str, Any]:
    size_gb = parse_disk_size_gb(disk_config)
    size_bytes = int(size_gb * 1024**3) if size_gb is not None else None
    result: dict[str, Any] = {
        "size_bytes": size_bytes,
        "used_bytes": None,
        "free_bytes": None,
        "free_percent": None,
    }
    if status != "running":
        return result
    ping = node_run(session, cfg, node, f"qm guest cmd {vmid} ping", check=False)
    if ping.returncode != 0:
        return result
    mounted_label = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        f"findmnt -n -o SOURCE /home/{cfg.user_name} 2>/dev/null | "
        "xargs -r blkid -s LABEL -o value",
        check=False,
    )
    if mounted_label != home_label(vmid):
        return result
    output = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        f"df -B1 --output=size,used,avail,pcent /home/{cfg.user_name} 2>/dev/null | tail -n1",
        check=False,
    )
    parts = output.split()
    if len(parts) != 4:
        return result
    try:
        total = int(parts[0])
        used = int(parts[1])
        free = int(parts[2])
        percent_used = float(parts[3].rstrip("%"))
    except ValueError:
        return result
    result.update(
        {
            "size_bytes": total,
            "used_bytes": used,
            "free_bytes": free,
            "free_percent": max(0.0, 100.0 - percent_used),
        }
    )
    return result


def guest_exec_on_node(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    shell_command: str,
    *,
    check: bool = True,
    timeout: int = 300,
) -> dict[str, Any]:
    def run(command: str, **kwargs: Any) -> Any:
        return node_run(session, cfg, node, command, **kwargs)

    return _complete_guest_exec(
        run,
        vmid,
        shell_command,
        check=check,
        timeout=timeout,
        context=f"VM {vmid} on {node}",
    )


def guest_out_on_node(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    shell_command: str,
    *,
    check: bool = True,
    timeout: int = 300,
) -> str:
    result = guest_exec_on_node(
        session,
        cfg,
        node,
        vmid,
        shell_command,
        check=check,
        timeout=timeout,
    )
    return str(result.get("out-data", "")).strip()
