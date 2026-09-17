"""Online growth of workspace disks and ext4 filesystems."""
from __future__ import annotations

import json
import re
import shlex
from typing import Any

from .config import Config
from .guest import guest_out_on_node, parse_disk_size_gb
from .models import AppError, GOLD_TAG
from .proxmox import has_tag, node_run, parse_home_size, qm_config_on_node
from .status import resolve_existing_workspace
from .transports.base import Transport

# Run read-only inspection in the guest; use serial identity instead of device order.
_INSPECT = r'''
import json, os, shutil, subprocess, sys
mount, role, label = sys.argv[1:]
def run(*args):
    return subprocess.check_output(args, text=True).strip()
def require(ok, message):
    if not ok:
        sys.exit(message)
for tool in ('findmnt', 'lsblk', 'blkid', 'resize2fs', 'blockdev'):
    require(shutil.which(tool), 'Missing guest tool: ' + tool)
fs = json.loads(run('findmnt', '--json', '--mountpoint', mount, '-o', 'SOURCE,FSTYPE,TARGET,OPTIONS'))['filesystems'][0]
require(fs['target'] == mount and fs['fstype'] == 'ext4', 'Resize requires an exact ext4 mount: ' + mount)
require('rw' in fs['options'].split(','), 'Filesystem is read-only')
device = os.path.realpath(fs['source'])
rows = json.loads(run('lsblk', '--json', '--bytes', '--paths', '--list', '-o', 'NAME,TYPE,PKNAME,SERIAL,SIZE'))['blockdevices']
by_name = {r['name']: r for r in rows}
require(device in by_name, 'Mounted source is not a supported block device')
entry = by_name[device]
require(entry['type'] in ('disk', 'part'), 'LVM and encrypted layouts are not supported')
parent = entry['pkname'] if entry['type'] == 'part' else device
require(parent in by_name and by_name[parent]['type'] == 'disk', 'Unsupported partition parent')
disks = [r for r in rows if r['type'] == 'disk' and not os.path.basename(r['name']).startswith('zram')]
require(len(disks) == 2, 'Resize requires exactly the root and home data disks')
homes = [r for r in disks if (r.get('serial') or '').strip() == label]
require(len(homes) == 1, 'Cannot identify the persistent home disk by serial')
expected = homes[0]['name'] if role == 'home' else next(r['name'] for r in disks if r['name'] != homes[0]['name'])
require(parent == expected, 'Mounted filesystem does not match the selected workspace disk')
if role == 'home':
    require(run('blkid', '-s', 'LABEL', '-o', 'value', device) == label, 'Persistent home label mismatch')
partition = None
if entry['type'] == 'part':
    require(shutil.which('growpart'), 'Missing guest tool: growpart')
    def sysvalue(name, field):
        with open('/sys/class/block/' + os.path.basename(name) + '/' + field) as f:
            return int(f.read())
    partition = sysvalue(device, 'partition')
    start = sysvalue(device, 'start')
    require(not any(r['type'] == 'part' and r['pkname'] == parent and sysvalue(r['name'], 'start') > start for r in rows), 'Selected partition is not the last partition on disk')
print(json.dumps(dict(device=device, parent=parent, partition=partition, size_bytes=int(by_name[parent]['size']))))
'''


def _inspect_guest(session: Transport, cfg: Config, node: str, vmid: int, role: str, label: str) -> dict[str, Any]:
    mount = '/' if role == 'root' else f'/home/{cfg.user_name}'
    command = shlex.join(['python3', '-c', _INSPECT, mount, role, label])
    try:
        return json.loads(guest_out_on_node(session, cfg, node, vmid, command))
    except (ValueError, TypeError) as exc:
        raise AppError('Invalid guest disk inspection response') from exc


