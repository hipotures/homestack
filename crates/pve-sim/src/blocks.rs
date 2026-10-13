//! The guest's block devices and the commands `resize` runs on them.
//!
//! A guest has disks (sized by the VM config, so `PUT /resize` shows at
//! once), partitions, LUKS mappings and LVM volumes ([`BlockSpec`]), and
//! filesystems on them. The tools print what util-linux 2.39, cryptsetup
//! 2.7, LVM 2.03 and e2fsprogs 1.47 of Ubuntu 24.04 print for the same
//! layout, in the parts HomeStack reads; growing works layer by layer:
//! `growpart` the partition up to the disk, `cryptsetup resize` the mapping
//! up to its device (less the 16 MiB LUKS2 header), `pvresize` and
//! `lvextend` the volume up to the mapping (in 4 MiB extents), and
//! `resize2fs` or `btrfs filesystem resize` the filesystem up to its
//! device. A LUKS key is checked against [`BlockSpec::luks_key`]: the
//! content of the key file, or the passphrase on standard input.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::guest::{ExecResult, Node};
use crate::scenario::{BlockSpec, FilesystemSpec, GuestSpec};

/// What the VM config says about a disk slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub size: u64,
    pub serial: Option<String>,
}

/// The VM's disks by slot.
pub type Disks = BTreeMap<String, DiskInfo>;

const LUKS_HEADER: u64 = 16 << 20;
const EXTENT: u64 = 4 << 20;

fn ok(out: impl Into<String>) -> ExecResult {
    ExecResult {
        exitcode: 0,
        out: out.into(),
        err: String::new(),
    }
}

fn failed(exitcode: i32, err: impl Into<String>) -> ExecResult {
    ExecResult {
        exitcode,
        out: String::new(),
        err: err.into(),
    }
}

/// The block devices: the scenario's, or one whole disk per filesystem on
/// `sdX` or `vdX`.
#[must_use]
pub fn devices(guest: &GuestSpec) -> Vec<BlockSpec> {
    if !guest.blocks.is_empty() {
        return guest.blocks.clone();
    }
    guest
        .filesystems
        .iter()
        .filter_map(|fs| {
            let bytes = fs.device.as_bytes();
            let whole = bytes.len() == 3 && matches!(&bytes[..2], b"sd" | b"vd") && bytes[2].is_ascii_lowercase();
            whole.then(|| {
                let index = u32::from(bytes[2] - b'a');
                BlockSpec::disk(&format!("/dev/{}", fs.device), &format!("8:{}", index * 16), &fs.slot)
            })
        })
        .collect()
}

/// Every path a device answers to.
fn paths(block: &BlockSpec) -> Vec<String> {
    let mut paths = vec![block.name.clone(), format!("/dev/{}", block.kernel_name())];
    paths.extend(block.aliases.iter().cloned());
    paths
}

fn find<'b>(blocks: &'b [BlockSpec], path: &str) -> Option<&'b BlockSpec> {
    blocks
        .iter()
        .find(|block| paths(block).iter().any(|known| known == path))
}

fn find_index(blocks: &[BlockSpec], path: &str) -> Option<usize> {
    blocks
        .iter()
        .position(|block| paths(block).iter().any(|known| known == path))
}

/// `realpath -m`: a device-mapper path resolves to `/dev/dm-N`.
fn realpath(blocks: &[BlockSpec], path: &str) -> String {
    match find(blocks, path) {
        Some(block) if block.dm.is_some() => format!("/dev/{}", block.kernel_name()),
        _ => path.to_owned(),
    }
}

fn size_of(block: &BlockSpec, disks: &Disks) -> u64 {
    match &block.slot {
        Some(slot) => disks.get(slot).map_or(0, |disk| disk.size),
        None => block.size,
    }
}

/// The filesystem on a device.
fn filesystem_on<'g>(guest: &'g GuestSpec, blocks: &[BlockSpec], path: &str) -> Option<&'g FilesystemSpec> {
    let block = find(blocks, path)?;
    guest
        .filesystems
        .iter()
        .find(|fs| paths(block).contains(&format!("/dev/{}", fs.device)))
}

