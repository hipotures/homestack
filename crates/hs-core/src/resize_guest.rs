//! The read-only guest inspection of `resize` (mapping B5; phase 7).
//!
//! Ports `resize_guest.INSPECT`. Python sends that script to the guest with
//! `python3 -c` and gets JSON back; here the logic runs in the server and
//! asks the guest only for what the script read: tool lookups, the output of
//! `findmnt`, `lsblk`, `blkid`, `btrfs`, `lvs`, `vgs` and `pvs`, sysfs and
//! `/etc/crypttab`, real paths, directory listings and whether a key file is
//! a regular file ([`GuestSystem`]). The checks, their order and their
//! messages are the script's; a failed command is reported with its exit
//! status and output instead of Python's `CalledProcessError` text.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CoreError;
use crate::guest::{FileError, GUEST_EXEC_TIMEOUT, guest_exec, read_text_or_reason, shell_join, shell_quote};
use crate::pve::Pve;

/// Which workspace disk is resized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Root,
    Home,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Home => "home",
        }
    }
}

/// How a LUKS layer is unlocked for `cryptsetup resize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CryptAuth {
    /// A key file in the guest, named by `/etc/crypttab`.
    Keyfile,
    /// A passphrase, sent with the request that applies the plan.
    Passphrase,
}

/// The LUKS layer of the mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CryptLayer {
    /// The mapper name.
    pub name: String,
    /// The LUKS device under it.
    pub device: String,
    pub key_file: Option<String>,
    pub auth: CryptAuth,
}

/// The LVM layer of the mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LvmLayer {
    pub lv_path: String,
    pub pv_path: String,
    pub lv_size_bytes: u64,
    pub extent_size_bytes: u64,
    pub vg_name: String,
}

/// What the inspection found: the script's JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestLayout {
    /// The block device the filesystem is on.
    pub device: String,
    /// The disk under it.
    pub parent: String,
    pub partition: Option<u32>,
    /// Size of the parent disk.
    pub size_bytes: u64,
    pub partition_device: Option<String>,
    pub crypt: Option<CryptLayer>,
    pub lvm: Option<LvmLayer>,
    /// `ext4` or `btrfs`.
    pub filesystem: String,
    pub mount: String,
    pub btrfs_devid: Option<u64>,
}

/// What a path is in the guest (`os.stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Missing,
    Regular,
    Other,
}

/// The guest as the inspection reads it. Every method may fail with a
/// [`CoreError`] when the guest cannot be asked at all.
pub trait GuestSystem: Send {
    /// `shutil.which(name)`
    fn has_tool(&mut self, name: &str) -> impl Future<Output = Result<bool, CoreError>> + Send;
    /// `subprocess.check_output(argv)`: the output, or why the command
    /// failed.
    fn run(&mut self, argv: &[&str]) -> impl Future<Output = Result<Result<String, String>, CoreError>> + Send;
    /// `open(path).read()`
    fn read(&mut self, path: &str) -> impl Future<Output = Result<Result<String, FileError>, CoreError>> + Send;
    /// `os.path.realpath` of each path, in order.
    fn realpaths(&mut self, paths: &[String]) -> impl Future<Output = Result<Vec<String>, CoreError>> + Send;
    /// `os.listdir(path)`, empty when it fails.
    fn list_dir(&mut self, path: &str) -> impl Future<Output = Result<Vec<String>, CoreError>> + Send;
    /// `os.stat(path)`, following links.
    fn file_kind(&mut self, path: &str) -> impl Future<Output = Result<FileKind, CoreError>> + Send;
}

/// Why the inspection stopped.
#[derive(Debug)]
pub enum InspectError {
    /// The guest's layout is not supported, with the script's message.
    Guest(String),
    /// The guest could not be asked.
    Core(CoreError),
}

impl From<CoreError> for InspectError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

type Inspect<T> = Result<T, InspectError>;

fn fail<T>(message: impl Into<String>) -> Inspect<T> {
    Err(InspectError::Guest(message.into()))
}

fn require(condition: bool, message: impl Into<String>) -> Inspect<()> {
    if condition { Ok(()) } else { fail(message) }
}

