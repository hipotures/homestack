"""Gold VM readiness checks for HomeStack."""

from __future__ import annotations

from dataclasses import dataclass
import re
import shlex
from typing import Callable
from urllib.parse import unquote

from .config import Config
from .guest import extract_mac, guest_out_on_node, qga_ping_command
from .models import AppError, GOLD_TAG, WORKSPACE_TAG
from .proxmox import (
    boot_order_contains_disk,
    disk_option,
    has_tag,
    node_run,
    qm_config_on_node,
    qm_status_on_node,
)
from .transports.base import Transport
from .workspace_ssh import validate_authorized_keys


@dataclass(frozen=True)
class GoldCheck:
    scope: str
    name: str
    status: str
    detail: str
    requirement: str = "required"


@dataclass(frozen=True)
class GoldReadiness:
    vmid: int
    node: str
    power_state: str
    checks: tuple[GoldCheck, ...]

    @property
    def failures(self) -> tuple[GoldCheck, ...]:
        return tuple(
            check
            for check in self.checks
            if check.status == "fail" and check.requirement == "required"
        )

    @property
    def optional_failures(self) -> tuple[GoldCheck, ...]:
        return tuple(
            check
            for check in self.checks
            if check.status == "fail" and check.requirement == "optional"
        )

    @property
    def ok(self) -> bool:
        return not self.failures

    @property
    def guest_checked(self) -> bool:
        return any(
            check.scope == "guest" and check.status != "skip"
            for check in self.checks
        )


def _agent_enabled(value: str | None) -> bool:
    text = str(value or "").strip()
    if text == "1":
        return True
    return any(
        token.strip().lower() in {"enabled=1", "enabled=true"}
        for token in text.split(",")
    )


def _cloud_init_slots(vm_cfg: dict[str, str]) -> list[str]:
    slots: list[str] = []
    for slot, raw in vm_cfg.items():
        if re.fullmatch(r"(?:ide|sata|scsi|virtio)[0-9]+", slot) is None:
            continue
        value = str(raw)
        volume = value.split(",", 1)[0].strip()
        if "cloudinit" in volume and disk_option(value, "media") == "cdrom":
            slots.append(slot)
    return sorted(slots)


def _extra_data_disks(vm_cfg: dict[str, str], cfg: Config) -> list[str]:
    extras: list[str] = []
    cloud_init_slots = set(_cloud_init_slots(vm_cfg))
    for slot, raw in vm_cfg.items():
        if re.fullmatch(r"(?:ide|sata|scsi|virtio)[0-9]+", slot) is None:
            continue
        if slot == cfg.root_disk or slot in cloud_init_slots:
            continue
        volume = str(raw).split(",", 1)[0].strip()
        if volume and volume != "none":
            extras.append(slot)
    return sorted(extras)


def _extra_nics(vm_cfg: dict[str, str]) -> list[str]:
    return sorted(
        key
        for key, value in vm_cfg.items()
        if re.fullmatch(r"net[1-9][0-9]*", key) and str(value).strip()
    )


def _pve_ssh_keys_usable(vm_cfg: dict[str, str]) -> tuple[bool, str]:
    raw = vm_cfg.get("sshkeys")
    if not raw:
        return False, "no Proxmox sshkeys configured"
    try:
        keys = validate_authorized_keys(unquote(raw))
    except AppError as exc:
        return False, str(exc)
    count = len(keys.splitlines())
    return True, f"{count} public key(s) in Proxmox sshkeys"