/// `/sys/class/block/<dev>/{partition,start,dm/name}`
#[must_use]
pub fn sysfs(guest: &GuestSpec, path: &str) -> Option<Node> {
    let rest = path.strip_prefix("/sys/class/block/")?;
    let (kernel, attribute) = rest.split_once('/')?;
    let blocks = devices(guest);
    let block = blocks.iter().find(|block| block.kernel_name() == kernel)?;
    let value = match attribute {
        "partition" => block.partition.map(|n| n.to_string()),
        "start" => block.start.map(|n| n.to_string()),
        "dm/name" => block.dm_name.clone(),
        _ => None,
    }?;
    Some(Node::File(format!("{value}\n").into_bytes()))
}

/// The words of a command made of plain and single-quoted words, or `None`
/// for anything else the shell would interpret.
fn words(shell: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = shell.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' | '$' | '`' => return None,
                        other => current.push(other),
                    }
                }
            }
            ';' | '|' | '&' | '<' | '>' | '$' | '`' | '(' | ')' | '\\' | '*' | '?' | '\n' => return None,
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        words.push(current);
    }
    Some(words)
}

/// `findmnt --json --mountpoint M -o SOURCE,FSTYPE,TARGET,OPTIONS,MAJ:MIN`
fn findmnt(guest: &GuestSpec, blocks: &[BlockSpec], mount: &str) -> ExecResult {
    let Some((index, fs)) = guest
        .filesystems
        .iter()
        .enumerate()
        .find(|(_, fs)| fs.mountpoint == mount)
    else {
        return failed(1, "");
    };
    let device = format!("/dev/{}", fs.device);
    let (source, maj_min) = if fs.fstype == "btrfs" {
        let source = fs
            .subvolume
            .as_ref()
            .map_or_else(|| device.clone(), |subvolume| format!("{device}[{subvolume}]"));
        (source, format!("0:{}", 30 + index))
    } else {
        let maj_min = find(blocks, &device).map(|b| b.maj_min.clone()).unwrap_or_default();
        (device, maj_min)
    };
    let body = json!({"filesystems": [{
        "source": source,
        "fstype": fs.fstype,
        "target": fs.mountpoint,
        "options": "rw,relatime",
        "maj:min": maj_min,
    }]});
    ok(format!("{body}\n"))
}

/// `lsblk --json --bytes --paths --list -o NAME,TYPE,PKNAME,SERIAL,SIZE,MAJ:MIN`
fn lsblk(blocks: &[BlockSpec], disks: &Disks) -> ExecResult {
    let rows: Vec<Value> = blocks
        .iter()
        .map(|block| {
            let serial = block.slot.as_ref().map(|slot| {
                disks
                    .get(slot)
                    .and_then(|disk| disk.serial.clone())
                    .unwrap_or_else(|| format!("0QEMU_QEMU_HARDDISK_drive-{slot}"))
            });
            json!({
                "name": block.name,
                "type": block.kind,
                "pkname": block.pkname,
                "serial": serial,
                "size": size_of(block, disks),
                "maj:min": block.maj_min,
            })
        })
        .collect();
    ok(format!("{}\n", json!({ "blockdevices": rows })))
}

/// `blkid -s TAG -o value DEVICE`; exit 2 without the tag.
fn blkid(guest: &GuestSpec, blocks: &[BlockSpec], tag: &str, device: &str) -> ExecResult {
    let Some(block) = find(blocks, device) else {
        return failed(2, "");
    };
    let luks = blocks.iter().any(|other| {
        other.kind == "crypt"
            && other
                .pkname
                .as_deref()
                .is_some_and(|p| paths(block).iter().any(|x| x == p))
    });
    let fs = filesystem_on(guest, blocks, device);
    let value = match tag {
        "TYPE" if luks => Some("crypto_LUKS".to_owned()),
        "TYPE" => fs.map(|fs| fs.fstype.clone()),
        "LABEL" => fs.and_then(|fs| fs.label.clone()),
        _ => None,
    }
    .or_else(|| block.blkid.get(tag).cloned());
    value.map_or_else(|| failed(2, ""), |value| ok(format!("{value}\n")))
}