/// Python's `str(value or '')`.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::Number(number)) if number.as_f64() == Some(0.0) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Python's `str(value)`.
fn py_repr_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `decimal_int`: an integral number, from a JSON number or its text.
fn decimal_int(value: Option<&Value>, description: &str) -> Inspect<u64> {
    let invalid = || fail(format!("Invalid {description}: {}", py_repr_str(value)));
    let text = match value {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.trim().to_owned(),
        _ => return invalid(),
    };
    let unsigned = text.strip_prefix('+').unwrap_or(&text);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() && fraction.is_empty() || !digits(whole) || !digits(fraction) {
        if let Ok(float) = text.parse::<f64>()
            && float.is_finite()
        {
            if float.fract() != 0.0 {
                return fail(format!("Non-integer {description}"));
            }
            if (0.0..1.8e19).contains(&float) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked above")]
                return Ok(float as u64);
            }
        }
        return invalid();
    }
    if fraction.bytes().any(|b| b != b'0') {
        return fail(format!("Non-integer {description}"));
    }
    if whole.is_empty() {
        return Ok(0);
    }
    whole.parse().map_or_else(|_| invalid(), Ok)
}

/// A row of `lsblk`.
#[derive(Debug, Clone)]
struct Row {
    name: String,
    kind: String,
    pkname: Option<Value>,
    serial: String,
    size: Option<Value>,
    mm: String,
}

/// A line of `/etc/crypttab` split into fields, with octal escapes decoded.
fn crypttab_tokens(line: &str) -> Inspect<Vec<String>> {
    let line = line.split('#').next().unwrap_or_default().trim();
    if line.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\\' {
            let octal = bytes
                .get(index + 1..index + 4)
                .is_some_and(|digits| digits.iter().all(|d| (b'0'..=b'7').contains(d)));
            require(octal, "Unsupported escape in /etc/crypttab")?;
        }
    }
    let mut fields = Vec::new();
    for field in line.split_whitespace() {
        let mut decoded = String::new();
        let mut rest = field;
        while let Some(at) = rest.find('\\') {
            decoded.push_str(&rest[..at]);
            let code = u32::from_str_radix(&rest[at + 1..at + 4], 8).unwrap_or_default();
            decoded.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            rest = &rest[at + 4..];
        }
        decoded.push_str(rest);
        fields.push(decoded);
    }
    Ok(fields)
}

/// `report_rows`: the `key` rows of an LVM JSON report.
fn report_rows<'v>(value: &'v Value, key: &str) -> Vec<&'v Value> {
    value
        .get("report")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|report| report.get(key).and_then(Value::as_array))
        .flatten()
        .collect()
}

