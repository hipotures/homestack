"""Cloudinit support for HomeStack."""

from __future__ import annotations

from pathlib import Path
import base64
import shlex
import uuid

from .config import Config
from .guest import remote_path_exists
from .models import AppError, integer_value
from .proxmox import qm_config_on_node
from .transports.base import Transport

def remote_write_text(session: Transport, path: Path, content: str, mode: int) -> None:
    encoded = base64.b64encode(content.encode("utf-8")).decode("ascii")
    command = (
        f"printf %s {shlex.quote(encoded)} | base64 -d > {shlex.quote(str(path))} && "
        f"chmod {mode:o} {shlex.quote(str(path))}"
    )
    session.run(command)


def snippet_names(name: str) -> dict[str, str]:
    return {
        "user": f"homestack-{name}-user.yaml",
        "vendor": f"homestack-{name}-vendor.yaml",
        "network": f"homestack-{name}-network.yaml",
        "meta": f"homestack-{name}-meta.yaml",
    }


def write_snippets(
    session: Transport,
    cfg: Config,
    name: str,
    vmid: int,
    mac: str,
    ip: str,
    home_fs_label: str,
    *,
    authorized_keys: str | None = None,
    replace: bool = False,
    preserve_home: bool = False,
    cidr: int | None = None,
    gateway: str | None = None,
) -> dict[str, Path]:
    session.run(f"mkdir -p {shlex.quote(str(cfg.snippet_dir))}")
    names = snippet_names(name)
    paths = {kind: cfg.snippet_dir / filename for kind, filename in names.items()}

    if not replace:
        for snippet_path in paths.values():
            if remote_path_exists(session, snippet_path):
                raise AppError(f"Cloud-Init snippet already exists: {snippet_path}")

    if not preserve_home and not authorized_keys:
        raise AppError("Workspace create requires at least one user SSH public key")

    user_data = f"""#cloud-config
users: []
disable_root: false
preserve_hostname: false
hostname: {name}
manage_etc_hosts: true
"""

    allow_format = "0" if preserve_home else "1"
    auth_b64 = (
        base64.b64encode((authorized_keys or "").encode("utf-8")).decode("ascii")
        if not preserve_home
        else ""
    )
    home_path = f"/home/{cfg.user_name}"

    vendor_data = f"""#cloud-config
runcmd:
  - |
      set -eu
      expected_serial={shlex.quote(home_fs_label)}
      expected_label={shlex.quote(home_fs_label)}
      home_path={shlex.quote(home_path)}
      allow_format={allow_format}

      devices="$(lsblk -dn -o PATH,SERIAL | awk -v s="$expected_serial" '$2 == s {{ print $1 }}')"
      device_count="$(printf '%s\\n' "$devices" | sed '/^$/d' | wc -l)"
      if [ "$device_count" -ne 1 ]; then
          echo "HomeStack: expected exactly one disk with serial $expected_serial, found $device_count" >&2
          exit 70
      fi
      device="$devices"

      fstype="$(blkid -s TYPE -o value "$device" 2>/dev/null || true)"
      fslabel="$(blkid -s LABEL -o value "$device" 2>/dev/null || true)"

      if [ "$fstype" = "ext4" ] && [ "$fslabel" = "$expected_label" ]; then
          :
      elif [ -z "$fstype" ] && [ -z "$fslabel" ]; then
          if [ "$allow_format" != "1" ]; then
              echo "HomeStack: persistent home is blank during refresh; refusing mkfs" >&2
              exit 71
          fi
          if [ "$(lsblk -nr -o TYPE "$device" | wc -l)" -ne 1 ]; then
              echo "HomeStack: new home disk has child block devices; refusing mkfs" >&2
              exit 72
          fi
          if [ -n "$(wipefs -n "$device" 2>/dev/null)" ]; then
              echo "HomeStack: new home disk has filesystem signatures; refusing mkfs" >&2
              exit 73
          fi
          mkfs.ext4 -m 0 -L "$expected_label" "$device"
      else
          echo "HomeStack: unexpected filesystem on $device: type=$fstype label=$fslabel; refusing changes" >&2
          exit 74
      fi

      systemctl disable --now home-user.mount >/dev/null 2>&1 || true
      rm -f /etc/systemd/system/home-user.mount
      mkdir -p "$home_path"
      sed -i "\\|[[:space:]]$home_path[[:space:]]|d" /etc/fstab
      printf '\\nLABEL=%s %s ext4 defaults 0 2\\n' "$expected_label" "$home_path" >> /etc/fstab
      systemctl daemon-reload
      mountpoint -q "$home_path" || mount "$home_path"

      if [ "$allow_format" = "1" ]; then
          rm -rf -- "$home_path/lost+found"
          chown {cfg.user_uid}:{cfg.user_gid} "$home_path"
          chmod 700 "$home_path"
          install -d -m 700 -o {cfg.user_uid} -g {cfg.user_gid} "$home_path/.ssh"
          printf %s {shlex.quote(auth_b64)} | base64 -d > "$home_path/.ssh/authorized_keys"
          chown {cfg.user_uid}:{cfg.user_gid} "$home_path/.ssh/authorized_keys"
          chmod 600 "$home_path/.ssh/authorized_keys"
      fi
"""

    network_cidr = cfg.network_cidr if cidr is None else cidr
    network_gateway = cfg.gateway if gateway is None else gateway
    dns_yaml = "\n".join(f"          - {server}" for server in cfg.dns_servers)
    network_data = f"""version: 2
ethernets:
  workspace:
    match:
      macaddress: "{mac.lower()}"
    set-name: eth0
    renderer: NetworkManager
    dhcp4: false
    dhcp6: false
    addresses:
      - {ip}/{network_cidr}
    routes:
      - to: 0.0.0.0/0
        via: {network_gateway}
    nameservers:
      addresses:
{dns_yaml}
"""

    meta_data = f"""instance-id: {name}-{vmid}-{uuid.uuid4().hex[:12]}
local-hostname: {name}
"""

    contents = {
        "user": user_data,
        "vendor": vendor_data,
        "network": network_data,
        "meta": meta_data,
    }
    for kind, snippet_path in paths.items():
        remote_write_text(session, snippet_path, contents[kind], 0o644)

    return paths