fn lvm_volumes(blocks: &[BlockSpec]) -> impl Iterator<Item = &BlockSpec> {
    blocks.iter().filter(|block| block.kind == "lvm")
}

fn lv_path(block: &BlockSpec) -> String {
    block.lv_path.clone().unwrap_or_else(|| block.name.clone())
}

fn free_extents(block: &BlockSpec) -> u64 {
    block.pv_size.unwrap_or(block.size).saturating_sub(block.size) / EXTENT
}

/// `lvs`, `vgs` and `pvs` with `--reportformat json --units b --nosuffix`.
fn lvm_report(blocks: &[BlockSpec], tool: &str) -> ExecResult {
    let (key, rows): (&str, Vec<Value>) = match tool {
        "lvs" => (
            "lv",
            lvm_volumes(blocks)
                .map(|block| {
                    let path = lv_path(block);
                    json!({
                        "lv_path": path,
                        "lv_name": path.rsplit('/').next().unwrap_or_default(),
                        "vg_name": block.vg,
                        "lv_size": block.size.to_string(),
                        "lv_attr": "-wi-ao----",
                        "segtype": "linear",
                    })
                })
                .collect(),
        ),
        "vgs" => (
            "vg",
            lvm_volumes(blocks)
                .map(|block| json!({"vg_name": block.vg, "vg_extent_size": EXTENT.to_string()}))
                .collect(),
        ),
        _ => (
            "pv",
            lvm_volumes(blocks)
                .map(|block| json!({"pv_name": block.pkname, "vg_name": block.vg}))
                .collect(),
        ),
    };
    ok(format!("  {}\n", json!({ "report": [{ key: rows }] })))
}

fn device_size(blocks: &[BlockSpec], disks: &Disks, path: &str) -> Option<u64> {
    find(blocks, path).map(|block| size_of(block, disks))
}

