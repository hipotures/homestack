//! The simulated guest behind the QEMU guest agent.
//!
//! The audit-level agent commands (`get-host-name`, `network-get-interfaces`,
//! `get-fsinfo`) are answered from the scenario's guest description, with the
//! shapes a PVE 9.2 / qemu-ga 10.2 guest returned. `exec` runs no shell: it
//! recognises the commands HomeStack sends and prints what the real tools
//! would print for the same guest, so both implementations read one state.
//! Anything else exits 127, so an unmodelled command is never silent.
//!
//! `file-read` reads a small file tree derived from the same description:
//! `/etc/passwd` from the accounts, the programs on the `PATH`, cloud-init's
//! files under `/run/cloud-init`, each filesystem's block device (an ext4
//! superblock with its label) and the scenario's own files.

use serde_json::{Value, json};

use crate::scenario::{FilesystemSpec, GuestSpec};

/// What a path is in the simulated guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File(Vec<u8>),
    Directory,
    Missing,
}

/// Directories programs are found in (merged `/usr`).
const BIN_DIRS: [&str; 4] = ["/usr/bin", "/usr/sbin", "/bin", "/sbin"];

/// The first bytes of a block device holding `fs`: an ext4 superblock at
/// offset 1024 with magic `0xEF53` (offset 56) and the label (offset 120,
/// 16 bytes, NUL-padded), as `blkid` reads it.
fn block_device(fs: &FilesystemSpec) -> Vec<u8> {
    let mut bytes = vec![0_u8; 4096];
    if fs.fstype == "ext4" {
        bytes[1024 + 56] = 0x53;
        bytes[1024 + 57] = 0xEF;
        if let Some(label) = &fs.label {
            for (index, byte) in label.bytes().take(16).enumerate() {
                bytes[1024 + 120 + index] = byte;
            }
        }
    }
    bytes
}

/// `/etc/passwd`
#[must_use]
pub fn passwd(guest: &GuestSpec) -> String {
    guest
        .accounts()
        .iter()
        .map(|account| account.passwd_line() + "\n")
        .collect()
}

/// Look a path up in the guest.
#[must_use]
pub fn file(guest: &GuestSpec, path: &str) -> Node {
    if let Some(text) = guest.files.get(path) {
        return Node::File(text.clone().into_bytes());
    }
    let generated = match path {
        "/etc/passwd" => Some(passwd(guest).into_bytes()),
        "/run/cloud-init/result.json" => guest.cloud_id.as_ref().map(|_| {
            b"{\n \"v1\": {\n  \"datasource\": \"DataSourceNoCloud [seed=/dev/sr0]\",\n  \"errors\": []\n }\n}\n"
                .to_vec()
        }),
        "/run/cloud-init/cloud-id" => guest.cloud_id.as_ref().map(|id| format!("{id}\n").into_bytes()),
        _ => None,
    };
    if let Some(bytes) = generated {
        return Node::File(bytes);
    }
    if let Some(device) = path.strip_prefix("/dev/")
        && let Some(fs) = guest.filesystems.iter().find(|fs| fs.device == device)
    {
        return Node::File(block_device(fs));
    }
    if let Some(node) = crate::blocks::sysfs(guest, path) {
        return node;
    }
    if let Some((dir, name)) = path.rsplit_once('/')
        && BIN_DIRS.contains(&dir)
        && guest.has_command(name)
    {
        return Node::File(b"\x7fELF\x02\x01\x01".to_vec());
    }
    let is_parent = |known: &str| known.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'));
    let homes = guest.accounts().into_iter().map(|account| account.home);
    let directories = [
        "/",
        "/etc",
        "/run",
        "/run/cloud-init",
        "/dev",
        "/usr",
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
    ];
    if directories.contains(&path)
        || homes.into_iter().any(|home| home == path || is_parent(&home))
        || guest.files.keys().any(|known| is_parent(known))
    {
        return Node::Directory;
    }
    Node::Missing
}