/// `Total devices N` of `btrfs filesystem show`.
fn btrfs_total_devices(show: &str) -> Option<u64> {
    let at = show.find("Total devices")?;
    let rest = &show[at + "Total devices".len()..];
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        return None;
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The `devid N ... path P` lines of `btrfs filesystem show`.
fn btrfs_devices(show: &str) -> Vec<(u64, String)> {
    let mut devices = Vec::new();
    for line in show.lines() {
        let Some(rest) = line.trim_start().strip_prefix("devid") else {
            continue;
        };
        let after = rest.trim_start();
        if after.len() == rest.len() {
            continue;
        }
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let Ok(devid) = digits.parse() else {
            continue;
        };
        let tail = &after[digits.len()..];
        let mut found = None;
        for (at, _) in tail.match_indices("path") {
            let boundary = tail[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            let value = &tail[at + 4..];
            let path = value.trim_start();
            if boundary && path.len() < value.len() && !path.trim_end().is_empty() {
                found = Some(path.trim_end().to_owned());
                break;
            }
        }
        if let Some(path) = found {
            devices.push((devid, path));
        }
    }
    devices
}

/// Options of a crypttab row the resize cannot honour.
fn unsupported_option(option: &str) -> bool {
    const PLAIN: [&str; 8] = [
        "keyscript",
        "offset",
        "skip",
        "sector-size",
        "keyfile-offset",
        "keyfile-size",
        "key-slot",
        "header",
    ];
    PLAIN.contains(&option) || PLAIN.iter().any(|name| option.starts_with(&format!("{name}=")))
}

struct Inspector<'g, G: GuestSystem> {
    guest: &'g mut G,
    rows: Vec<Row>,
    by_path: HashMap<String, usize>,
    real: HashMap<String, String>,
}

impl<G: GuestSystem> Inspector<'_, G> {
    async fn tool(&mut self, name: &str) -> Inspect<()> {
        let found = self.guest.has_tool(name).await?;
        require(found, format!("Missing guest tool: {name}"))
    }

    async fn command(&mut self, argv: &[&str]) -> Inspect<String> {
        match self.guest.run(argv).await? {
            Ok(output) => Ok(output.trim().to_owned()),
            Err(reason) => fail(format!(
                "Guest command failed: {}: {reason}",
                argv.first().copied().unwrap_or_default()
            )),
        }
    }

    async fn optional_command(&mut self, argv: &[&str]) -> Inspect<String> {
        Ok(self
            .guest
            .run(argv)
            .await?
            .map(|out| out.trim().to_owned())
            .unwrap_or_default())
    }

    async fn json_command(&mut self, argv: &[&str]) -> Inspect<Value> {
        let output = self.command(argv).await?;
        serde_json::from_str(&output).or_else(|error| {
            fail(format!(
                "Invalid output from {}: {error}",
                argv.first().copied().unwrap_or_default()
            ))
        })
    }

    async fn realpath(&mut self, path: &str) -> Inspect<String> {
        if let Some(real) = self.real.get(path) {
            return Ok(real.clone());
        }
        self.prefetch(&[path.to_owned()]).await?;
        Ok(self.real.get(path).cloned().unwrap_or_else(|| path.to_owned()))
    }

    async fn prefetch(&mut self, paths: &[String]) -> Inspect<()> {
        let wanted: Vec<String> = paths
            .iter()
            .filter(|path| !self.real.contains_key(*path))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let real = self.guest.realpaths(&wanted).await?;
        if real.len() != wanted.len() {
            return Err(CoreError::Unusable("realpath returned a different number of paths".to_owned()).into());
        }
        self.real.extend(wanted.into_iter().zip(real));
        Ok(())
    }

    async fn sysfs_value(&mut self, path: &str) -> Inspect<String> {
        Ok(self
            .guest
            .read(path)
            .await?
            .map(|text| text.trim().to_owned())
            .unwrap_or_default())
    }

    async fn row_for(&mut self, value: Option<&str>) -> Inspect<Option<usize>> {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let value = if value.starts_with('/') {
            value.to_owned()
        } else {
            format!("/dev/{value}")
        };
        let real = self.realpath(&value).await?;
        Ok(self.by_path.get(&real).copied())
    }

    async fn row_for_value(&mut self, value: Option<&Value>) -> Inspect<Option<usize>> {
        let text = py_str(value);
        self.row_for(Some(&text)).await
    }

    async fn row_key(&mut self, index: usize) -> Inspect<String> {
        let row = &self.rows[index];
        if !row.mm.is_empty() {
            return Ok(row.mm.clone());
        }
        let name = row.name.clone();
        self.realpath(&name).await
    }

    async fn dm_name(&mut self, index: usize) -> Inspect<String> {
        let name = self.rows[index].name.clone();
        let real = self.realpath(&name).await?;
        let base = basename(&real);
        if !base.starts_with("dm-") {
            return Ok(String::new());
        }
        self.sysfs_value(&format!("/sys/class/block/{base}/dm/name")).await
    }

    async fn parents(&mut self, index: usize) -> Inspect<Vec<usize>> {
        let mut found: Vec<(String, usize)> = Vec::new();
        let add = |key: String, row: usize, found: &mut Vec<(String, usize)>| {
            if let Some(slot) = found.iter_mut().find(|(known, _)| *known == key) {
                slot.1 = row;
            } else {
                found.push((key, row));
            }
        };
        let pkname = self.rows[index].pkname.clone();
        if let Some(candidate) = self.row_for_value(pkname.as_ref()).await? {
            let key = self.row_key(candidate).await?;
            add(key, candidate, &mut found);
        }
        let name = self.rows[index].name.clone();
        let real = self.realpath(&name).await?;
        let base = basename(&real).to_owned();
        let slaves = self.guest.list_dir(&format!("/sys/class/block/{base}/slaves")).await?;
        for slave in slaves {
            if let Some(candidate) = self.row_for(Some(&slave)).await? {
                let key = self.row_key(candidate).await?;
                add(key, candidate, &mut found);
            }
        }
        Ok(found.into_iter().map(|(_, row)| row).collect())
    }

    async fn walk_chain(&mut self, start: usize) -> Inspect<Vec<usize>> {
        let mut chain = vec![start];
        let mut seen = BTreeSet::from([self.row_key(start).await?]);
        let mut current = start;
        while !matches!(self.rows[current].kind.as_str(), "disk" | "part") {
            let parents = self.parents(current).await?;
            let name = self.rows[current].name.clone();
            require(!parents.is_empty(), format!("Cannot identify parent of {name}"))?;
            require(parents.len() == 1, format!("Ambiguous multiple parents for {name}"))?;
            current = parents[0];
            let key = self.row_key(current).await?;
            require(!seen.contains(&key), "Cyclic block-device parent chain")?;
            seen.insert(key);
            chain.push(current);
        }
        Ok(chain)
    }

    async fn same_block(&mut self, left: &str, right: &str) -> Inspect<bool> {
        if left.is_empty() || right.is_empty() {
            return Ok(false);
        }
        let left_row = self.row_for(Some(left)).await?;
        let right_row = self.row_for(Some(right)).await?;
        if let (Some(left_row), Some(right_row)) = (left_row, right_row) {
            return Ok(self.row_key(left_row).await? == self.row_key(right_row).await?);
        }
        Ok(self.realpath(left).await? == self.realpath(right).await?)
    }

    async fn crypttab_rows(&mut self) -> Inspect<Vec<Vec<String>>> {
        let text = match self.guest.read("/etc/crypttab").await? {
            Ok(text) => text,
            Err(FileError::Missing) => return Ok(Vec::new()),
            Err(FileError::Unreadable(reason)) => return fail(format!("Cannot read /etc/crypttab: {reason}")),
        };
        let mut rows = Vec::new();
        for line in text.lines() {
            let fields = crypttab_tokens(line)?;
            if fields.is_empty() {
                continue;
            }
            require((3..=4).contains(&fields.len()), "Unsupported /etc/crypttab row")?;
            rows.push(fields);
        }
        Ok(rows)
    }
}

