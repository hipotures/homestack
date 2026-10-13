//! Which FIDO security keys a workspace SSH login may see (Python
//! `fido.py`, I2).
//!
//! OpenSSH asks every attached FIDO device in USB order, so a locked second
//! key (for example a Trezor) can ask for a PIN before the key that owns the
//! login handle is tried. The workspace SSH master therefore runs in a
//! bubblewrap mount namespace that shows only the preferred kind of device.

use crate::AppError;
use crate::system::System;

/// Names accepted in `[workspace_ssh] fido_devices`, with their USB vendor
/// IDs and labels.
pub const NAMES: [(&str, &[&str], &str); 2] = [("yubikey", &["1050"], "YubiKey"), ("trezor", &["1209"], "Trezor")];

/// One `fido2-token -L` device, IDs as four lower-case hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    pub vendor: String,
    pub product: String,
}

fn vendors(entry: &str) -> Option<&'static [&'static str]> {
    NAMES.iter().find(|(name, _, _)| *name == entry).map(|(_, ids, _)| *ids)
}

fn hex4(text: &str) -> bool {
    text.len() == 4 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One `fido_devices` entry normalized: a known name, `vvvv` or `vvvv:pppp`.
pub fn validate_entry(value: &str) -> Result<String, AppError> {
    let entry = value.trim().to_lowercase();
    let usb = match entry.split_once(':') {
        None => hex4(&entry),
        Some((vendor, product)) => hex4(vendor) && hex4(product),
    };
    if vendors(&entry).is_some() || usb {
        return Ok(entry);
    }
    Err(AppError::new(
        "[workspace_ssh] fido_devices entries must be yubikey, trezor or a USB id such as 1050 or 1050:0407",
    ))
}

/// `YubiKey` for `yubikey`; other entries as they are.
#[must_use]
pub fn label(entry: &str) -> String {
    NAMES
        .iter()
        .find(|(name, _, _)| *name == entry)
        .map_or_else(|| entry.to_owned(), |(_, _, label)| (*label).to_owned())
}

fn hex_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let value = text.strip_prefix(key)?.strip_prefix("0x")?;
    let end = value.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(value.len());
    (end > 0).then(|| &value[..end])
}

/// Parse `fido2-token -L`: `/dev/hidrawN: vendor=0x1050, product=0x0407 ...`.
#[must_use]
pub fn parse_listing(text: &str) -> Vec<Device> {
    text.lines()
        .filter_map(|line| {
            let (path, rest) = line.split_once(": ")?;
            let number = path.strip_prefix("/dev/hidraw")?;
            if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let (vendor, rest) = rest.split_once(", ")?;
            let vendor = hex_field(vendor, "vendor=")?;
            let product = hex_field(rest, "product=")?;
            Some(Device {
                path: path.to_owned(),
                vendor: format!("{:0>4}", vendor.to_lowercase()),
                product: format!("{:0>4}", product.to_lowercase()),
            })
        })
        .collect()
}

/// The attached devices (`fido2-token -L`, required).
pub fn list_devices(system: &dyn System) -> Result<Vec<Device>, AppError> {
    if system.which("fido2-token").is_none() {
        return Err(AppError::new(
            "fido2-token (libfido2 tools) is required for [workspace_ssh] fido_devices",
        ));
    }
    let output = system
        .run(&["fido2-token".to_owned(), "-L".to_owned()])
        .map_err(|_| AppError::new("Could not list security keys with fido2-token"))?;
    Ok(parse_listing(&output.stdout))
}

fn matches(entry: &str, device: &Device) -> bool {
    if let Some(ids) = vendors(entry) {
        return ids.contains(&device.vendor.as_str());
    }
    match entry.split_once(':') {
        Some(_) => format!("{}:{}", device.vendor, device.product) == entry,
        None => device.vendor == entry,
    }
}