/// The content of a file, or `None` when it is missing, a directory or
/// empty (`test -s`).
fn non_empty(guest: &GuestSpec, path: &str) -> Option<Vec<u8>> {
    match file(guest, path) {
        Node::File(bytes) if !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

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

/// Run one of the shell commands HomeStack sends through `exec`, with
/// `input` on its standard input; `disks` are the VM config's disks.
#[must_use]
pub fn exec(guest: &mut GuestSpec, disks: &crate::blocks::Disks, shell: &str, input: Option<&str>) -> ExecResult {
    let shell = shell.trim();
    if let Some(result) = crate::blocks::exec(guest, disks, shell, input) {
        return result;
    }
    if let Some(result) = ensure_keys(guest, shell) {
        return result;
    }
    let guest: &GuestSpec = guest;
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
        let present = guest.accounts().iter().any(|account| account.name == user);
        return ExecResult::line(if present { "true" } else { "false" });
    }
    if let Some(result) = exec_checks(guest, shell) {
        return result;
    }
    ExecResult {
        exitcode: 127,
        out: String::new(),
        err: format!("pve-sim: guest command not modelled: {shell}\n"),
    }
}

/// Append to `<dir>/.ssh/authorized_keys` the `keys` whose blob (second
/// field) it lacks, after a newline when the file does not end with one;
/// nothing when `dir` is not a directory (the key step of
/// `hs_core::addkey::ensure_keys_script` and of the vendor snippets).
pub fn append_missing_keys<'k>(guest: &mut GuestSpec, dir: &str, keys: impl Iterator<Item = &'k str>) {
    if file(guest, dir) != Node::Directory {
        return;
    }
    let path = format!("{dir}/.ssh/authorized_keys");
    let mut text = guest.files.get(&path).cloned().unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for key in keys.map(str::trim).filter(|key| !key.is_empty()) {
        let blob = key.split_whitespace().nth(1).unwrap_or_default();
        if !text.contains(blob) {
            text.push_str(key);
            text.push('\n');
        }
    }
    guest.files.insert(path, text);
}

/// `hs_core::addkey::ensure_keys_script`: the keys between the `HS_KEYS`
/// heredoc markers appended in each directory of its `for dir in` line.
fn ensure_keys(guest: &mut GuestSpec, shell: &str) -> Option<ExecResult> {
    let body = shell.strip_prefix("# HomeStack ensure-keys\nset -eu\nkeys=$(cat <<'HS_KEYS'\n")?;
    let (keys, rest) = body.split_once("\nHS_KEYS\n)\nfor dir in ")?;
    let (dirs, _) = rest.split_once("; do\n")?;
    let keys = keys.to_owned();
    let dirs: Vec<String> = dirs.split_whitespace().map(str::to_owned).collect();
    for dir in &dirs {
        append_missing_keys(guest, dir, keys.lines());
    }
    Some(ExecResult::line("HomeStack: ensured SSH keys from Cloud-Init"))
}

fn failed(exitcode: i32, err: String) -> ExecResult {
    ExecResult {
        exitcode,
        out: String::new(),
        err,
    }
}