/// The `resize` commands; `None` for anything else.
pub fn exec(guest: &mut GuestSpec, disks: &Disks, shell: &str, input: Option<&str>) -> Option<ExecResult> {
    let blocks = devices(guest);
    if let Some(rest) = shell.strip_prefix("realpath -m -- ") {
        let list = words(rest)?;
        let lines: String = list.iter().map(|path| realpath(&blocks, path) + "\n").collect();
        return Some(ok(lines));
    }
    if let Some(path) = shell
        .strip_prefix("ls -1A -- ")
        .and_then(|rest| rest.strip_suffix(" 2>/dev/null || true"))
    {
        let kernel = path
            .strip_prefix("/sys/class/block/")
            .and_then(|rest| rest.strip_suffix("/slaves"));
        let slaves: String = kernel
            .and_then(|kernel| blocks.iter().find(|block| block.kernel_name() == kernel))
            .filter(|block| block.dm.is_some())
            .and_then(|block| block.pkname.as_deref())
            .and_then(|parent| find(&blocks, parent))
            .map(|parent| format!("{}\n", parent.kernel_name()))
            .unwrap_or_default();
        return Some(ok(slaves));
    }
    if let Some(rest) = shell.strip_prefix("if [ -e ") {
        let path = rest.split_once(" ]").map(|(path, _)| path)?;
        let expected = format!(
            "if [ -e {path} ]; then if [ -f {path} ]; then echo regular; else echo other; fi; else echo missing; fi"
        );
        if shell != expected {
            return None;
        }
        let kind = match crate::guest::file(guest, path) {
            Node::File(_) => "regular",
            Node::Directory => "other",
            Node::Missing => "missing",
        };
        return Some(ok(format!("{kind}\n")));
    }
    let argv = words(shell)?;
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    match argv.as_slice() {
        [
            "findmnt",
            "--json",
            "--mountpoint",
            mount,
            "-o",
            "SOURCE,FSTYPE,TARGET,OPTIONS,MAJ:MIN",
        ] => Some(findmnt(guest, &blocks, mount)),
        [
            "lsblk",
            "--json",
            "--bytes",
            "--paths",
            "--list",
            "-o",
            "NAME,TYPE,PKNAME,SERIAL,SIZE,MAJ:MIN",
        ] => Some(lsblk(&blocks, disks)),
        ["blkid", "-s", tag, "-o", "value", device] => Some(blkid(guest, &blocks, tag, device)),
        ["btrfs", "filesystem", "show", "--raw", mount] => {
            let fs = guest
                .filesystems
                .iter()
                .find(|fs| fs.mountpoint == *mount && fs.fstype == "btrfs")?;
            let device = format!("/dev/{}", fs.device);
            let size = device_size(&blocks, disks, &device).unwrap_or(fs.size);
            Some(ok(format!(
                "Label: none  uuid: 00000000-0000-4000-8000-000000000000\n\tTotal devices 1 FS bytes used {}\n\tdevid    1 size {size} used {} path {device}\n\n",
                fs.used, fs.used
            )))
        }
        [
            tool @ ("lvs" | "vgs" | "pvs"),
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "--noheadings",
            "-o",
            _,
        ] => Some(lvm_report(&blocks, tool)),
        ["vgs", "--noheadings", "-o", "vg_free_count", vg] => {
            let block = lvm_volumes(&blocks).find(|block| block.vg.as_deref() == Some(*vg))?;
            Some(ok(format!("  {}\n", free_extents(block))))
        }
        ["blockdev", "--getsize64", device] => Some(device_size(&blocks, disks, device).map_or_else(
            || {
                failed(
                    1,
                    format!("blockdev: cannot open {device}: No such file or directory\n"),
                )
            },
            |size| ok(format!("{size}\n")),
        )),
        ["cryptsetup", "--batch-mode", "--key-file", key, rest @ ..] => {
            Some(cryptsetup(guest, &blocks, disks, key, rest, input))
        }
        ["growpart", disk, number] => Some(growpart(guest, &blocks, disks, disk, number)),
        ["pvresize", pv] => {
            let size = device_size(&blocks, disks, pv)?;
            let index = guest
                .blocks
                .iter()
                .position(|block| block.kind == "lvm" && block.pkname.as_deref() == Some(*pv))?;
            guest.blocks[index].pv_size = Some(size.saturating_sub(1 << 20));
            Some(ok(format!(
                "  Physical volume \"{pv}\" changed\n  1 physical volume(s) resized or updated / 0 physical volume(s) not resized\n"
            )))
        }
        ["lvextend", "--extents", "+100%FREE", lv] => {
            let index = find_index(&guest.blocks, lv).filter(|i| guest.blocks[*i].kind == "lvm")?;
            let block = &mut guest.blocks[index];
            let free = free_extents(block);
            if free == 0 {
                return Some(failed(
                    5,
                    format!(
                        "  New size given ({} extents) not larger than existing size.\n",
                        block.size / EXTENT
                    ),
                ));
            }
            block.size += free * EXTENT;
            Some(ok(format!(
                "  Logical volume {} successfully resized.\n",
                lv_path(block)
            )))
        }
        ["resize2fs", device] => {
            let size = device_size(&blocks, disks, device)?;
            let fs_index = guest.filesystems.iter().position(|fs| {
                fs.fstype == "ext4"
                    && find(&blocks, device).is_some_and(|b| paths(b).contains(&format!("/dev/{}", fs.device)))
            })?;
            Some(grow_filesystem(&mut guest.filesystems[fs_index], size, device))
        }
        ["btrfs", "filesystem", "resize", spec, mount] => {
            let index = guest
                .filesystems
                .iter()
                .position(|fs| fs.mountpoint == *mount && fs.fstype == "btrfs")?;
            if *spec != "1:max" {
                return Some(failed(
                    1,
                    format!("ERROR: unable to resize '{mount}': No such device\n"),
                ));
            }
            let device = format!("/dev/{}", guest.filesystems[index].device);
            let size = device_size(&blocks, disks, &device)?;
            let fs = &mut guest.filesystems[index];
            let old = fs.size;
            fs.avail += size.saturating_sub(old);
            fs.size = size.max(old);
            Some(ok(format!("Resize device id 1 ({device}) from {old} to max\n")))
        }
        _ => None,
    }
}

