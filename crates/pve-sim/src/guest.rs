//! The simulated guest behind the QEMU guest agent.
//!
//! The audit-level agent commands (`get-host-name`, `network-get-interfaces`,
//! `get-fsinfo`) are answered from the scenario's guest description, with the
//! shapes a PVE 9.2 / qemu-ga 10.2 guest returned. `exec` runs no shell: it
//! recognises the commands HomeStack sends and prints what the real tools
//! would print for the same guest, so both implementations read one state.
//! Anything else exits 127, so an unmodelled command is never silent.

use serde_json::{Value, json};

use crate::scenario::{FilesystemSpec, GuestSpec};

/// Outcome of one `exec`, as `exec-status` reports it.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exitcode: i32,
    pub out: String,
    pub err: String,
}

impl ExecResult {
    fn ok(out: impl Into<String>) -> Self {
        Self {
            exitcode: 0,
            out: out.into(),
            err: String::new(),
        }
    }

    fn line(text: &str) -> Self {
        Self::ok(format!("{text}\n"))
    }

    #[must_use]
    pub fn status_json(&self) -> Value {
        let mut body = json!({ "exited": 1, "exitcode": self.exitcode });
        if !self.out.is_empty() {
            body["out-data"] = Value::String(self.out.clone());
        }
        if !self.err.is_empty() {
            body["err-data"] = Value::String(self.err.clone());
        }
        body
    }
}

/// `get-host-name`
#[must_use]
pub fn host_name(guest: &GuestSpec) -> Value {
    json!({ "host-name": guest.hostname })
}

/// `network-get-interfaces`: `lo` first, then the scenario's interfaces.
#[must_use]
pub fn interfaces(guest: &GuestSpec) -> Value {
    let mut list = vec![json!({
        "name": "lo",
        "hardware-address": "00:00:00:00:00:00",
        "ip-addresses": [
            { "ip-address-type": "ipv4", "ip-address": "127.0.0.1", "prefix": 8 },
            { "ip-address-type": "ipv6", "ip-address": "::1", "prefix": 128 },
        ],
    })];
    for interface in &guest.interfaces {
        let addresses: Vec<Value> = interface
            .ipv4
            .iter()
            .filter_map(|cidr| cidr.split_once('/'))
            .map(|(address, prefix)| {
                json!({
                    "ip-address-type": "ipv4",
                    "ip-address": address,
                    "prefix": prefix.parse::<u8>().unwrap_or(32),
                })
            })
            .collect();
        list.push(json!({
            "name": interface.name,
            "hardware-address": interface.mac,
            "ip-addresses": addresses,
        }));
    }
    Value::Array(list)
}

/// `get-fsinfo`. qemu-ga reports `total-bytes` as used plus what
/// unprivileged users may still use, and `total-bytes-privileged` as the
/// whole filesystem; the disk serial is QEMU's default for the drive, not
/// the `serial=` of the VM config (observed on PVE 9.2).
#[must_use]
pub fn fsinfo(guest: &GuestSpec) -> Value {
    let list: Vec<Value> = guest
        .filesystems
        .iter()
        .map(|fs| {
            let unit = fs
                .slot
                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .parse::<u32>()
                .unwrap_or(0);
            json!({
                "name": fs.device,
                "mountpoint": fs.mountpoint,
                "type": fs.fstype,
                "used-bytes": fs.used,
                "total-bytes": fs.used.saturating_add(fs.avail),
                "total-bytes-privileged": fs.size,
                "disk": [{
                    "serial": format!("0QEMU_QEMU_HARDDISK_drive-{}", fs.slot),
                    "dev": format!("/dev/{}", fs.device),
                    "bus-type": "scsi",
                    "bus": 0,
                    "target": 0,
                    "unit": unit,
                    "pci-controller": { "domain": 0, "bus": 9, "slot": unit + 1, "function": 0 },
                }],
            })
        })
        .collect();
    Value::Array(list)
}

/// Run one of the shell commands HomeStack sends through `exec`.
#[must_use]
pub fn exec(guest: &GuestSpec, shell: &str) -> ExecResult {
    let shell = shell.trim();
    if shell == "hostname" {
        return ExecResult::line(&guest.hostname);
    }
    if let Some(rest) = shell.strip_prefix("ip -4 -o addr show dev ")
        && let Some((iface, tail)) = rest.split_once(' ')
        && tail == "2>/dev/null | awk '{print $4}' | head -n1"
    {
        let address = guest
            .interfaces
            .iter()
            .find(|interface| interface.name == iface)
            .and_then(|interface| interface.ipv4.first());
        return address.map_or_else(|| ExecResult::ok(""), |address| ExecResult::line(address));
    }
    if let Some(path) = between(shell, "findmnt -n -o SOURCE,FSTYPE,TARGET ", " 2>/dev/null || true") {
        return mounted(guest, path).map_or_else(
            || ExecResult::ok(""),
            |fs| ExecResult::line(&format!("/dev/{} {} {}", fs.device, fs.fstype, fs.mountpoint)),
        );
    }
    if let Some(path) = between(
        shell,
        "findmnt -n -o SOURCE ",
        " 2>/dev/null | xargs -r blkid -s LABEL -o value",
    ) {
        return mounted(guest, path)
            .and_then(|fs| fs.label.as_deref())
            .map_or_else(|| ExecResult::ok(""), ExecResult::line);
    }
    if let Some(path) = between(
        shell,
        "df -B1 --output=size,used,avail,pcent ",
        " 2>/dev/null | tail -n1",
    ) {
        return containing(guest, path).map_or_else(
            || ExecResult::ok(""),
            |fs| {
                ExecResult::line(&format!(
                    "{} {} {} {:>3}%",
                    fs.size,
                    fs.used,
                    fs.avail,
                    df_percent(fs.used, fs.avail)
                ))
            },
        );
    }
    if let Some(user) = between(
        shell,
        "if getent passwd ",
        " >/dev/null 2>&1; then echo true; else echo false; fi",
    ) {
        let present = guest.users.iter().any(|name| name == user);
        return ExecResult::line(if present { "true" } else { "false" });
    }
    ExecResult {
        exitcode: 127,
        out: String::new(),
        err: format!("pve-sim: guest command not modelled: {shell}\n"),
    }
}

