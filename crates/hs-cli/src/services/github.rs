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

/// A newer HomeStack image release that has a Git tag but is not yet in
/// the registry. Publication status is advisory; only registry images can
/// be deployed. This reads the latest attempt, including workflow reruns.
pub fn image_publication(
    system: &dyn System,
    repository: &str,
    service: &str,
    published: Option<&str>,
) -> Result<Option<String>, String> {
    let prefix = match service {
        "homestack-api" => "",
        "pangolin" => "pangolin-",
        "otel" => "otel-",
        "ai-usage" => "ai-usage-",
        "metrics" => "metrics-",
        _ => return Ok(None),
    };
    let base = format!("repos/{repository}");
    let refs = get(system, &format!("{base}/git/matching-refs/tags/{prefix}"))?.ok_or("Release tags were not found")?;
    let refs = refs.as_array().ok_or("GitHub returned invalid release tags")?;
    let newest = refs
        .iter()
        .filter_map(|entry| {
            let tag = entry["ref"].as_str()?.strip_prefix("refs/tags/")?;
            let number = tag.strip_prefix(prefix)?;
            // Release scripts publish exactly X.Y.Z; ignore development tags.
            if number.contains('-') || number.split('.').count() != 3 {
                return None;
            }
            Some((hs_core::registry::version(number)?, number, tag))
        })
        .max_by_key(|(number, _, _)| *number);
    let Some((number, release, tag)) = newest else {
        return Ok(None);
    };
    if published
        .and_then(hs_core::registry::version)
        .is_some_and(|current| number <= current)
    {
        return Ok(None);
    }
    let runs = get(
        system,
        &format!(
            "{base}/actions/workflows/ci.yml/runs?event=push&branch={}&per_page=1",
            segment(tag)
        ),
    )?
    .ok_or("Image publication workflow was not found")?;
    let runs = runs["workflow_runs"]
        .as_array()
        .ok_or("GitHub returned invalid workflow runs")?;
    let Some(run) = runs.first() else {
        return Ok(Some(format!(
            "release {release}: publication workflow has not started; https://github.com/{repository}/actions"
        )));
    };
    let id = run["id"].as_u64().ok_or("GitHub returned an invalid workflow run ID")?;
    let jobs = get(
        system,
        &format!("{base}/actions/runs/{id}/jobs?filter=latest&per_page=100"),
    )?
    .ok_or("Image publication jobs were not found")?;
    Ok(Some(publication_status(release, run, &jobs)?))
}

fn publication_status(release: &str, run: &Value, jobs: &Value) -> Result<String, String> {
    let status = text(run, "/status")?;
    let state = if status == "completed" {
        match text(run, "/conclusion")? {
            "success" => "workflow succeeded; image not yet available in registry",
            "failure" => "publication failed",
            "cancelled" => "publication cancelled",
            "timed_out" => "publication timed out",
            "action_required" => "publication requires action",
            other => other,
        }
    } else {
        match status {
            "in_progress" => "publication in progress",
            "queued" => "publication queued",
            "waiting" | "pending" | "requested" => "publication waiting",
            other => other,
        }
    };
    let jobs = jobs["jobs"]
        .as_array()
        .ok_or("GitHub returned invalid publication jobs")?;
    let job = jobs.iter().find(|job| {
        if status == "completed" {
            matches!(job["conclusion"].as_str(), Some("failure" | "timed_out" | "cancelled"))
        } else {
            job["status"] == "in_progress"
        }
    });
    let mut detail = String::new();
    if let Some(job) = job {
        detail.push_str(text(job, "/name")?);
        if let Some(step) = job["steps"].as_array().and_then(|steps| {
            steps.iter().find(|step| {
                step["status"] == "in_progress"
                    || matches!(step["conclusion"].as_str(), Some("failure" | "timed_out" | "cancelled"))
            })
        }) {
            detail.push_str(" / ");
            detail.push_str(text(step, "/name")?);
        }
    }
    let detail = if detail.is_empty() {
        detail
    } else {
        format!(" ({detail})")
    };
    Ok(format!(
        "release {release}: {state}{detail}; {}",
        text(run, "/html_url")?
    ))
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
    if let Some(services) = entries(&tree).into_iter().find(|entry| entry["path"] == "services") {
        let groups = match read_tree(system, &base, &services, ".homestack/services") {
            Ok(groups) => groups,
            Err(error) => {
                discovery.problem = Some(error);
                Vec::new()
            }
        };
        if discovery.files.len() + groups.len() > MAX_FILES {
            return Err(format!("More than {MAX_FILES} service definitions"));
        }
        for group in groups {
            let id = group["path"].as_str().unwrap_or_default();
            let directory = format!(".homestack/services/{id}");
            let manifest = (|| {
                let members = read_tree(system, &base, &group, &directory)?;
                if members.iter().filter_map(|m| m["path"].as_str()).any(|name| {
                    name.ends_with(".yaml") && !["service.yaml", "defaults.yaml", "inputs.yaml"].contains(&name)
                }) {
                    return Err(format!("{directory}: unexpected YAML member"));
                }
                let mut texts = Vec::new();
                for name in ["service.yaml", "defaults.yaml", "inputs.yaml"] {
                    let member = members
                        .iter()
                        .find(|entry| entry["path"] == name)
                        .ok_or_else(|| format!("{directory}/{name}: missing member"))?;
                    texts.push(read_regular(system, &base, member).map_err(|e| format!("{directory}/{name}: {e}"))?);
                }
                super::schema3::parse(&texts[0], &texts[1], &texts[2], id)
            })();
            discovery.files.push(ManifestFile {
                path: format!("{directory}/service.yaml"),
                // The immutable directory tree binds all three component blobs.
                blob: group["sha"].as_str().unwrap_or_default().to_owned(),
                manifest,
                releases: None,
            });
        }
    }
    let ids: Vec<String> = discovery
        .files
        .iter()
        .filter_map(|file| {
            file.manifest
                .as_ref()
                .ok()
                .and_then(|checked| checked.id.clone())
                .or_else(|| {
                    file.path
                        .strip_prefix(".homestack/services/")
                        .and_then(|p| p.strip_suffix("/service.yaml"))
                        .map(str::to_owned)
                })
        })
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

fn read_tree(system: &dyn System, base: &str, entry: &Value, path: &str) -> Result<Vec<Value>, String> {
    if entry["mode"] != "040000" || entry["type"] != "tree" {
        return Err(format!("{path}: expected a real directory; symlinks are refused"));
    }
    let tree = get(system, &format!("{base}/git/trees/{}", text(entry, "/sha")?))?
        .ok_or_else(|| format!("{path}: tree unavailable"))?;
    if tree["truncated"] == true {
        return Err(format!("{path}: truncated tree"));
    }
    tree["tree"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("{path}: invalid tree"))
}

fn read_regular(system: &dyn System, base: &str, entry: &Value) -> Result<String, String> {
    if !matches!(entry["mode"].as_str(), Some("100644" | "100755")) || entry["type"] != "blob" {
        return Err("not a regular file; symlinks are refused".into());
    }
    if entry["size"].as_u64().is_none_or(|size| size > MAX_BYTES) {
        return Err(format!("missing size or larger than {MAX_BYTES} bytes"));
    }
    read_blob(system, base, text(entry, "/sha")?)
}
