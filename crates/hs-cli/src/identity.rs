//! The local side of `addkey` (Python `addkey.private_key_for`,
//! `register_identity`, `sync_local_ssh`, I1): the new key in
//! `identity_files` of `client.toml` and in the VM's SSH config.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use hs_config::client::WorkspaceSsh;
use serde_json::{Value, json};

use crate::{AppError, expand_home, local_ssh};

/// The private key next to a `.pub` file, and how configs write it (`~/`
/// form below `home`).
pub fn private_key_for(home: &Path, public: &Path) -> Result<(PathBuf, String), AppError> {
    let shown_public = public.display().to_string();
    let public = expand_home(&shown_public, home);
    let private = if public.extension().is_some_and(|ext| ext == "pub") {
        public.with_extension("")
    } else {
        public.clone()
    };
    if private == public || !private.is_file() {
        return Err(AppError::new(format!(
            "The private key file {} next to {shown_public} was not found",
            private.display()
        )));
    }
    let resolved = private.canonicalize().unwrap_or_else(|_| private.clone());
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let shown = match resolved.strip_prefix(&home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => private.display().to_string(),
    };
    Ok((private, shown))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

/// The `[workspace_ssh]` header line: `[workspace_ssh]` with optional
/// trailing blanks.
fn is_section_header(line: &str, name: &str) -> bool {
    line.strip_prefix(name)
        .is_some_and(|rest| rest.chars().all(|c| c == ' ' || c == '\t'))
}

/// The one-line `identity_files = [...]` of `[workspace_ssh]`: its byte
/// range and the array's body.
fn identity_line(text: &str) -> Option<(usize, usize, String)> {
    let mut offset = 0;
    let mut in_section = false;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let content = line.trim_end_matches('\n');
        if in_section && content.starts_with('[') {
            return None;
        }
        if !in_section {
            in_section = is_section_header(content, "[workspace_ssh]");
            continue;
        }
        let Some(rest) = content.strip_prefix("identity_files") else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let body = rest.strip_prefix('[')?;
        let close = body.find(']')?;
        if !body[close + 1..].chars().all(|c| c == ' ' || c == '\t') {
            return None;
        }
        return Some((start, start + content.len(), body[..close].to_owned()));
    }
    None
}

/// Append `identity` to the one-line `identity_files` array of
/// `[workspace_ssh]` in `config`, keeping the rest of the file and its
/// mode; `false` when it is already there.
pub fn register_identity(home: &Path, config: &Path, identity: &str) -> Result<bool, AppError> {
    let failed = |error: std::io::Error| AppError::new(format!("{}: {error}", config.display()));
    let text = std::fs::read_to_string(config).map_err(failed)?;
    let refused =
        || AppError::new("identity_files in [workspace_ssh] is not a one-line array; add the key to it by hand");
    let (start, end, body) = identity_line(&text).ok_or_else(refused)?;
    let existing: Vec<String> = toml::from_str::<toml::Table>(&format!("x = [{body}]"))
        .ok()
        .and_then(|table| table.get("x").and_then(toml::Value::as_array).cloned())
        .ok_or_else(refused)?
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(refused))
        .collect::<Result<_, _>>()?;
    let wanted = expand_home(identity, home);
    if existing.iter().any(|item| expand_home(item, home) == wanted) {
        return Ok(false);
    }
    let mut items: Vec<String> = existing.iter().map(|item| toml_string(item)).collect();
    items.push(toml_string(identity));
    let new_text = format!(
        "{}identity_files = [{}]{}",
        &text[..start],
        items.join(", "),
        &text[end..]
    );
    let mode = std::fs::metadata(config).map_err(failed)?.permissions().mode() & 0o777;
    let directory = config.parent().unwrap_or(Path::new("."));
    let name = config
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(directory)
        .map_err(failed)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(failed)?;
    temporary.write_all(new_text.as_bytes()).map_err(failed)?;
    temporary.as_file().sync_all().map_err(failed)?;
    temporary.persist(config).map_err(|error| failed(error.error))?;
    Ok(true)
}

/// Make the new key usable locally: `identity_files` of `config`, and this
/// VM's SSH config written with it.
pub fn sync_local_ssh(
    home: &Path,
    config: &Path,
    ssh: &WorkspaceSsh,
    private_keys: &[String],
    vmid: u32,
    name: &str,
    ip: &str,
) -> Result<Value, AppError> {
    let mut changed = false;
    let mut updated = ssh.clone();
    for private_key in private_keys {
        changed |= register_identity(home, config, private_key)?;
        let wanted = expand_home(private_key, home);
        if !updated
            .identity_files
            .iter()
            .any(|item| expand_home(item, home) == wanted)
        {
            updated.identity_files.push(private_key.to_owned());
        }
    }
    let path = local_ssh::write_local_ssh_config(home, &updated, vmid, name, ip)?;
    Ok(json!({"identity_files_changed": changed, "ssh_config": path.display().to_string()}))
}

/// The names of the public keys in `~/.ssh` whose fingerprint is the
/// `SHA256:…` in `text` (ssh's "Confirm user presence for key …"): each
/// key's comment, else its file name; empty when none is known.
#[must_use]
pub fn key_names(home: &Path, text: &str) -> Vec<String> {
    use base64::Engine;
    use sha2::Digest;
    let Some(wanted) = text
        .split_whitespace()
        .find(|word| word.starts_with("SHA256:"))
        .map(|word| word.trim_end_matches(['.', ',', ')']))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(home.join(".ssh")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pub"))
        .collect();
    files.sort();
    let mut names: Vec<String> = Vec::new();
    for file in files {
        let Ok(line) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut words = line.split_whitespace();
        let (Some(_), Some(blob)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(blob) else {
            continue;
        };
        let fingerprint = format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(&bytes))
        );
        if fingerprint != wanted {
            continue;
        }
        let comment = words.collect::<Vec<_>>().join(" ");
        let name = if comment.is_empty() {
            file.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            comment
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Public keys in the desktop SSH directory, named by their comments.
pub fn public_keys(home: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(home.join(".ssh")) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".pub"))
        .map(|name| format!("~/.ssh/{name}"))
        .collect();
    keys.sort();
    keys.into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(crate::expand_home(&path, home)).unwrap_or_default();
            let name = text.split_whitespace().skip(2).collect::<Vec<_>>().join(" ");
            (
                path,
                if name.is_empty() {
                    "Unnamed key".to_owned()
                } else {
                    name
                },
            )
        })
        .collect()
}
