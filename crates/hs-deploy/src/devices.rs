//! Structured Proxmox LXC device passthrough, independent of storage volumes.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

pub type Devices = BTreeMap<String, Device>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub path: String,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

pub fn valid_slot(key: &str) -> bool {
    key.strip_prefix("dev")
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| key == format!("dev{number}"))
}

pub fn validate(devices: &Devices) -> Result<(), String> {
    let mut paths = BTreeSet::new();
    for (slot, device) in devices {
        if !valid_slot(slot) {
            return Err(format!("{slot}: expected dev0 through dev255"));
        }
        if !device.path.starts_with("/dev/")
            || device
                .path
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || device
                .path
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ',' | '='))
        {
            return Err(format!(
                "{slot}: expected an absolute device path below /dev without traversal or property separators"
            ));
        }
        if !paths.insert(&device.path) {
            return Err(format!("{slot}: duplicate device path"));
        }
        if device.mode.as_ref().is_some_and(|mode| {
            mode.len() != 4 || !mode.starts_with('0') || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
        }) {
            return Err(format!("{slot}: mode must be a quoted octal string such as \"0660\""));
        }
    }
    Ok(())
}

impl Device {
    pub fn same_access(&self, other: &Self) -> bool {
        self.path == other.path
            && self.uid.unwrap_or(0) == other.uid.unwrap_or(0)
            && self.gid.unwrap_or(0) == other.gid.unwrap_or(0)
            && self.mode.as_deref().unwrap_or("0660") == other.mode.as_deref().unwrap_or("0660")
            && self.read_only == other.read_only
    }

    pub fn property(&self) -> String {
        let mut parts = vec![format!("path={}", self.path)];
        if let Some(uid) = self.uid {
            parts.push(format!("uid={uid}"));
        }
        if let Some(gid) = self.gid {
            parts.push(format!("gid={gid}"));
        }
        if let Some(mode) = &self.mode {
            parts.push(format!("mode={mode}"));
        }
        parts.push(format!("deny-write={}", u8::from(self.read_only)));
        parts.join(",")
    }

    fn parse(property: &str) -> Result<Self, String> {
        let mut fields = BTreeMap::new();
        for (index, part) in property.split(',').enumerate() {
            let (key, value) = match part.split_once('=') {
                Some(pair) => pair,
                None if index == 0 => ("path", part),
                None => return Err("Invalid device property".into()),
            };
            if !["path", "uid", "gid", "mode", "deny-write"].contains(&key) || fields.insert(key, value).is_some() {
                return Err("Unknown or duplicate device property".into());
            }
        }
        let id = |key| -> Result<Option<u32>, String> {
            fields
                .get(key)
                .map(|value| value.parse().map_err(|_| format!("Invalid device {key}")))
                .transpose()
        };
        Ok(Self {
            path: fields.get("path").ok_or("Device path is missing")?.to_string(),
            uid: id("uid")?,
            gid: id("gid")?,
            mode: fields.get("mode").map(|value| value.to_string()),
            read_only: match fields.get("deny-write").copied() {
                None | Some("0") => false,
                Some("1") => true,
                _ => return Err("Invalid device deny-write".into()),
            },
        })
    }
}

pub fn read(config: &Value) -> Result<Devices, String> {
    let mut devices = Devices::new();
    for (key, value) in config.as_object().ok_or("Invalid container configuration")? {
        if key.starts_with("dev") {
            let property = value
                .as_str()
                .ok_or_else(|| format!("{key}: invalid device configuration"))?;
            devices.insert(
                key.clone(),
                Device::parse(property).map_err(|error| format!("{key}: {error}"))?,
            );
        }
    }
    validate(&devices)?;
    Ok(devices)
}

pub fn fields(devices: &Devices) -> Vec<(String, String)> {
    devices
        .iter()
        .map(|(slot, device)| (slot.clone(), device.property()))
        .collect()
}

/// Declared slots take the manifest's settings; other attached devices survive.
pub fn merged(existing: &Devices, declared: &Devices) -> Result<Devices, String> {
    let mut devices = existing.clone();
    devices.extend(declared.clone());
    validate(&devices)?;
    Ok(devices)
}

/// New device attachment requires Proxmox root@pam authorization. Existing
/// mappings can be retained by switching only the root disk through the API.
pub fn require_deploy_token_access(devices: &Devices) -> Result<(), String> {
    if devices.is_empty() {
        Ok(())
    } else {
        Err("New device attachment requires root@pam in Proxmox. The deploy token cannot create device mappings; existing matching mappings are preserved during image updates through the API.".into())
    }
}
