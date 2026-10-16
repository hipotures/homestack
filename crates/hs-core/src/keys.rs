//! The workspace's public keys (mapping C5): `workspace_ssh.get_workspace_authorized_keys`,
//! `validate_authorized_keys` and `parse_authorized_key_records`.
//!
//! The keys come from Gold's `sshkeys`, or, when those are unusable and Gold
//! runs, from Gold's `authorized_keys` files. Python reads the files with
//! `test -s F && cat F` through `exec`; the port uses `file-read`, which
//! needs only `VM.GuestAgent.FileRead` (decision 5).

use crate::error::CoreError;
use crate::guest::{qga_ping, read_text};
use crate::inventory::{cluster_vm_resources, vm_config, vm_status};
use crate::pve::Pve;
use hs_model::config::Config;

/// `urllib.parse.unquote`
#[must_use]
pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |i: usize| bytes.get(i).and_then(|b| (*b as char).to_digit(16));
        match (bytes[index], hex(index + 1), hex(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or(b'?'));
                index += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first 80 characters, quoted as Python's `repr` of a plain string.
fn shown(line: &str) -> String {
    format!("'{}'", line.chars().take(80).collect::<String>())
}

/// `validate_authorized_keys`: the key lines, stripped, one per line with a
/// final newline. Blank lines and `#` comments are skipped; any other line
/// must start with `ssh-`, `ecdsa-` or `sk-`.
pub fn validate_authorized_keys(text: &str) -> Result<String, CoreError> {
    let mut keys = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !["ssh-", "ecdsa-", "sk-"].iter().any(|prefix| line.starts_with(prefix)) {
            return Err(CoreError::Invalid(format!(
                "Unexpected line in authorized_keys source: {}",
                shown(line)
            )));
        }
        keys.push(line);
    }
    if keys.is_empty() {
        return Err(CoreError::Invalid(
            "No SSH public keys found for workspace user".to_owned(),
        ));
    }
    Ok(keys.join("\n") + "\n")
}

/// One key of `parse_authorized_key_records`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// Python's `type`.
    pub key_type: String,
    pub blob: String,
    pub comment: String,
    /// The comment, else the type and the first 12 characters of the blob.
    pub label: String,
    /// `type blob`, which identifies the key whatever its comment.
    pub identity: String,
}

/// The first whitespace-separated field and what follows it (one step of
/// Python's `str.split(None, 2)`).
fn split_field(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    text.split_at(text.find(char::is_whitespace).unwrap_or(text.len()))
}

/// `parse_authorized_key_records`
pub fn parse_authorized_key_records(text: &str) -> Result<Vec<KeyRecord>, CoreError> {
    validate_authorized_keys(text)?
        .lines()
        .map(|line| {
            let (key_type, rest) = split_field(line);
            let (blob, rest) = split_field(rest);
            if blob.is_empty() {
                return Err(CoreError::Invalid(format!(
                    "Invalid authorized_keys line: {}",
                    shown(line)
                )));
            }
            let comment = rest.trim().to_owned();
            let label = if comment.is_empty() {
                format!("{key_type} {}…", blob.chars().take(12).collect::<String>())
            } else {
                comment.clone()
            };
            Ok(KeyRecord {
                key_type: key_type.to_owned(),
                blob: blob.to_owned(),
                comment,
                label,
                identity: format!("{key_type} {blob}"),
            })
        })
        .collect()
}

/// The keys for a new workspace and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeys {
    /// Validated, one key per line.
    pub keys: String,
    /// `Gold VM N:qm sshkeys` or `Gold VM N:<path>`.
    pub source: String,
}

/// `get_workspace_authorized_keys`: every request goes to Gold's node.
pub async fn workspace_authorized_keys(pve: &impl Pve, cfg: &Config) -> Result<WorkspaceKeys, CoreError> {
    let gold = crate::gold::configured_gold(cfg)?;
    let resource = cluster_vm_resources(pve)
        .await?
        .into_iter()
        .find(|resource| resource.vmid == gold)
        .ok_or_else(|| CoreError::Failed(format!("Gold VM {gold} does not exist")))?;
    let node = resource.node.trim();
    if node.is_empty() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold} has no node in cluster inventory"
        )));
    }
    let gold_cfg = vm_config(pve, node, gold).await?;
    if let Some(configured) = gold_cfg.get("sshkeys").filter(|keys| !keys.is_empty())
        && let Ok(keys) = validate_authorized_keys(&unquote(configured))
    {
        return Ok(WorkspaceKeys {
            keys,
            source: format!("Gold VM {gold}:qm sshkeys"),
        });
    }

    if vm_status(pve, node, gold).await != "running" {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold} is stopped and its Proxmox sshkeys setting does not contain usable public keys."
        )));
    }
    if qga_ping(pve, node, gold).await.is_err() {
        return Err(CoreError::Failed(format!(
            "Gold VM {gold} QEMU Guest Agent is not available"
        )));
    }

    let user = &cfg.user.name;
    for candidate in [
        format!("/home/{user}/.ssh/authorized_keys"),
        "/root/.ssh/authorized_keys".to_owned(),
    ] {
        let Some(text) = read_text(pve, node, gold, &candidate).await? else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if let Ok(keys) = validate_authorized_keys(&text) {
            return Ok(WorkspaceKeys {
                keys,
                source: format!("Gold VM {gold}:{candidate}"),
            });
        }
    }
    Err(CoreError::Failed(format!(
        "Gold VM {gold} has no usable public keys in its Proxmox sshkeys setting, \
         /home/{user}/.ssh/authorized_keys, or /root/.ssh/authorized_keys"
    )))
}