/// `resize_guest.INSPECT` for the filesystem mounted at `mount`, on the
/// disk of `role`; `label` is the home's serial and label.
pub async fn inspect(guest: &mut impl GuestSystem, mount: &str, role: Role, label: &str) -> Inspect<GuestLayout> {
    let mut this = Inspector {
        guest,
        rows: Vec::new(),
        by_path: HashMap::new(),
        real: HashMap::new(),
    };
    for tool in ["findmnt", "lsblk", "blkid", "blockdev"] {
        this.tool(tool).await?;
    }
    let found = this
        .json_command(&[
            "findmnt",
            "--json",
            "--mountpoint",
            mount,
            "-o",
            "SOURCE,FSTYPE,TARGET,OPTIONS,MAJ:MIN",
        ])
        .await?;
    let filesystems = found
        .get("filesystems")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    require(
        filesystems.len() == 1,
        format!("Cannot identify exact mounted filesystem: {mount}"),
    )?;
    let fs = &filesystems[0];
    let filesystem = py_str(fs.get("fstype"));
    require(
        fs.get("target").and_then(Value::as_str) == Some(mount) && matches!(filesystem.as_str(), "ext4" | "btrfs"),
        format!("Resize requires an exact ext4 or btrfs mount: {mount}"),
    )?;
    let options = py_str(fs.get("options"));
    require(
        options.split(',').any(|option| option == "rw"),
        "Filesystem is read-only",
    )?;
    let fs_mm = py_str(fs.get("maj:min"));
    this.tool(if filesystem == "ext4" { "resize2fs" } else { "btrfs" })
        .await?;

    let listed = this
        .json_command(&[
            "lsblk",
            "--json",
            "--bytes",
            "--paths",
            "--list",
            "-o",
            "NAME,TYPE,PKNAME,SERIAL,SIZE,MAJ:MIN",
        ])
        .await?;
    let mut flat = Vec::new();
    let mut stack: Vec<Value> = listed
        .get("blockdevices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    stack.reverse();
    while let Some(row) = stack.pop() {
        let children = row
            .get("children")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        flat.push(row);
        stack.extend(children.into_iter().rev());
    }
    let mut by_mm: HashMap<String, usize> = HashMap::new();
    for raw in flat {
        let name = py_str(raw.get("name"));
        if name.is_empty() {
            continue;
        }
        let name = if name.starts_with('/') {
            name
        } else {
            format!("/dev/{name}")
        };
        let mm = py_str(raw.get("maj:min"));
        if !mm.is_empty() {
            require(!by_mm.contains_key(&mm), format!("Ambiguous lsblk MAJ:MIN: {mm}"))?;
            by_mm.insert(mm.clone(), this.rows.len());
        }
        this.rows.push(Row {
            name,
            kind: py_str(raw.get("type")),
            pkname: raw.get("pkname").cloned(),
            serial: py_str(raw.get("serial")),
            size: raw.get("size").cloned(),
            mm,
        });
    }
    let names: Vec<String> = this
        .rows
        .iter()
        .flat_map(|row| [row.name.clone(), format!("/dev/{}", basename(&row.name))])
        .collect();
    this.prefetch(&names).await?;
    for index in 0..this.rows.len() {
        let name = this.rows[index].name.clone();
        let real = this.realpath(&name).await?;
        this.by_path.insert(real, index);
        let real = this.realpath(&format!("/dev/{}", basename(&name))).await?;
        this.by_path.insert(real, index);
    }

    let source = py_str(fs.get("source"));
    let (entry, btrfs_devid) = if filesystem == "ext4" {
        require(
            !fs_mm.is_empty(),
            format!("Mounted filesystem has no MAJ:MIN: {source}"),
        )?;
        let matches: Vec<usize> = (0..this.rows.len()).filter(|i| this.rows[*i].mm == fs_mm).collect();
        require(
            matches.len() == 1,
            format!("Cannot identify mounted block device: {source} ({fs_mm})"),
        )?;
        (matches[0], None)
    } else {
        let show = this.command(&["btrfs", "filesystem", "show", "--raw", mount]).await?;
        require(
            btrfs_total_devices(&show) == Some(1),
            "Resize requires a single-device btrfs filesystem",
        )?;
        let devices = btrfs_devices(&show);
        require(
            devices.len() == 1 && !matches!(devices[0].1.as_str(), "unknown" | "-"),
            "Cannot identify the btrfs backing device",
        )?;
        let (devid, path) = devices[0].clone();
        let mut entry: Option<usize> = None;
        let mount_source = source.split('[').next().unwrap_or_default().to_owned();
        for candidate in [path.trim().to_owned(), mount_source] {
            if candidate.is_empty() {
                continue;
            }
            if let Some(found) = this.row_for(Some(&candidate)).await? {
                match entry {
                    None => entry = Some(found),
                    Some(known) => {
                        let same = this.row_key(known).await? == this.row_key(found).await?;
                        require(same, "Mounted btrfs device does not match filesystem info")?;
                    }
                }
            }
        }
        let Some(entry) = entry else {
            return fail("Cannot identify the mounted btrfs block device");
        };
        (entry, Some(devid))
    };

    let chain = this.walk_chain(entry).await?;
    let types: Vec<String> = chain.iter().map(|i| this.rows[*i].kind.clone()).collect();
    require(
        types
            .iter()
            .all(|kind| matches!(kind.as_str(), "disk" | "part" | "crypt" | "lvm")),
        format!("Unsupported block-device layout: {}", types.join(" -> ")),
    )?;
    let position = |kind: &str| types.iter().position(|t| t == kind);
    let count = |kind: &str| types.iter().filter(|t| *t == kind).count();
    require(
        count("crypt") <= 1 && count("lvm") <= 1,
        "Nested encrypted or LVM layouts are not supported",
    )?;
    if let (Some(crypt), Some(lvm)) = (position("crypt"), position("lvm")) {
        require(lvm < crypt, "Unsupported LVM/encryption nesting")?;
    }

    let physical = chain[chain.len() - 1];
    let entry_name = this.rows[entry].name.clone();
    require(
        matches!(this.rows[physical].kind.as_str(), "disk" | "part"),
        format!("Unsupported mounted block device: {entry_name}"),
    )?;
    let parent = if this.rows[physical].kind == "part" {
        let pkname = this.rows[physical].pkname.clone();
        let partition_parent = this.row_for_value(pkname.as_ref()).await?;
        let Some(partition_parent) = partition_parent.filter(|p| this.rows[*p].kind == "disk") else {
            return fail("Unsupported partition parent");
        };
        partition_parent
    } else {
        physical
    };

    let disks: Vec<usize> = (0..this.rows.len())
        .filter(|i| this.rows[*i].kind == "disk" && !basename(&this.rows[*i].name).starts_with("zram"))
        .collect();
    require(disks.len() == 2, "Resize requires exactly the root and home data disks")?;
    let homes: Vec<usize> = disks
        .iter()
        .copied()
        .filter(|i| this.rows[*i].serial.trim() == label)
        .collect();
    require(homes.len() == 1, "Cannot identify the persistent home disk by serial")?;
    let on_home = this.row_key(parent).await? == this.row_key(homes[0]).await?;
    require(
        on_home == (role == Role::Home),
        "Mounted filesystem does not match the selected workspace disk",
    )?;
    if role == Role::Home {
        let found = this
            .command(&["blkid", "-s", "LABEL", "-o", "value", &entry_name])
            .await?;
        require(found == label, "Persistent home label mismatch")?;
    }

    let mut partition_device = None;
    let mut partition = None;
    if this.rows[physical].kind == "part" {
        this.tool("growpart").await?;
        let physical_name = this.rows[physical].name.clone();
        let real = this.realpath(&physical_name).await?;
        let base = basename(&real).to_owned();
        let number = this.sysfs_value(&format!("/sys/class/block/{base}/partition")).await?;
        let first = this.sysfs_value(&format!("/sys/class/block/{base}/start")).await?;
        let (Ok(number), Ok(start)) = (number.parse::<i64>(), first.parse::<i64>()) else {
            return fail(format!("Cannot inspect partition geometry: {physical_name}"));
        };
        require(number > 0, format!("Invalid partition number: {physical_name}"))?;
        partition =
            Some(u32::try_from(number).or_else(|_| fail(format!("Invalid partition number: {physical_name}")))?);
        partition_device = Some(physical_name);
        let parent_key = this.row_key(parent).await?;
        for candidate in 0..this.rows.len() {
            if this.rows[candidate].kind != "part" {
                continue;
            }
            let pkname = this.rows[candidate].pkname.clone();
            let Some(candidate_parent) = this.row_for_value(pkname.as_ref()).await? else {
                continue;
            };
            if this.row_key(candidate_parent).await? != parent_key {
                continue;
            }
            let candidate_name = this.rows[candidate].name.clone();
            let real = this.realpath(&candidate_name).await?;
            let candidate_base = basename(&real).to_owned();
            let value = this
                .sysfs_value(&format!("/sys/class/block/{candidate_base}/start"))
                .await?;
            let Ok(candidate_start) = value.parse::<i64>() else {
                return fail(format!("Cannot inspect partition geometry: {candidate_name}"));
            };
            require(
                candidate_start <= start,
                "Selected partition is not the last partition on disk",
            )?;
        }
    }

    let mut crypt = None;
    if let Some(at) = position("crypt") {
        this.tool("cryptsetup").await?;
        let crypt_row = chain[at];
        let crypt_parents = this.parents(crypt_row).await?;
        require(
            crypt_parents.len() == 1,
            "Encrypted mapping has ambiguous backing devices",
        )?;
        let backing = this.rows[crypt_parents[0]].name.clone();
        let kind = this
            .optional_command(&["blkid", "-s", "TYPE", "-o", "value", &backing])
            .await?;
        require(
            kind == "crypto_LUKS",
            format!("Encrypted backing device is not LUKS: {backing}"),
        )?;
        let mut mapper_name = this.dm_name(crypt_row).await?;
        if mapper_name.is_empty() {
            let name = this.rows[crypt_row].name.clone();
            mapper_name = if name.contains("/mapper/") {
                basename(&name).to_owned()
            } else {
                basename(&this.realpath(&name).await?).to_owned()
            };
        }
        let uuid = this
            .optional_command(&["blkid", "-s", "UUID", "-o", "value", &backing])
            .await?;
        let partuuid = this
            .optional_command(&["blkid", "-s", "PARTUUID", "-o", "value", &backing])
            .await?;
        let backing_label = this
            .optional_command(&["blkid", "-s", "LABEL", "-o", "value", &backing])
            .await?;
        let backing_real = this.realpath(&backing).await?;
        let mut matches = Vec::new();
        for fields in this.crypttab_rows().await? {
            if fields[0] != mapper_name {
                continue;
            }
            let source = fields[1].as_str();
            let source_match = if let Some(value) = source.strip_prefix("UUID=") {
                value == uuid
            } else if let Some(value) = source.strip_prefix("PARTUUID=") {
                value == partuuid
            } else if let Some(value) = source.strip_prefix("LABEL=") {
                value == backing_label
            } else if matches!(source, "none" | "-") {
                false
            } else {
                this.realpath(source).await? == backing_real
            };
            if source_match {
                matches.push(fields);
            }
        }
        require(
            matches.len() <= 1,
            format!("Ambiguous /etc/crypttab rows for {mapper_name}"),
        )?;
        let mut key_file = None;
        let mut auth = CryptAuth::Passphrase;
        if let Some(fields) = matches.first() {
            let key_value = fields[2].clone();
            let options: Vec<&str> = match fields.get(3) {
                Some(options) if !options.is_empty() => options.split(',').collect(),
                _ => Vec::new(),
            };
            if let Some(option) = options.iter().find(|option| unsupported_option(option)) {
                return fail(format!("Unsupported /etc/crypttab option: {option}"));
            }
            if !matches!(key_value.as_str(), "none" | "-") {
                require(
                    key_value.starts_with('/'),
                    format!("Crypttab key file must be an absolute path: {key_value}"),
                )?;
                match this.guest.file_kind(&key_value).await? {
                    FileKind::Missing => return fail(format!("Crypttab key file is missing: {key_value}")),
                    FileKind::Other => return fail(format!("Crypttab key file is not a regular file: {key_value}")),
                    FileKind::Regular => {}
                }
                key_file = Some(key_value);
                auth = CryptAuth::Keyfile;
            }
        }
        crypt = Some(CryptLayer {
            name: mapper_name,
            device: backing,
            key_file,
            auth,
        });
    }

    let mut lvm = None;
    if let Some(at) = position("lvm") {
        for tool in ["pvs", "vgs", "lvs", "pvresize", "lvextend"] {
            this.tool(tool).await?;
        }
        let lv_name = this.rows[chain[at]].name.clone();
        let lvs = this
            .json_command(&[
                "lvs",
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--noheadings",
                "-o",
                "lv_path,lv_name,vg_name,lv_size,lv_attr,segtype",
            ])
            .await?;
        let mut candidates = Vec::new();
        for candidate in report_rows(&lvs, "lv") {
            let path = py_str(candidate.get("lv_path"));
            if this.same_block(&path, &lv_name).await? {
                candidates.push(candidate.clone());
            }
        }
        require(candidates.len() == 1, "Cannot identify mounted logical volume")?;
        let selected = &candidates[0];
        let attrs = py_str(selected.get("lv_attr"));
        require(
            attrs.starts_with('-') && py_str(selected.get("segtype")) == "linear",
            "Only ordinary linear logical volumes are supported",
        )?;
        let vg_name = py_str(selected.get("vg_name"));
        require(!vg_name.is_empty(), "Logical volume has no volume group")?;
        let in_vg = report_rows(&lvs, "lv")
            .into_iter()
            .filter(|candidate| py_str(candidate.get("vg_name")) == vg_name)
            .count();
        require(
            in_vg == 1,
            "Resize requires a dedicated volume group with one logical volume",
        )?;
        let vgs = this
            .json_command(&[
                "vgs",
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--noheadings",
                "-o",
                "vg_name,vg_extent_size",
            ])
            .await?;
        let vg_rows: Vec<&Value> = report_rows(&vgs, "vg")
            .into_iter()
            .filter(|candidate| py_str(candidate.get("vg_name")) == vg_name)
            .collect();
        require(vg_rows.len() == 1, format!("Cannot identify volume group: {vg_name}"))?;
        let pvs = this
            .json_command(&[
                "pvs",
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--noheadings",
                "-o",
                "pv_name,vg_name",
            ])
            .await?;
        let pv_rows: Vec<&Value> = report_rows(&pvs, "pv")
            .into_iter()
            .filter(|candidate| py_str(candidate.get("vg_name")) == vg_name)
            .collect();
        require(
            pv_rows.len() == 1,
            "Resize requires a volume group with one physical volume",
        )?;
        let pv_path = py_str(pv_rows[0].get("pv_name"));
        let mut below = false;
        if !pv_path.is_empty() {
            for row in &chain[at + 1..] {
                let name = this.rows[*row].name.clone();
                if this.same_block(&pv_path, &name).await? {
                    below = true;
                    break;
                }
            }
        }
        require(
            below,
            "Volume group physical volume does not match the mounted disk chain",
        )?;
        lvm = Some(LvmLayer {
            lv_path: py_str(selected.get("lv_path")),
            pv_path,
            lv_size_bytes: decimal_int(selected.get("lv_size"), "logical volume size")?,
            extent_size_bytes: decimal_int(vg_rows[0].get("vg_extent_size"), "volume group extent size")?,
            vg_name,
        });
    }

    Ok(GuestLayout {
        device: entry_name,
        parent: this.rows[parent].name.clone(),
        partition,
        size_bytes: decimal_int(this.rows[parent].size.as_ref(), "disk size")?,
        partition_device,
        crypt,
        lvm,
        filesystem,
        mount: mount.to_owned(),
        btrfs_devid,
    })
}

/// The programs the inspection and the growth may need; [`QgaGuest`] looks
/// them all up with one command.
const TOOLS: [&str; 13] = [
    "findmnt",
    "lsblk",
    "blkid",
    "blockdev",
    "resize2fs",
    "btrfs",
    "growpart",
    "cryptsetup",
    "pvs",
    "vgs",
    "lvs",
    "pvresize",
    "lvextend",
];

/// [`GuestSystem`] through the QEMU guest agent: `exec` as root (needs
/// `VM.GuestAgent.Unrestricted`) and `file-read` (`VM.GuestAgent.FileRead`).
pub struct QgaGuest<'p, P: Pve> {
    pve: &'p P,
    node: String,
    vmid: u32,
    missing: Option<BTreeSet<String>>,
    timeout: Duration,
}

