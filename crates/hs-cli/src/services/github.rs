//! Read a repository's service manifests through `gh` without cloning it:
//! one GraphQL request reads the branch's commit and the `.homestack`
//! files, each bounded by [`MAX_BYTES`]. Read only.

use serde_json::Value;
use std::collections::BTreeMap;

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

/// The publication of a release tag that is not yet in the registry.
pub struct Publication {
    pub release: String,
    /// Short state, such as "building" or "failed".
    pub state: &'static str,
    /// The running or failed job and step, when known.
    pub detail: String,
    pub url: String,
    /// Still on its way to the registry; false once the workflow has failed.
    pub building: bool,
}

/// A newer HomeStack image release that has a Git tag but is not yet in
/// the registry. Publication status is advisory; only registry images can
/// be deployed. This reads the latest attempt, including workflow reruns.
pub fn image_publication(
    system: &dyn System,
    repository: &str,
    service: &str,
    published: Option<&str>,
) -> Result<Option<Publication>, String> {
    let prefix = match service {
        "homestack-api" => "",
        "pangolin" => "pangolin-",
        "otel" => "otel-",
        "ai-usage" => "ai-usage-",
        "metrics" => "metrics-",
        "agent-accounts" => "agent-accounts-",
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
        return Ok(Some(Publication {
            release: release.to_owned(),
            state: "workflow not started",
            detail: String::new(),
            url: format!("https://github.com/{repository}/actions"),
            building: true,
        }));
    };
    let id = run["id"].as_u64().ok_or("GitHub returned an invalid workflow run ID")?;
    let jobs = get(
        system,
        &format!("{base}/actions/runs/{id}/jobs?filter=latest&per_page=100"),
    )?
    .ok_or("Image publication jobs were not found")?;
    Ok(Some(publication_status(release, run, &jobs)?))
}