/// The commands of the refresh verification and the Gold contract
/// (`lifecycle._verify_refreshed_guest`, `gold.check_gold_readiness`).
fn exec_checks(guest: &GuestSpec, shell: &str) -> Option<ExecResult> {
    const MISSING_LOOP: (&str, &str) = (
        "for c in ",
        r#"; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#,
    );
    if shell == "command -v cloud-init >/dev/null 2>&1 && timeout 180 cloud-init status --wait >/dev/null 2>&1 || true"
    {
        return Some(ExecResult::ok(""));
    }
    if shell == "command -v cloud-id >/dev/null 2>&1 && cloud-id || true" {
        let id = guest.cloud_id.as_deref().filter(|_| guest.has_command("cloud-id"));
        return Some(id.map_or_else(|| ExecResult::ok(""), ExecResult::line));
    }
    if let Some(iface) = shell.strip_prefix("ip -4 -o addr show dev ")
        && !iface.contains(' ')
    {
        let Some(interface) = guest.interfaces.iter().find(|i| i.name == iface) else {
            return Some(failed(1, format!("Device \"{iface}\" does not exist.\n")));
        };
        let lines: String = interface
            .ipv4
            .iter()
            .map(|cidr| {
                format!(
                    "2: {iface}    inet {cidr} scope global {iface}\\       valid_lft forever preferred_lft forever\n"
                )
            })
            .collect();
        return Some(ExecResult::ok(lines));
    }
    if let Some(path) = shell.strip_prefix("findmnt -n -o SOURCE,FSTYPE,TARGET ")
        && !path.contains(' ')
    {
        return Some(mounted(guest, path).map_or_else(
            || failed(1, String::new()),
            |fs| ExecResult::line(&format!("/dev/{} {} {}", fs.device, fs.fstype, fs.mountpoint)),
        ));
    }
    if let Some(path) = between(shell, "findmnt -n -o SOURCE ", " | xargs -r blkid -s LABEL -o value") {
        return Some(
            mounted(guest, path)
                .and_then(|fs| fs.label.as_deref())
                .map_or_else(|| ExecResult::ok(""), ExecResult::line),
        );
    }
    if let Some(user) = shell.strip_prefix("id ")
        && !user.contains(' ')
    {
        let accounts = guest.accounts();
        return Some(match accounts.iter().find(|a| a.name == user) {
            Some(a) => ExecResult::line(&format!(
                "uid={}({}) gid={}({}) groups={}({})",
                a.uid, a.name, a.gid, a.name, a.gid, a.name
            )),
            None => failed(1, format!("id: '{user}': no such user\n")),
        });
    }
    if let Some(path) = between(shell, "test -s ", " && echo OK || echo MISSING") {
        return Some(ExecResult::line(if non_empty(guest, path).is_some() {
            "OK"
        } else {
            "MISSING"
        }));
    }
    if shell == r#"awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd"# {
        let lines: String = guest
            .accounts()
            .iter()
            .filter(|a| (1000..65534).contains(&a.uid))
            .map(|a| format!("{}:{}:{}\n", a.name, a.uid, a.gid))
            .collect();
        return Some(ExecResult::ok(lines));
    }
    if let Some(name) = between(
        shell,
        "if command -v ",
        " >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
    ) {
        return Some(ExecResult::line(if guest.has_command(name) {
            "PRESENT"
        } else {
            "ABSENT"
        }));
    }
    if shell == "python3 -c 'import yaml' >/dev/null 2>&1 && echo OK || echo MISSING" {
        let importable = guest.has_command("python3") && guest.has_command("python3-yaml");
        return Some(ExecResult::line(if importable { "OK" } else { "MISSING" }));
    }
    if let Some(words) = between(shell, MISSING_LOOP.0, MISSING_LOOP.1) {
        let missing: String = words
            .split(' ')
            .filter(|name| !guest.has_command(name))
            .map(|name| format!("{name}\n"))
            .collect();
        return Some(ExecResult::ok(missing));
    }
    if let Some(user) = between(shell, "getent passwd ", " || true") {
        let line = guest
            .accounts()
            .into_iter()
            .find(|a| a.name == user)
            .map(|a| a.passwd_line());
        return Some(line.map_or_else(|| ExecResult::ok(""), |line| ExecResult::line(&line)));
    }
    // `workspace_ssh.get_workspace_authorized_keys` (phase 4).
    if let Some((path, again)) = shell
        .strip_prefix("test -s ")
        .and_then(|rest| rest.split_once(" && cat "))
        && path == again
        && !path.contains(' ')
    {
        return Some(non_empty(guest, path).map_or_else(
            || failed(1, String::new()),
            |bytes| ExecResult::ok(String::from_utf8_lossy(&bytes).into_owned()),
        ));
    }
    if let Some(path) = between(shell, "cat ", " 2>/dev/null || true") {
        return Some(match file(guest, path) {
            Node::File(bytes) => ExecResult::ok(String::from_utf8_lossy(&bytes).into_owned()),
            Node::Directory | Node::Missing => ExecResult::ok(""),
        });
    }
    None
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