fn between<'a>(text: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    text.strip_prefix(prefix)?.strip_suffix(suffix)
}

/// The filesystem mounted exactly at `path` (`findmnt PATH`).
fn mounted<'a>(guest: &'a GuestSpec, path: &str) -> Option<&'a FilesystemSpec> {
    guest.filesystems.iter().find(|fs| fs.mountpoint == path)
}

/// The filesystem holding `path` (`df PATH`): the longest mountpoint prefix.
fn containing<'a>(guest: &'a GuestSpec, path: &str) -> Option<&'a FilesystemSpec> {
    guest
        .filesystems
        .iter()
        .filter(|fs| {
            fs.mountpoint == "/"
                || path == fs.mountpoint
                || path
                    .strip_prefix(fs.mountpoint.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|fs| fs.mountpoint.len())
}

/// `df`'s use percentage: used / (used + avail), rounded up.
#[must_use]
pub fn df_percent(used: u64, avail: u64) -> u64 {
    let total = u128::from(used) + u128::from(avail);
    if total == 0 {
        return 0;
    }
    let percent = (u128::from(used) * 100).div_ceil(total);
    u64::try_from(percent).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::InterfaceSpec;

    fn guest() -> GuestSpec {
        GuestSpec {
            hostname: "test1".to_owned(),
            interfaces: vec![InterfaceSpec {
                name: "eth0".to_owned(),
                mac: "bc:24:11:56:a9:e8".to_owned(),
                ipv4: vec!["192.0.2.200/24".to_owned()],
            }],
            filesystems: vec![
                FilesystemSpec {
                    mountpoint: "/".to_owned(),
                    fstype: "ext4".to_owned(),
                    device: "dm-1".to_owned(),
                    slot: "scsi0".to_owned(),
                    label: None,
                    size: 10_464_022_528,
                    used: 3_457_495_040,
                    avail: 6_452_879_360,
                },
                FilesystemSpec {
                    mountpoint: "/home/user".to_owned(),
                    fstype: "ext4".to_owned(),
                    device: "sdb".to_owned(),
                    slot: "scsi1".to_owned(),
                    label: Some("HS_HOME_200".to_owned()),
                    size: 20_957_446_144,
                    used: 338_767_872,
                    avail: 20_601_901_056,
                },
            ],
            users: vec!["user".to_owned()],
        }
    }

    #[test]
    fn exec_prints_what_the_tools_print() {
        let guest = guest();
        let out = |shell: &str| exec(&guest, shell).out;
        assert_eq!(out("hostname"), "test1\n");
        assert_eq!(
            out("ip -4 -o addr show dev eth0 2>/dev/null | awk '{print $4}' | head -n1"),
            "192.0.2.200/24\n"
        );
        assert_eq!(
            out("findmnt -n -o SOURCE,FSTYPE,TARGET /home/user 2>/dev/null || true"),
            "/dev/sdb ext4 /home/user\n"
        );
        assert_eq!(
            out("findmnt -n -o SOURCE /home/user 2>/dev/null | xargs -r blkid -s LABEL -o value"),
            "HS_HOME_200\n"
        );
        // Observed on VM 200 with the same numbers (df rounds the percentage up).
        assert_eq!(
            out("df -B1 --output=size,used,avail,pcent /home/user 2>/dev/null | tail -n1"),
            "20957446144 338767872 20601901056   2%\n"
        );
        assert_eq!(
            out("if getent passwd ubuntu >/dev/null 2>&1; then echo true; else echo false; fi"),
            "false\n"
        );
        assert_eq!(
            out("findmnt -n -o SOURCE,FSTYPE,TARGET /home/other 2>/dev/null || true"),
            ""
        );
        assert_eq!(exec(&guest, "rm -rf /").exitcode, 127);
    }

    #[test]
    fn fsinfo_free_space_matches_df() {
        let info = fsinfo(&guest());
        let home = &info[1];
        assert_eq!(home["total-bytes-privileged"], 20_957_446_144_u64);
        let free = home["total-bytes"].as_u64().unwrap_or(0) - home["used-bytes"].as_u64().unwrap_or(0);
        assert_eq!(free, 20_601_901_056);
        assert_eq!(home["disk"][0]["serial"], "0QEMU_QEMU_HARDDISK_drive-scsi1");
        assert_eq!(df_percent(11_097_509_888, 9_910_312_960), 53);
        assert_eq!(df_percent(0, 0), 0);
    }
}