/// The first preferred kind with an attached device, and all its devices.
pub fn select(preference: &[String], devices: &[Device]) -> Result<(String, Vec<Device>), AppError> {
    for entry in preference {
        let found: Vec<Device> = devices.iter().filter(|d| matches(entry, d)).cloned().collect();
        if !found.is_empty() {
            return Ok((entry.clone(), found));
        }
    }
    Err(AppError::new(format!(
        "No configured security key is connected ({}); plug one in and retry.",
        preference
            .iter()
            .map(|entry| label(entry))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Why the chosen kind cannot log in to the host, from its `IdentityFile`
/// names: three short lines, or `None` when the host lists a key of that
/// kind or names no kind at all.
#[must_use]
pub fn missing_key_message(
    entry: &str,
    identity_files: &[String],
    name: &str,
    address: Option<&str>,
) -> Option<String> {
    vendors(entry)?;
    let mut named: Vec<&str> = NAMES
        .iter()
        .map(|(kind, _, _)| *kind)
        .filter(|kind| identity_files.iter().any(|path| path.to_lowercase().contains(kind)))
        .collect();
    if named.is_empty() || named.contains(&entry) {
        return None;
    }
    named.sort_unstable();
    let others = named.iter().map(|kind| label(kind)).collect::<Vec<_>>().join(" or ");
    let who = address.map_or_else(|| name.to_owned(), |address| format!("{name} ({address})"));
    let wanted = label(entry);
    Some(format!(
        "{who} has no {wanted} key configured.\nOnly {others} keys are set up there.\nConnect a {others} or add a {wanted} key to the host."
    ))
}

/// The kind of security key an identity file is named after.
#[must_use]
pub fn kind_of(path: &str) -> Option<&'static str> {
    let lowered = path.to_lowercase();
    NAMES
        .iter()
        .map(|(kind, _, _)| *kind)
        .find(|kind| lowered.contains(kind))
}

/// Drop `-i FILE` pairs whose file names another kind than `entry`; files
/// that name no kind stay.
#[must_use]
pub fn filter_identity_options(entry: &str, options: &[String]) -> Vec<String> {
    if vendors(entry).is_none() {
        return options.to_vec();
    }
    let mut kept = Vec::with_capacity(options.len());
    let mut index = 0;
    while index < options.len() {
        if options[index] == "-i" && index + 1 < options.len() {
            let lowered = options[index + 1].to_lowercase();
            if !NAMES
                .iter()
                .any(|(kind, _, _)| *kind != entry && lowered.contains(kind))
            {
                kept.extend_from_slice(&options[index..index + 2]);
            }
            index += 2;
        } else {
            kept.push(options[index].clone());
            index += 1;
        }
    }
    kept
}

/// The argv prefix that runs a command with only `devices` below `/dev`.
pub fn sandbox_prefix(system: &dyn System, devices: &[Device]) -> Result<Vec<String>, AppError> {
    let bwrap = system.which("bwrap").ok_or_else(|| {
        AppError::new(
            "bubblewrap (bwrap) is required for [workspace_ssh] fido_devices; install it or remove the setting.",
        )
    })?;
    let mut prefix = vec![bwrap];
    prefix.extend(["--bind", "/", "/", "--dev", "/dev"].map(str::to_owned));
    for device in devices {
        prefix.extend(["--dev-bind".to_owned(), device.path.clone(), device.path.clone()]);
    }
    Ok(prefix)
}

/// A hint for when several kinds are attached and no preference is set.
#[must_use]
pub fn advice(devices: &[Device]) -> Option<String> {
    let mut vendors: Vec<&str> = devices.iter().map(|d| d.vendor.as_str()).collect();
    vendors.sort_unstable();
    vendors.dedup();
    if vendors.len() < 2 {
        return None;
    }
    let mut names: Vec<String> = vendors
        .iter()
        .map(|vendor| {
            NAMES
                .iter()
                .find(|(_, ids, _)| ids.contains(vendor))
                .map_or_else(|| (*vendor).to_owned(), |(_, _, label)| (*label).to_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    Some(format!(
        "Several security keys are connected ({}). A key that is not used for this login may ask for a PIN: reject it on that device, unplug it, or set [workspace_ssh] fido_devices to choose one.",
        names.join(", ")
    ))
}
