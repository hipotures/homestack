//! Private, atomic application selections under the active XDG HomeStack profile.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::deploy::Source;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub repository: String,
    pub service: String,
    pub domain: String,
    pub hostname: String,
    pub revision: String,
    pub values: BTreeMap<String, Value>,
}

fn path(root: &Path, source: &Source, hostname: &str, domain: &str) -> PathBuf {
    let mut identity = Sha256::new();
    for part in [&source.repository, &source.manifest.id, hostname, domain] {
        identity.update((part.len() as u64).to_be_bytes());
        identity.update(part.as_bytes());
    }
    let hash: String = identity.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    root.join("services").join(format!("{hash}.json"))
}

pub fn load(
    root: &Path,
    source: &Source,
    hostname: &str,
    domain: &str,
    required: bool,
) -> Result<Option<Saved>, String> {
    let path = path(root, source, hostname, domain);
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Ok(meta)
                if meta.is_dir()
                    && meta.permissions().mode() & 0o077 == 0
                    && meta.uid() == rustix::process::getuid().as_raw() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
            _ => return Err("Application selections directory must be private and owned by this user".into()),
        }
    }
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(_) => return Err("Private application selections are unavailable; restore this deployment's local configuration before Review/Update".into()),
    };
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect private application selections")?;
    if !meta.is_file()
        || meta.permissions().mode() & 0o077 != 0
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.len() > 128 * 1024
    {
        return Err("Private application selections must be a regular owner-only file smaller than 128 KiB".into());
    }
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read private application selections")?;
    if bytes.len() > 128 * 1024 {
        return Err("Private application selections exceed 128 KiB".into());
    }
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| "Invalid private application selections")?;
    if saved.repository != source.repository
        || saved.service != source.manifest.id
        || saved.hostname != hostname
        || saved.domain != domain
    {
        return Err("Private application selections have a different deployment identity".into());
    }
    // Values are checked against the current manifest where they are used
    // (Review and image replacement), not here: a value the manifest no
    // longer accepts must still reach the form so the operator can fix it.
    if source.manifest.schema3.is_none() {
        return Err("Application selections require schema 3".into());
    }
    Ok(Some(saved))
}

pub fn save(
    root: &Path,
    source: &Source,
    hostname: &str,
    domain: &str,
    values: &BTreeMap<String, Value>,
) -> Result<(), String> {
    let contract = source
        .manifest
        .schema3
        .as_ref()
        .ok_or("Application selections require schema 3")?;
    contract.resolve(values)?;
    // HomeStack's own settings are read from it each time, never kept here.
    let values: BTreeMap<String, Value> = values
        .iter()
        .filter(|(id, _)| {
            !contract
                .inputs
                .iter()
                .any(|input| &input.id == *id && input.from_homestack())
        })
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect();
    let path = path(root, source, hostname, domain);
    let parent = path.parent().ok_or("Invalid selections path")?;
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let meta = std::fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() || meta.permissions().mode() & 0o077 != 0
        {
            return Err("Application selections directory must be private and owned by this user".into());
        }
        let saved = Saved {
            repository: source.repository.clone(),
            service: source.manifest.id.clone(),
            hostname: hostname.into(),
            domain: domain.into(),
            revision: source.blob.clone(),
            values: values.clone(),
        };
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let encoded = serde_json::to_vec(&saved)?;
        if encoded.len() > 128 * 1024 {
            return Err("Private application selections exceed 128 KiB".into());
        }
        temporary.write_all(&encoded)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    write().map_err(|_| "Cannot atomically save private application selections".into())
}

/// Instance numbers belong to the service identity, not to a Proxmox node.
pub fn base_name(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((base, number)) if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => name,
    }
}