def check_gold_readiness(
    session: Transport,
    cfg: Config,
    node: str,
    vmid: int,
    *,
    progress: Callable[[str, int, int], None] | None = None,
) -> GoldReadiness:
    """Verify the non-destructive Gold contract required by current HomeStack behavior."""
    total = 6
    checks: list[GoldCheck] = []

    def update(description: str, completed: int) -> None:
        if progress is not None:
            progress(description, completed, total)

    def add(
        scope: str,
        name: str,
        ok: bool,
        detail: str,
        *,
        requirement: str = "required",
    ) -> None:
        checks.append(
            GoldCheck(
                scope,
                name,
                "pass" if ok else "fail",
                detail,
                requirement=requirement,
            )
        )

    update("Read Gold Proxmox configuration", 0)
    vm_cfg = qm_config_on_node(session, cfg, node, vmid)

    tags_ok = has_tag(vm_cfg.get("tags"), GOLD_TAG) and not has_tag(
        vm_cfg.get("tags"), WORKSPACE_TAG
    )
    add("PVE", "Role tag", tags_ok, f"requires {GOLD_TAG} and not {WORKSPACE_TAG}")

    root_ok = bool(vm_cfg.get(cfg.root_disk))
    add("PVE", "Disposable root", root_ok, cfg.root_disk)

    extra_disks = _extra_data_disks(vm_cfg, cfg)
    add(
        "PVE",
        "Extra data disks",
        not extra_disks,
        "none" if not extra_disks else ", ".join(extra_disks),
    )

    net0 = vm_cfg.get("net0", "")
    try:
        mac = extract_mac(net0) if net0 else ""
    except AppError:
        mac = ""
    add("PVE", "Primary NIC", bool(mac), f"net0 {mac}" if mac else "net0 missing or invalid")

    extra_nics = _extra_nics(vm_cfg)
    add(
        "PVE",
        "Extra NICs",
        not extra_nics,
        "none" if not extra_nics else ", ".join(extra_nics),
    )

    cloud_slots = _cloud_init_slots(vm_cfg)
    add(
        "PVE",
        "Cloud-Init drive",
        bool(cloud_slots),
        ", ".join(cloud_slots) if cloud_slots else "missing",
    )

    add(
        "PVE",
        "QEMU Guest Agent option",
        _agent_enabled(vm_cfg.get("agent")),
        str(vm_cfg.get("agent") or "disabled"),
    )

    boot = vm_cfg.get("boot", "")
    add(
        "PVE",
        "Boot order",
        boot_order_contains_disk(boot, cfg.root_disk),
        boot or "not configured",
    )
    update("Read Gold power state", 1)

    power_state = qm_status_on_node(session, cfg, node, vmid)
    add(
        "PVE",
        "Power state",
        power_state in {"running", "stopped"},
        power_state,
    )

    pve_keys_ok, pve_keys_detail = _pve_ssh_keys_usable(vm_cfg)

    if power_state != "running":
        add(
            "PVE",
            "Workspace public-key source",
            pve_keys_ok,
            pve_keys_detail + "; required while Gold is stopped",
        )
        for scope, name in (
            ("guest", "QEMU Guest Agent response"),
            ("guest", "Required guest tools"),
            ("guest", "Workspace account"),
            ("security", "Regular user policy"),
            ("security", "No sudo"),
            ("security", "Root authorized_keys"),
        ):
            checks.append(
                GoldCheck(
                    scope,
                    name,
                    "skip",
                    f"not inspected because Gold is {power_state}",
                )
            )
        checks.append(
            GoldCheck(
                "guest",
                "rsync for homestack sync",
                "skip",
                f"not inspected because Gold is {power_state}",
                requirement="optional",
            )
        )
        checks.append(
            GoldCheck(
                "guest",
                "git/ssh-keygen for homestack repo",
                "skip",
                f"not inspected because Gold is {power_state}",
                requirement="optional",
            )
        )
        update("Gold guest inspection skipped", total)
        return GoldReadiness(vmid, node, power_state, tuple(checks))

    update("Check QEMU Guest Agent", 2)
    qga = node_run(
        session,
        cfg,
        node,
        qga_ping_command(vmid),
        check=False,
        timeout=10,
    )
    qga_ok = qga.returncode == 0
    add(
        "guest",
        "QEMU Guest Agent response",
        qga_ok,
        "responding" if qga_ok else (qga.output.strip() or f"exit {qga.returncode}"),
    )
    if not qga_ok:
        for scope, name in (
            ("guest", "Required guest tools"),
            ("guest", "Workspace account"),
            ("security", "Regular user policy"),
            ("security", "No sudo"),
            ("security", "Root authorized_keys"),
            ("security", "Workspace public-key source"),
        ):
            checks.append(
                GoldCheck(scope, name, "skip", "QEMU Guest Agent unavailable")
            )
        checks.append(
            GoldCheck(
                "guest",
                "rsync for homestack sync",
                "skip",
                "QEMU Guest Agent unavailable",
                requirement="optional",
            )
        )
        checks.append(
            GoldCheck(
                "guest",
                "git/ssh-keygen for homestack repo",
                "skip",
                "QEMU Guest Agent unavailable",
                requirement="optional",
            )
        )
        update("Gold readiness check failed", total)
        return GoldReadiness(vmid, node, power_state, tuple(checks))

    update("Check required Gold guest tools", 3)
    required_tools = (
        "cloud-init",
        "sshd",
        "nmcli",
        "lsblk",
        "blkid",
        "wipefs",
        "mkfs.ext4",
        "mount",
        "mountpoint",
        "systemctl",
        "systemd-escape",
        "install",
    )
    tool_words = " ".join(shlex.quote(tool) for tool in required_tools)
    missing_tools = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "for c in "
        + tool_words
        + '; do command -v "$c" >/dev/null 2>&1 || printf "%s\\n" "$c"; done',
        check=False,
    ).splitlines()
    add(
        "guest",
        "Required guest tools",
        not missing_tools,
        "all present" if not missing_tools else "missing: " + ", ".join(missing_tools),
    )
    rsync_state = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "if command -v rsync >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
        check=False,
    )
    add(
        "guest",
        "rsync for homestack sync",
        rsync_state == "PRESENT",
        "installed" if rsync_state == "PRESENT" else "not installed; sync will be unavailable",
        requirement="optional",
    )
    repo_tool_words = " ".join(shlex.quote(tool) for tool in ("git", "ssh-keygen"))
    missing_repo_tools = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "for c in "
        + repo_tool_words
        + '; do command -v "$c" >/dev/null 2>&1 || printf "%s\\n" "$c"; done',
        check=False,
    ).splitlines()
    add(
        "guest",
        "git/ssh-keygen for homestack repo",
        not missing_repo_tools,
        "all present"
        if not missing_repo_tools
        else "missing: " + ", ".join(missing_repo_tools),
        requirement="optional",
    )

    update("Check Gold accounts and privilege policy", 4)
    account = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        f"getent passwd {shlex.quote(cfg.user_name)} || true",
        check=False,
    )
    fields = account.split(":")
    account_ok = (
        len(fields) >= 4
        and fields[0] == cfg.user_name
        and fields[2] == str(cfg.user_uid)
        and fields[3] == str(cfg.user_gid)
    )
    add(
        "guest",
        "Workspace account",
        account_ok,
        f"{cfg.user_name} ({cfg.user_uid}:{cfg.user_gid})",
    )

    regular_users = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        """awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd""",
        check=False,
    ).splitlines()
    expected_regular = f"{cfg.user_name}:{cfg.user_uid}:{cfg.user_gid}"
    add(
        "security",
        "Regular user policy",
        regular_users == [expected_regular],
        ", ".join(regular_users) if regular_users else "none",
    )

    sudo_state = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "if command -v sudo >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
        check=False,
    )
    add(
        "security",
        "No sudo",
        sudo_state == "ABSENT",
        "not installed" if sudo_state == "ABSENT" else "installed",
    )

    update("Check Gold SSH public-key sources", 5)
    root_keys = guest_out_on_node(
        session,
        cfg,
        node,
        vmid,
        "test -s /root/.ssh/authorized_keys && echo OK || echo MISSING",
        check=False,
    )
    add(
        "security",
        "Root authorized_keys",
        root_keys == "OK",
        "present" if root_keys == "OK" else "missing",
    )

    user_key_detail = pve_keys_detail
    user_keys_ok = pve_keys_ok
    if not user_keys_ok:
        user_keys = guest_out_on_node(
            session,
            cfg,
            node,
            vmid,
            f"cat /home/{shlex.quote(cfg.user_name)}/.ssh/authorized_keys 2>/dev/null || true",
            check=False,
        )
        try:
            validated = validate_authorized_keys(user_keys)
            user_keys_ok = True
            user_key_detail = (
                f"{len(validated.splitlines())} public key(s) in "
                f"/home/{cfg.user_name}/.ssh/authorized_keys"
            )
        except AppError as exc:
            user_key_detail = str(exc)
    add("security", "Workspace public-key source", user_keys_ok, user_key_detail)

    update("Gold readiness check complete", total)
    return GoldReadiness(vmid, node, power_state, tuple(checks))