def build_resize_plan(
    session: Transport, cfg: Config, vmid: int, *,
    root_size: str | None = None, home_size: str | None = None,
) -> dict[str, Any]:
    if (root_size is None) == (home_size is None):
        raise AppError('Specify exactly one of --root-size or --home-size')
    role = 'root' if root_size is not None else 'home'
    size, target_gib = parse_home_size(root_size if root_size is not None else home_size)
    info = resolve_existing_workspace(session, cfg, vmid, require_network=False)
    vm_cfg = info['vm_config']
    if info['status'] != 'running':
        raise AppError('Resize requires a running VM with a working QEMU Guest Agent')
    if vm_cfg.get('lock') or vm_cfg.get('template') == '1' or has_tag(vm_cfg.get('tags'), GOLD_TAG):
        raise AppError('Cannot resize a locked VM or Gold/template VM')
    disk = cfg.root_disk if role == 'root' else cfg.home_disk
    data_disks = {
        key for key, value in vm_cfg.items()
        if re.fullmatch(r'(?:scsi|virtio|sata|ide)\d+', key)
        and 'media=cdrom' not in value and 'cloudinit' not in value
    }
    if data_disks != {cfg.root_disk, cfg.home_disk}:
        raise AppError('Resize requires exactly the configured root and home data disks')
    current = parse_disk_size_gb(vm_cfg[disk])
    if current is None:
        raise AppError(f'Cannot determine current disk size for {disk}')
    if target_gib < current:
        raise AppError(f'Shrinking is not supported: {disk} is {current:g}G, requested {size}')
    guest = _inspect_guest(session, cfg, info['node'], vmid, role, info['home_label'])
    if guest['size_bytes'] != int(current * 1024**3):
        raise AppError('Guest disk size does not match Proxmox; wait for the guest to detect the disk size')
    return dict(vmid=vmid, name=info['name'], node=info['node'], role=role, disk=disk,
                volume=vm_cfg[disk].split(',', 1)[0], current_size_gib=current,
                size=size, target_size_gib=target_gib, guest=guest,
                digest=vm_cfg.get('digest', ''))


def resize_workspace(session: Transport, cfg: Config, plan: dict[str, Any]) -> dict[str, Any]:
    vmid, node, disk = plan['vmid'], plan['node'], plan['disk']
    current = build_resize_plan(session, cfg, vmid, **{f'{plan["role"]}_size': plan['size']})
    for key in ('node', 'disk', 'volume', 'current_size_gib', 'guest'):
        if current[key] != plan[key]:
            raise AppError(f'Workspace {key} changed after resize confirmation; generate a new plan')
    if not current['digest']:
        raise AppError('Missing Proxmox configuration digest; cannot safely resize')
    if current['target_size_gib'] > current['current_size_gib']:
        node_run(session, cfg, node, shlex.join([
            'qm', 'disk', 'resize', str(vmid), disk, plan['size'], '--digest', current['digest'],
        ]), timeout=300)
    guest = current['guest']
    parent, device = shlex.quote(guest['parent']), shlex.quote(guest['device'])
    target_bytes = int(current['target_size_gib'] * 1024**3)
    script = f'''set -eu
n=0
while [ "$(blockdev --getsize64 {parent})" -lt {target_bytes} ]; do
    n=$((n + 1))
    [ "$n" -lt 15 ] || {{ echo 'Guest has not detected the enlarged disk' >&2; exit 1; }}
    sleep 1
done
'''
    if guest['partition'] is not None:
        script += f'''if output=$(growpart {parent} {int(guest['partition'])} 2>&1); then
    printf '%s\\n' "$output"
else
    code=$?
    case "$code:$output" in 1:NOCHANGE:*) ;; *) printf '%s\\n' "$output" >&2; exit "$code" ;; esac
fi
'''
    script += f'resize2fs {device}\n'
    try:
        final = qm_config_on_node(session, cfg, node, vmid)
        if final.get(disk, '').split(',', 1)[0] != plan['volume'] or parse_disk_size_gb(final.get(disk, '')) != current['target_size_gib']:
            raise AppError('Proxmox disk size or identity verification failed')
        guest_out_on_node(session, cfg, node, vmid, script)
    except AppError as exc:
        raise AppError(
            f'Disk growth may already be applied; filesystem growth is incomplete. '
            f'Re-run resize {vmid} --{plan["role"]}-size {plan["size"]} to finish. {exc}'
        ) from exc
    return {'ok': True, **plan, 'status': 'resized'}
