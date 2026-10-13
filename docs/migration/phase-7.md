# Phase 7: `resize` (B5)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** B5 `resize`: guest inspection, growing the disk in Proxmox and
the layers in the guest (partition, LUKS, LVM, ext4, Btrfs), finishing an
interrupted growth by repeating it with the same size; event
`workspace.resized`
**Decisions used:** 5 (`exec` stays for `resize`), 6 (decided 2026-10-13:
the LUKS passphrase is sent to the server and reaches `cryptsetup` as
`input-data`), and the rules of the gate before phase 5 (only VMs of the
instance's pool)

## 1. Python behaviour (step 3)

### Plan (`resize.build_resize_plan`)

1. Exactly one of `root_size` and `home_size` ("Specify exactly one of
   --root-size or --home-size"); the role is `root` or `home`.
2. `proxmox.parse_home_size`: an integer number of GiB or TiB (`20G`,
   `1T`), upper-cased; the target in GiB.
3. `status.resolve_existing_workspace(require_network=False)`: not Gold,
   exists, has a node, the role tags, a valid name, root and home disks on
   different volumes, the home's serial `HS_HOME_<vmid>`.
4. The VM must be `running` ("Resize requires a running VM with a working
   QEMU Guest Agent").
5. No Proxmox `lock`, not a template, no `homestack-gold` tag ("Cannot
   resize a locked VM or Gold/template VM"). The `homestack-lock` tag does
   not block a resize (`test_lock_allows_resize_and_routes_to_current_node`).
6. The VM's data disks (`scsiN`, `virtioN`, `sataN`, `ideN` that are not a
   CD-ROM or the Cloud-Init drive) are exactly the configured root and home
   slots ("Resize requires exactly the configured root and home data
   disks").
7. The disk's current size from its `size=` (`guest.parse_disk_size_gb`,
   K/M/G/T); none → "Cannot determine current disk size for <slot>".
8. A target below the current size is refused before the guest is touched
   ("Shrinking is not supported: <slot> is <n>G, requested <size>"). An
   equal size is allowed: it finishes an interrupted growth.
9. Guest inspection (`_inspect_guest`, below) through `agent/exec`; a
   failure is "Cannot inspect <role> disk in VM <vmid>: <the guest's
   reason>", without the script.
10. The disk size the guest sees must equal Proxmox's ("Guest disk size
    does not match Proxmox; wait for the guest to detect the disk size").
11. The plan: VMID, name, node, role, slot, volume, current size, size,
    target size, the inspection, the config `digest`, and the VM's IP when
    the LUKS layer needs a passphrase (for the SSH connection).

### Guest inspection (`resize_guest.INSPECT`)

A Python script sent with `python3 -c` and run in the guest as root,
read-only. Arguments: the mount (`/` or `/home/<user>`), the role and the
home label.

1. The tools `findmnt`, `lsblk`, `blkid`, `blockdev` must exist ("Missing
   guest tool: X").
2. `findmnt --json --mountpoint M -o SOURCE,FSTYPE,TARGET,OPTIONS,MAJ:MIN`:
   exactly one filesystem, mounted exactly at M, `ext4` or `btrfs`,
   mounted `rw`. Then `resize2fs` or `btrfs` must exist.
3. `lsblk --json --bytes --paths --list -o
   NAME,TYPE,PKNAME,SERIAL,SIZE,MAJ:MIN`, flattened; a repeated MAJ:MIN is
   refused. Rows are found by MAJ:MIN or by the real path of their name and
   of `/dev/<basename>`.
4. ext4: the row with the filesystem's MAJ:MIN (`/dev/root` and other
   aliases resolve this way). Btrfs: `btrfs filesystem show --raw M` must
   have one device; its path and the mount's source (without `[/subvol]`)
   must name the same row.
5. The chain from that row to a disk or partition through `PKNAME` and
   `/sys/class/block/<dev>/slaves`: one parent at each step, no cycle,
   only `disk`, `part`, `crypt` and `lvm`, at most one `crypt` and one
   `lvm`, LVM above encryption only.
6. A partition's parent must be a disk. The guest must have exactly two
   disks besides `zram*`, exactly one with the home serial; the home role
   must sit on it and the root role must not; the home's filesystem label
   must be the home label.
7. A partition must be the last one on its disk (by `start` in sysfs), and
   `growpart` must exist.
8. LUKS: `cryptsetup` must exist; the backing device must be
   `crypto_LUKS`; the mapper name from sysfs `dm/name`; `/etc/crypttab`
   rows for that name whose source (`UUID=`, `PARTUUID=`, `LABEL=` or a
   path) is the backing device; at most one. Octal escapes (`\040`) are
   decoded, any other backslash is refused; options `keyscript`, `offset`,
   `skip`, `sector-size`, `keyfile-offset`, `keyfile-size`, `key-slot`,
   `header` are refused. A key file must be an absolute path to a regular
   file (`auth=keyfile`); `none`, `-` or no row means `auth=passphrase`.
9. LVM: `pvs`, `vgs`, `lvs`, `pvresize`, `lvextend` must exist; the mounted
   LV is an ordinary linear LV, alone in its VG, whose VG has one PV in the
   chain below the LV.
10. Output: `device`, `parent`, `partition`, `size_bytes` (the parent
    disk), `partition_device`, `crypt` (`name`, `device`, `key_file`,
    `auth`), `lvm` (`lv_path`, `pv_path`, `lv_size_bytes`,
    `extent_size_bytes`, `vg_name`), `filesystem`, `mount`, `btrfs_devid`.

### Execution (`resize.resize_workspace`, `_execute_resize`)

1. Plan again; node, slot, volume, current size and the inspection must be
   unchanged ("Workspace <key> changed after resize confirmation; generate
   a new plan"); the config must have a `digest`.
2. LUKS access before anything changes (`resize_auth.crypt_resize_access`):
   - key file: `cryptsetup --batch-mode --key-file K open --type luks
     --test-passphrase DEVICE` through QGA; failure → "LUKS authentication
     or resize failed; verify the configured key file in the VM";
   - passphrase: refused unless interactive on a TTY ("LUKS requires a
     passphrase. Run resize interactively without --json; no disk changes
     made"); the boot ID read through QGA must equal the one read over SSH
     as root ("SSH target is not the VM inspected through QEMU Guest Agent;
     no disk changes made"); the passphrase is read hidden, must not be
     empty, and goes to `cryptsetup --key-file -` on stdin over SSH, never
     in the arguments; a failed test → "LUKS passphrase verification
     failed; no disk changes made".
3. If the target is larger: `qm disk resize VMID SLOT SIZE --digest D`.
4. Read the config: the slot's volume and size must be the plan's volume
   and target ("Proxmox disk size or identity verification failed").
5. In the guest (`guest_out_on_node`, `set -eu`): wait up to 15 s for
   `blockdev --getsize64 PARENT` to reach the target ("Guest has not
   detected the enlarged disk"); `growpart PARENT N` (exit 1 with
   `NOCHANGE` is success); with LUKS, this part runs alone, then
   `cryptsetup resize NAME` (same authentication, key file through QGA,
   passphrase on stdin), then the rest; `pvresize PV`, `lvextend --extents
   +100%FREE LV` only if the VG has free extents; `resize2fs DEVICE` or
   `btrfs filesystem resize DEVID:max MOUNT`.
6. Any failure of 4–5 → "Disk growth may already be applied; filesystem
   growth is incomplete. Re-run resize VMID --ROLE-size SIZE to finish.
   <reason>". Repeating with the same size skips step 3 and redoes 5, which
   is idempotent.
7. Result: the plan plus `ok` and `status: "resized"`.

Nothing is journalled: the disk only grows, and every guest step can be
repeated.

### Tests in scope

`test_resize.py`, `test_resize_inspection.py`, `test_resize_layers.py`,
`test_resize_luks.py`, `test_resize_auth.py`. `test_resize_cli.py` is the
client (phase 13). The alien-tag case of `test_alien_workspace.py` is
assigned to phase 8 in the manifest and stays there.
