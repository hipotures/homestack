//! The unfinished deployment form of a service under the active XDG HomeStack
//! profile: saved at Review, removed after a successful deployment. Values
//! are setting titles mapped to text or to the chosen choice values; secrets
//! are never saved.
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::deploy::Source;

pub type Values = BTreeMap<String, Value>;

fn path(root: &Path, source: &Source) -> PathBuf {
    let mut identity = Sha256::new();
    for part in [&source.repository, &source.manifest.id] {
        identity.update((part.len() as u64).to_be_bytes());
        identity.update(part.as_bytes());
    }
    let hash: String = identity.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    root.join("drafts").join(format!("{hash}.json"))
}

/// The saved draft, or none when there is no readable one.
pub fn load(root: &Path, source: &Source) -> Values {
    std::fs::read(path(root, source))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(root: &Path, source: &Source, values: &Values) -> Result<(), String> {
    let path = path(root, source);
    let parent = path.parent().ok_or("Invalid draft path")?;
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&serde_json::to_vec(values)?)?;
        temporary.persist(&path)?;
        Ok(())
    };
    write().map_err(|_| "Cannot save the deployment draft".to_owned())
}

pub fn remove(root: &Path, source: &Source) {
    let _ = std::fs::remove_file(path(root, source));
}