fn grow_filesystem(fs: &mut FilesystemSpec, size: u64, device: &str) -> ExecResult {
    let blocks = size / 4096;
    if size <= fs.size {
        return ok(format!(
            "resize2fs 1.47.0 (5-Feb-2023)\nThe filesystem is already {blocks} (4k) blocks long.  Nothing to do!\n\n"
        ));
    }
    fs.avail += size - fs.size;
    fs.size = size;
    ok(format!(
        "resize2fs 1.47.0 (5-Feb-2023)\nResizing the filesystem on {device} to {blocks} (4k) blocks.\nThe filesystem on {device} is now {blocks} (4k) blocks long.\n\n"
    ))
}

/// `cryptsetup --batch-mode --key-file KEY open --type luks
/// --test-passphrase DEVICE` and `... resize NAME`.
fn cryptsetup(
    guest: &mut GuestSpec,
    blocks: &[BlockSpec],
    disks: &Disks,
    key: &str,
    rest: &[&str],
    input: Option<&str>,
) -> ExecResult {
    let index = match rest {
        ["open", "--type", "luks", "--test-passphrase", device] => guest.blocks.iter().position(|b| {
            b.kind == "crypt"
                && b.pkname
                    .as_deref()
                    .is_some_and(|p| find(blocks, p).is_some_and(|parent| paths(parent).iter().any(|x| x == device)))
        }),
        ["resize", name] => guest.blocks.iter().position(|b| {
            b.kind == "crypt" && (b.dm_name.as_deref() == Some(*name) || b.name.rsplit('/').next() == Some(*name))
        }),
        _ => return failed(1, "pve-sim: cryptsetup action not modelled\n"),
    };
    let Some(index) = index else {
        return failed(4, "Device is not a valid LUKS device.\n");
    };
    let given = if key == "-" {
        input.map(str::to_owned)
    } else {
        match crate::guest::file(guest, key) {
            Node::File(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            _ => return failed(1, format!("Failed to open key file {key}.\n")),
        }
    };
    if given.is_none() || given != guest.blocks[index].luks_key {
        return failed(2, "No key available with this passphrase.\n");
    }
    if let ["resize", _] = rest {
        let parent = guest.blocks[index]
            .pkname
            .as_deref()
            .and_then(|p| device_size(blocks, disks, p))
            .unwrap_or_default();
        guest.blocks[index].size = parent.saturating_sub(LUKS_HEADER);
    }
    ok("")
}

/// `growpart DISK N`: the partition up to the end of the disk.
fn growpart(guest: &mut GuestSpec, blocks: &[BlockSpec], disks: &Disks, disk: &str, number: &str) -> ExecResult {
    let Some(disk_size) = device_size(blocks, disks, disk) else {
        return failed(2, format!("FAILED: {disk}: does not exist\n"));
    };
    let index = guest.blocks.iter().position(|b| {
        b.kind == "part"
            && b.partition.map(|n| n.to_string()).as_deref() == Some(number)
            && b.pkname
                .as_deref()
                .is_some_and(|p| find(blocks, p).is_some_and(|parent| paths(parent).iter().any(|x| x == disk)))
    });
    let Some(index) = index else {
        return failed(2, format!("FAILED: partition {number} does not exist\n"));
    };
    let part = &mut guest.blocks[index];
    let start = part.start.unwrap_or(2048) * 512;
    let wanted = disk_size.saturating_sub(start).saturating_sub(33 * 512);
    if part.size >= wanted {
        return ExecResult {
            exitcode: 1,
            out: format!(
                "NOCHANGE: partition {number} is size {}. it cannot be grown\n",
                part.size / 512
            ),
            err: String::new(),
        };
    }
    let old = part.size;
    part.size = wanted;
    ok(format!(
        "CHANGED: partition={number} start={} old: size={} end={} new: size={} end={}\n",
        start / 512,
        old / 512,
        (start + old) / 512,
        wanted / 512,
        (start + wanted) / 512
    ))
}