fn publication_status(release: &str, run: &Value, jobs: &Value) -> Result<Publication, String> {
    let status = text(run, "/status")?;
    // A successful workflow's image is about to appear in the registry.
    let building = status != "completed" || text(run, "/conclusion")? == "success";
    let state = if status == "completed" {
        match text(run, "/conclusion")? {
            "success" => "built, not yet in registry",
            "failure" => "failed",
            "cancelled" => "cancelled",
            "timed_out" => "timed out",
            "action_required" => "requires action",
            _ => "ended",
        }
    } else {
        match status {
            "in_progress" => "building",
            "queued" => "queued",
            _ => "waiting",
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
    Ok(Publication {
        release: release.to_owned(),
        state,
        detail,
        url: text(run, "/html_url")?.to_owned(),
        building,
    })
}

/// `gh api graphql` with `fields` as string variables: the answer's `data`.
fn graphql(system: &dyn System, query: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    if system.which("gh").is_none() {
        return Err("Required local command 'gh' was not found".to_owned());
    }
    let mut argv: Vec<String> = ["gh", "api", "graphql", "--hostname", "github.com", "-f"]
        .map(str::to_owned)
        .to_vec();
    argv.push(format!("query={query}"));
    for (name, value) in fields {
        argv.push("-f".to_owned());
        argv.push(format!("{name}={value}"));
    }
    let output = system
        .run(&argv)
        .map_err(|error| format!("Could not run gh: {error}"))?;
    if output.code != 0 {
        let reason = output.stderr.lines().next().unwrap_or("no details").trim().to_owned();
        return Err(format!("GitHub request failed: {reason}"));
    }
    let answer: Value =
        serde_json::from_str(&output.stdout).map_err(|_| "GitHub API returned invalid JSON".to_owned())?;
    if let Some(error) = answer["errors"].as_array().and_then(|errors| errors.first()) {
        let reason = error["message"].as_str().unwrap_or("no details");
        return Err(format!("GitHub request failed: {reason}"));
    }
    Ok(answer["data"].clone())
}

/// Warning titles for the exact image versions offered by the registry.
/// Release metadata never renames a Git tag or changes an image reference.
pub fn image_warnings(
    system: &dyn System,
    repository: &str,
    service: &str,
    images: &[String],
) -> Result<BTreeMap<String, String>, String> {
    if images.is_empty() {
        return Ok(BTreeMap::new());
    }
    let (owner, name) = repository.split_once('/').ok_or("Repository must be OWNER/NAME")?;
    let prefix = if service == "homestack-api" {
        String::new()
    } else {
        format!("{service}-")
    };
    let mut variables = String::new();
    let mut releases = String::new();
    let mut tags = Vec::new();
    for (index, image) in images.iter().enumerate() {
        let (_, _, version) = hs_core::registry::split(image).ok_or("Invalid image reference")?;
        variables.push_str(&format!(",$tag{index}:String!"));
        releases.push_str(&format!("r{index}:release(tagName:$tag{index}){{name isDraft}} "));
        tags.push((format!("tag{index}"), format!("{prefix}{version}")));
    }
    let query =
        format!("query($owner:String!,$name:String!{variables}){{repository(owner:$owner,name:$name){{{releases}}}}}");
    let mut fields = vec![("owner", owner), ("name", name)];
    fields.extend(tags.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    let data = graphql(system, &query, &fields)?;
    if data["repository"].is_null() {
        return Err(format!("{repository} was not found"));
    }
    let mut warnings = BTreeMap::new();
    for (index, image) in images.iter().enumerate() {
        let release = &data["repository"][format!("r{index}")];
        if release["isDraft"] == true {
            continue;
        }
        if let Some(title) = release["name"].as_str()
            && title.to_ascii_uppercase().contains("DO NOT USE")
        {
            warnings.insert(image.clone(), title.to_owned());
        }
    }
    Ok(warnings)
}

/// A tree entry's Git mode as the octal text Git writes: `040000`, `100644`.
fn mode(entry: &Value) -> String {
    entry["mode"]
        .as_u64()
        .map(|mode| format!("{mode:06o}"))
        .unwrap_or_default()
}

/// The entries of a tree entry that GraphQL read with it.
fn children(entry: &Value) -> Vec<Value> {
    entry["object"]["entries"].as_array().cloned().unwrap_or_default()
}

/// The text of a regular file entry, bounded by [`MAX_BYTES`].
fn file_text(entry: &Value) -> Result<String, String> {
    if !matches!(mode(entry).as_str(), "100644" | "100755") || entry["type"] != "blob" {
        return Err("not a regular file; symlinks are refused".into());
    }
    let blob = &entry["object"];
    let size = blob["byteSize"].as_u64().ok_or("GitHub did not report the file size")?;
    if size > MAX_BYTES {
        return Err(format!("{size} bytes; the limit is {MAX_BYTES}"));
    }
    if blob["isBinary"] == true {
        return Err("not UTF-8 text".to_owned());
    }
    match blob["text"].as_str() {
        Some(text) if blob["isTruncated"] != true => Ok(text.to_owned()),
        _ => Err("GitHub returned an unexpected file".to_owned()),
    }
}

/// The manifests of `repository` at the head of `branch` (its default
/// branch; read from GitHub when the catalog did not say). One GraphQL
/// request reads the commit and every `.homestack` file needed.
pub fn discover(system: &dyn System, repository: &str, branch: Option<&str>) -> Result<Discovery, String> {
    let (owner, name) = repository
        .split_once('/')
        .ok_or_else(|| format!("{repository} is not OWNER/NAME"))?;
    let branch = branch.filter(|branch| !branch.is_empty());
    // `.homestack`, its files and `services`, each service and its files.
    let entry = |inner: &str| format!("entries{{name mode type oid object{{...B{inner}}}}}");
    let services = entry(&format!(" ... on Tree{{{}}}", entry("")));
    let homestack = entry(&format!(" ... on Tree{{{services}}}"));
    let target = format!(
        "target{{... on Commit{{oid file(path:\".homestack\"){{mode type object{{... on Tree{{{homestack}}}}}}}}}}}"
    );
    let (head, variables) = match branch {
        Some(_) => (format!("ref(qualifiedName:$ref){{name {target}}}"), ",$ref:String!"),
        None => (format!("defaultBranchRef{{name {target}}}"), ""),
    };
    let query = format!(
        "query($owner:String!,$name:String!{variables}){{repository(owner:$owner,name:$name){{{head}}}}} \
         fragment B on Blob{{byteSize isBinary isTruncated text}}"
    );
    let qualified = branch.map(|branch| format!("refs/heads/{branch}"));
    let mut fields = vec![("owner", owner), ("name", name)];
    if let Some(qualified) = &qualified {
        fields.push(("ref", qualified));
    }
    let data = graphql(system, &query, &fields)?;
    let found = &data["repository"];
    if found.is_null() {
        return Err(format!("{repository} was not found"));
    }
    let head = &found[if branch.is_some() { "ref" } else { "defaultBranchRef" }];
    if head.is_null() {
        return Err(match branch {
            Some(branch) => format!("Branch {branch} of {repository} was not found"),
            None => format!("{repository} has no default branch"),
        });
    }
    let mut discovery = Discovery {
        repository: repository.to_owned(),
        branch: text(head, "/name")?.to_owned(),
        commit: text(head, "/target/oid")?.to_owned(),
        problem: None,
        files: Vec::new(),
    };
    let directory = &head["target"]["file"];
    if directory.is_null() {
        return Ok(discovery);
    }
    match (mode(directory).as_str(), directory["type"].as_str()) {
        ("040000", Some("tree")) => {}
        ("120000", _) => {
            discovery.problem = Some(".homestack is a symlink; symlinks are refused".to_owned());
            return Ok(discovery);
        }
        _ => {
            discovery.problem = Some(".homestack is not a directory".to_owned());
            return Ok(discovery);
        }
    }
    let tree = children(directory);
    let mut candidates: Vec<&Value> = tree
        .iter()
        .filter(|entry| entry["name"].as_str().is_some_and(|name| name.ends_with(".yaml")))
        .collect();
    candidates.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    if candidates.len() > MAX_FILES {
        discovery.problem = Some(format!(
            "{} manifest files; only the first {MAX_FILES} were read",
            candidates.len()
        ));
        candidates.truncate(MAX_FILES);
    }
    for entry in candidates {
        discovery.files.push(ManifestFile {
            path: format!(".homestack/{}", entry["name"].as_str().unwrap_or_default()),
            blob: entry["oid"].as_str().unwrap_or_default().to_owned(),
            manifest: file_text(entry).and_then(|text| parse(&text)),
            releases: None,
        });
    }
    if let Some(services) = tree.iter().find(|entry| entry["name"] == "services") {
        let groups = if mode(services) == "040000" && services["type"] == "tree" {
            children(services)
        } else {
            discovery.problem = Some(".homestack/services: expected a real directory; symlinks are refused".to_owned());
            Vec::new()
        };
        if discovery.files.len() + groups.len() > MAX_FILES {
            return Err(format!("More than {MAX_FILES} service definitions"));
        }
        for group in &groups {
            let id = group["name"].as_str().unwrap_or_default();
            let directory = format!(".homestack/services/{id}");
            let manifest = (|| {
                if mode(group) != "040000" || group["type"] != "tree" {
                    return Err(format!("{directory}: expected a real directory; symlinks are refused"));
                }
                let members = children(group);
                if members.iter().filter_map(|m| m["name"].as_str()).any(|name| {
                    name.ends_with(".yaml") && !["service.yaml", "defaults.yaml", "inputs.yaml"].contains(&name)
                }) {
                    return Err(format!("{directory}: unexpected YAML member"));
                }
                let mut texts = Vec::new();
                for name in ["service.yaml", "defaults.yaml", "inputs.yaml"] {
                    let member = members
                        .iter()
                        .find(|entry| entry["name"] == name)
                        .ok_or_else(|| format!("{directory}/{name}: missing member"))?;
                    texts.push(file_text(member).map_err(|e| format!("{directory}/{name}: {e}"))?);
                }
                super::schema3::parse(&texts[0], &texts[1], &texts[2], id)
            })();
            discovery.files.push(ManifestFile {
                path: format!("{directory}/service.yaml"),
                // The immutable directory tree binds all three component blobs.
                blob: group["oid"].as_str().unwrap_or_default().to_owned(),
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