impl<'p, P: Pve> QgaGuest<'p, P> {
    #[must_use]
    pub fn new(pve: &'p P, node: &str, vmid: u32) -> Self {
        Self {
            pve,
            node: node.to_owned(),
            vmid,
            missing: None,
            timeout: GUEST_EXEC_TIMEOUT,
        }
    }

    async fn shell(&self, command: &str) -> Result<crate::guest::ExecResult, CoreError> {
        guest_exec(self.pve, &self.node, self.vmid, command, false, self.timeout).await
    }

    async fn checked(&self, command: &str) -> Result<String, CoreError> {
        let result = self.shell(command).await?;
        if result.exitcode != 0 {
            return Err(CoreError::Failed(format!(
                "Guest command failed in VM {} (exit={}): {command}",
                self.vmid, result.exitcode
            )));
        }
        Ok(result.out)
    }
}

/// The shell loop that prints the programs missing from the `PATH`.
#[must_use]
pub fn missing_tools_command(tools: &[&str]) -> String {
    let words: Vec<String> = tools.iter().map(|tool| shell_quote(tool)).collect();
    format!(
        r#"for c in {}; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#,
        words.join(" ")
    )
}

impl<P: Pve> GuestSystem for QgaGuest<'_, P> {
    async fn has_tool(&mut self, name: &str) -> Result<bool, CoreError> {
        if !TOOLS.contains(&name) {
            let out = self.checked(&missing_tools_command(&[name])).await?;
            return Ok(out.trim().is_empty());
        }
        if self.missing.is_none() {
            let out = self.checked(&missing_tools_command(&TOOLS)).await?;
            self.missing = Some(out.lines().map(|line| line.trim().to_owned()).collect());
        }
        Ok(!self.missing.as_ref().is_some_and(|missing| missing.contains(name)))
    }

    async fn run(&mut self, argv: &[&str]) -> Result<Result<String, String>, CoreError> {
        let result = self.shell(&shell_join(argv)).await?;
        if result.exitcode == 0 {
            return Ok(Ok(result.out));
        }
        let detail = [result.err.trim(), result.out.trim()]
            .into_iter()
            .find(|part| !part.is_empty())
            .unwrap_or_default()
            .to_owned();
        let mut reason = format!("exit status {}", result.exitcode);
        if !detail.is_empty() {
            reason.push_str(": ");
            reason.push_str(&detail);
        }
        Ok(Err(reason))
    }

    async fn read(&mut self, path: &str) -> Result<Result<String, FileError>, CoreError> {
        read_text_or_reason(self.pve, &self.node, self.vmid, path).await
    }

    async fn realpaths(&mut self, paths: &[String]) -> Result<Vec<String>, CoreError> {
        let words: Vec<&str> = paths.iter().map(String::as_str).collect();
        let out = self.checked(&format!("realpath -m -- {}", shell_join(&words))).await?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    async fn list_dir(&mut self, path: &str) -> Result<Vec<String>, CoreError> {
        let out = self
            .checked(&format!("ls -1A -- {} 2>/dev/null || true", shell_quote(path)))
            .await?;
        Ok(out.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect())
    }

    async fn file_kind(&mut self, path: &str) -> Result<FileKind, CoreError> {
        let quoted = shell_quote(path);
        let out = self
            .checked(&format!(
                "if [ -e {quoted} ]; then if [ -f {quoted} ]; then echo regular; else echo other; fi; else echo missing; fi"
            ))
            .await?;
        Ok(match out.trim() {
            "regular" => FileKind::Regular,
            "other" => FileKind::Other,
            _ => FileKind::Missing,
        })
    }
}
