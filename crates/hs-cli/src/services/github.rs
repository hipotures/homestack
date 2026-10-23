//! Read a repository's service manifests through `gh` without cloning it:
//! the default branch's commit, the `.homestack` tree and the `*.yaml`
//! blobs in it, each bounded by [`MAX_BYTES`]. Read only.

use base64::Engine;
use serde_json::Value;

use super::manifest::{Checked, MAX_BYTES, Manifest, parse};
use crate::system::System;

/// The most manifest files read from one repository.
const MAX_FILES: usize = 64;

/// What one repository's default branch declares.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub repository: String,
    pub branch: String,
    /// The commit the manifests were read at.
    pub commit: String,
    /// Why `.homestack` was not read, when it exists but is unusable.
    pub problem: Option<String>,
    pub files: Vec<ManifestFile>,
}

/// One `.homestack/*.yaml` file: what was read of it, or why it is refused.
#[derive(Debug, Clone)]
pub struct ManifestFile {
    pub path: String,
    /// The Git blob: the manifest's revision.
    pub blob: String,
    pub manifest: Result<Checked, String>,
    /// The published releases of its image, newest first; `None` until
    /// they are read.
    pub releases: Option<Result<Vec<hs_core::registry::Published>, String>>,
}

impl ManifestFile {
    /// The manifest when the file has no issues.
    #[must_use]
    pub fn valid(&self) -> Option<&Manifest> {
        self.manifest.as_ref().ok()?.manifest.as_ref()
    }
}

impl Discovery {
    /// The valid manifests.
    pub fn services(&self) -> impl Iterator<Item = (&ManifestFile, &Manifest)> {
        self.files
            .iter()
            .filter_map(|file| file.valid().map(|manifest| (file, manifest)))
    }
}

/// A path segment with everything but unreserved characters escaped.
fn segment(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `gh api PATH`; `None` when GitHub answers 404.
fn get(system: &dyn System, path: &str) -> Result<Option<Value>, String> {
    if system.which("gh").is_none() {
        return Err("Required local command 'gh' was not found".to_owned());
    }
    let argv: Vec<String> = ["gh", "api", "--hostname", "github.com", path]
        .map(str::to_owned)
        .to_vec();
    let output = system
        .run(&argv)
        .map_err(|error| format!("Could not run gh: {error}"))?;
    if output.code != 0 {
        if output.stderr.contains("(HTTP 404)") {
            return Ok(None);
        }
        let reason = output.stderr.lines().next().unwrap_or("no details").trim().to_owned();
        return Err(format!("GitHub request failed: {reason}"));
    }
    serde_json::from_str(&output.stdout)
        .map(Some)
        .map_err(|_| "GitHub API returned invalid JSON".to_owned())
}

fn text<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub API returned an unexpected response".to_owned())
}

/// The manifests of `repository` at the head of `branch` (its default
/// branch; read from GitHub when the catalog did not say).
pub fn discover(system: &dyn System, repository: &str, branch: Option<&str>) -> Result<Discovery, String> {
    let base = format!("repos/{repository}");
    let branch = match branch.filter(|branch| !branch.is_empty()) {
        Some(branch) => branch.to_owned(),
        None => {
            let repo = get(system, &base)?.ok_or_else(|| format!("{repository} was not found"))?;
            text(&repo, "/default_branch")?.to_owned()
        }
    };
    let head = get(system, &format!("{base}/branches/{}", segment(&branch)))?
        .ok_or_else(|| format!("Branch {branch} of {repository} was not found"))?;
    let commit = text(&head, "/commit/sha")?.to_owned();
    let root = get(
        system,
        &format!("{base}/git/trees/{}", text(&head, "/commit/commit/tree/sha")?),
    )?
    .ok_or_else(|| "The commit's tree was not found".to_owned())?;
    let mut discovery = Discovery {
        repository: repository.to_owned(),
        branch,
        commit,
        problem: None,
        files: Vec::new(),
    };
    let entries = |tree: &Value| tree["tree"].as_array().cloned().unwrap_or_default();
    let Some(directory) = entries(&root).into_iter().find(|entry| entry["path"] == ".homestack") else {
        return Ok(discovery);
    };
    match (directory["mode"].as_str(), directory["type"].as_str()) {
        (Some("040000"), Some("tree")) => {}
        (Some("120000"), _) => {
            discovery.problem = Some(".homestack is a symlink; symlinks are refused".to_owned());
            return Ok(discovery);
        }
        _ => {
            discovery.problem = Some(".homestack is not a directory".to_owned());
            return Ok(discovery);
        }
    }
    let tree = get(system, &format!("{base}/git/trees/{}", text(&directory, "/sha")?))?
        .ok_or_else(|| "The .homestack tree was not found".to_owned())?;
    let mut candidates: Vec<Value> = entries(&tree)
        .into_iter()
        .filter(|entry| entry["path"].as_str().is_some_and(|name| name.ends_with(".yaml")))
        .collect();
    candidates.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    if candidates.len() > MAX_FILES {
        discovery.problem = Some(format!(
            "{} manifest files; only the first {MAX_FILES} were read",
            candidates.len()
        ));
        candidates.truncate(MAX_FILES);
    }
    for entry in candidates {
        let path = format!(".homestack/{}", entry["path"].as_str().unwrap_or_default());
        let blob = entry["sha"].as_str().unwrap_or_default().to_owned();
        let manifest = match (entry["mode"].as_str(), entry["type"].as_str()) {
            (Some("120000"), _) => Err("symlinks are refused".to_owned()),
            (Some("100644" | "100755"), Some("blob")) => match entry["size"].as_u64() {
                Some(size) if size > MAX_BYTES => Err(format!("{size} bytes; the limit is {MAX_BYTES}")),
                Some(_) => read_blob(system, &base, &blob).and_then(|text| parse(&text)),
                None => Err("GitHub did not report the file size".to_owned()),
            },
            _ => Err("not a regular file".to_owned()),
        };
        discovery.files.push(ManifestFile {
            path,
            blob,
            manifest,
            releases: None,
        });
    }
    let ids: Vec<String> = discovery
        .files
        .iter()
        .filter_map(|file| file.manifest.as_ref().ok().and_then(|checked| checked.id.clone()))
        .collect();
    for file in &mut discovery.files {
        if let Ok(checked) = &file.manifest
            && let Some(id) = &checked.id
            && ids.iter().filter(|known| *known == id).count() > 1
        {
            file.manifest = Err(format!("id {id} is declared by more than one file"));
        }
    }
    Ok(discovery)
}

fn read_blob(system: &dyn System, base: &str, sha: &str) -> Result<String, String> {
    let blob = get(system, &format!("{base}/git/blobs/{sha}"))?.ok_or_else(|| "The file was not found".to_owned())?;
    if blob["encoding"] != "base64" || blob["size"].as_u64().is_none_or(|size| size > MAX_BYTES) {
        return Err("GitHub returned an unexpected file".to_owned());
    }
    let packed: String = text(&blob, "/content")?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(packed)
        .map_err(|_| "GitHub returned invalid file content".to_owned())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "not UTF-8 text".to_owned())
}