def snippet_references(
    session: Transport,
    cfg: Config,
    name: str,
) -> list[int]:
    expected = {
        f"{cfg.snippet_storage}:snippets/{filename}"
        for filename in snippet_names(name).values()
    }
    data = session.run_json_value(
        "pvesh get /cluster/resources --type vm --output-format json",
        timeout=30,
    )
    if not isinstance(data, list):
        raise AppError("Proxmox VM inventory did not return a JSON array")

    references: list[int] = []
    for item in data:
        if not isinstance(item, dict) or item.get("type") != "qemu":
            continue
        vmid = integer_value(item.get("vmid"))
        node = str(item.get("node") or "")
        if vmid is None or not node:
            continue
        vm_cfg = qm_config_on_node(session, cfg, node, vmid)
        cicustom = vm_cfg.get("cicustom", "")
        if any(token in cicustom for token in expected):
            references.append(vmid)
    return sorted(set(references))


def stale_create_snippets(
    session: Transport,
    cfg: Config,
    name: str,
) -> list[Path]:
    paths = [
        cfg.snippet_dir / filename
        for filename in snippet_names(name).values()
    ]
    existing = [path for path in paths if remote_path_exists(session, path)]
    if not existing:
        return []

    references = snippet_references(session, cfg, name)
    if references:
        raise AppError(
            f"Cloud-Init snippets for workspace name {name!r} already exist and are "
            f"referenced by VMIDs {references}; refusing to overwrite them."
        )
    return existing


def remove_stale_create_snippets(
    session: Transport,
    cfg: Config,
    name: str,
    expected_paths: list[str],
) -> None:
    current = stale_create_snippets(session, cfg, name)
    current_strings = sorted(str(path) for path in current)
    expected = sorted(expected_paths)
    if current_strings != expected:
        raise AppError(
            "Stale Cloud-Init snippet state changed after confirmation; "
            "re-run create and review the new plan."
        )
    for path in current:
        session.run(f"rm -f {shlex.quote(str(path))}")
        if remote_path_exists(session, path):
            raise AppError(f"Failed to remove stale Cloud-Init snippet: {path}")


def cicustom_value(cfg: Config, name: str) -> str:
    names = snippet_names(name)
    return ",".join(
        [
            f"user={cfg.snippet_storage}:snippets/{names['user']}",
            f"vendor={cfg.snippet_storage}:snippets/{names['vendor']}",
            f"network={cfg.snippet_storage}:snippets/{names['network']}",
            f"meta={cfg.snippet_storage}:snippets/{names['meta']}",
        ]
    )


def sync_snippets_to_node(
    session: Transport,
    cfg: Config,
    source_node: str,
    target_node: str,
    name: str,
) -> list[str]:
    """Relay snippets through the HomeStack control node.

    This deliberately avoids source-node -> target-node SSH. Only the control node
    needs SSH trust to the cluster nodes, which is already required by HomeStack.
    """
    names = snippet_names(name)
    paths = [cfg.snippet_dir / filename for filename in names.values()]
    quoted_paths = " ".join(shlex.quote(str(path)) for path in paths)
    temp_paths = " ".join(
        f'"$hs_tmp/{filename}"'
        for filename in names.values()
    )

    source_check = (
        f'for f in {quoted_paths}; do test -s "$f" || exit 81; done'
    )
    if source_node == cfg.control_node:
        fetch = (
            f"{source_check} && "
            f"cp -p {quoted_paths} \"$hs_tmp/\""
        )
    else:
        source = shlex.quote(f"root@{source_node}")
        remote_specs = " ".join(
            shlex.quote(f"root@{source_node}:{path}")
            for path in paths
        )
        fetch = (
            f"ssh -o BatchMode=yes {source} {shlex.quote(source_check)} && "
            f"scp -q -p {remote_specs} \"$hs_tmp/\""
        )

    target_mkdir = f"mkdir -p {shlex.quote(str(cfg.snippet_dir))}"
    target_check = (
        "for f in "
        + " ".join(shlex.quote(str(path)) for path in paths)
        + '; do test -s "$f" || exit 82; done'
    )
    if target_node == cfg.control_node:
        deliver = (
            f"{target_mkdir} && "
            f"cp -p {temp_paths} {shlex.quote(str(cfg.snippet_dir))}/ && "
            f"{target_check}"
        )
    else:
        target = shlex.quote(f"root@{target_node}")
        deliver = (
            f"ssh -o BatchMode=yes {target} {shlex.quote(target_mkdir)} && "
            f"scp -q -p {temp_paths} "
            f"{shlex.quote(f'root@{target_node}:{cfg.snippet_dir}/')} && "
            f"ssh -o BatchMode=yes {target} {shlex.quote(target_check)}"
        )

    command = (
        'hs_tmp=$(mktemp -d /tmp/homestack-snippets.XXXXXX) || exit 80; '
        'trap \'rm -rf "$hs_tmp"\' EXIT HUP INT TERM; '
        f"{fetch} && {deliver}"
    )
    session.run(command, timeout=120)
    return [str(path) for path in paths]
