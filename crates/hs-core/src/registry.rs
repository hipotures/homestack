//! The images a registry publishes and when each was made: the tags of a
//! repository and the `created` time of each image's configuration, read
//! through the registry's API (`/v2/`) with an anonymous pull token, as
//! ghcr.io gives one for a public package.

use std::time::Duration;

use serde_json::Value;

/// An image of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// `REGISTRY/PATH:TAG`.
    pub image: String,
    /// When it was made (RFC 3339, `2026-10-16T01:42:01.53Z`), when the
    /// registry says.
    pub created: Option<String>,
}

/// The manifests asked for: an index (several platforms) or one image.
const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// `REGISTRY/PATH:TAG` as its registry, path and tag.
#[must_use]
pub fn split(image: &str) -> Option<(&str, &str, &str)> {
    let (repository, tag) = image.rsplit_once(':')?;
    let (host, path) = repository.split_once('/')?;
    (!tag.is_empty() && !tag.contains('/') && !path.is_empty()).then_some((host, path, tag))
}

/// The registry of one repository.
struct Registry {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Registry {
    async fn open(host: &str, path: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| error.to_string())?;
        // Without a token a registry may still answer (a local one).
        let token = match http
            .get(format!("https://{host}/token?scope=repository:{path}:pull"))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| body["token"].as_str().map(str::to_owned)),
            _ => None,
        };
        Ok(Self {
            http,
            base: format!("https://{host}/v2/{path}"),
            token,
        })
    }

    async fn get(&self, tail: &str, accept: Option<&str>) -> Result<Value, String> {
        self.get_digest(tail, accept).await.map(|(body, _)| body)
    }

    /// The body and the registry's `Docker-Content-Digest`.
    async fn get_digest(&self, tail: &str, accept: Option<&str>) -> Result<(Value, Option<String>), String> {
        let mut request = self.http.get(format!("{}/{tail}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(accept) = accept {
            request = request.header(reqwest::header::ACCEPT, accept);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{tail}: {status}"));
        }
        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.json().await.map_err(|error| format!("{tail}: {error}"))?;
        Ok((body, digest))
    }

    /// When the image of `tag` was made: its configuration's `created`; an
    /// index gives the linux/amd64 image's.
    async fn created(&self, tag: &str) -> Result<Option<String>, String> {
        let mut manifest = self.get(&format!("manifests/{tag}"), Some(MANIFESTS)).await?;
        if let Some(list) = manifest["manifests"].as_array() {
            let amd64 = list
                .iter()
                .find(|entry| entry["platform"]["os"] == "linux" && entry["platform"]["architecture"] == "amd64");
            let Some(digest) = amd64
                .or_else(|| list.first())
                .and_then(|entry| entry["digest"].as_str())
            else {
                return Ok(None);
            };
            manifest = self.get(&format!("manifests/{digest}"), Some(MANIFESTS)).await?;
        }
        let Some(digest) = manifest["config"]["digest"].as_str() else {
            return Ok(None);
        };
        let config = self.get(&format!("blobs/{digest}"), None).await?;
        Ok(config["created"].as_str().map(str::to_owned))
    }
}

/// A release checked for deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The digest of what the tag names: an index or one image.
    pub digest: String,
    /// The linux/amd64 image's digest (the same for a single image).
    pub platform: String,
    pub created: Option<String>,
}

fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The digest of `image` and of its linux/amd64 image; an error when the
/// tag has no linux/amd64 image (an index's attestations are skipped).
pub async fn resolve(image: &str) -> Result<Resolved, String> {
    let (host, path, tag) = split(image).ok_or_else(|| format!("{image} is not REGISTRY/PATH:TAG"))?;
    let registry = Registry::open(host, path).await?;
    let (manifest, digest) = registry
        .get_digest(&format!("manifests/{tag}"), Some(MANIFESTS))
        .await?;
    let digest = digest
        .filter(|digest| is_digest(digest))
        .ok_or_else(|| format!("{image}: the registry gave no sha256 digest"))?;
    let (platform, image_manifest) = if let Some(list) = manifest["manifests"].as_array() {
        let platform = list
            .iter()
            .find(|entry| entry["platform"]["os"] == "linux" && entry["platform"]["architecture"] == "amd64")
            .and_then(|entry| entry["digest"].as_str())
            .filter(|digest| is_digest(digest))
            .ok_or_else(|| format!("{image} has no linux/amd64 image"))?
            .to_owned();
        let found = registry.get(&format!("manifests/{platform}"), Some(MANIFESTS)).await?;
        (platform, found)
    } else {
        (digest.clone(), manifest)
    };
    let config_digest = image_manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| format!("{image}: the image has no configuration"))?;
    let config = registry.get(&format!("blobs/{config_digest}"), None).await?;
    if config["os"] != "linux" || config["architecture"] != "amd64" {
        return Err(format!("{image} is not a linux/amd64 image"));
    }
    Ok(Resolved {
        digest,
        platform,
        created: config["created"].as_str().map(str::to_owned),
    })
}

/// The image reference recorded by HomeStack's deployer.
#[must_use]
pub fn deployed_image(config: &Value) -> Option<String> {
    let description = config["description"].as_str()?;
    if !description
        .lines()
        .any(|line| line.trim() == "made by homestack-deploy")
    {
        return None;
    }
    description.lines().find_map(|line| {
        let image = line.trim().strip_prefix("image ")?.trim();
        split(image).map(|_| image.to_owned())
    })
}

/// The service HomeStack deployed in a container, from its description.
#[must_use]
pub fn deployed_service(config: &Value) -> Option<hs_model::status::DeployedService> {
    let description = config["description"].as_str()?;
    let value = |key: &str| {
        description
            .lines()
            .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix(' '))
            .map(|value| value.trim().to_owned())
    };
    if !description.lines().any(|line| line.trim() == "made by homestack") {
        return None;
    }
    Some(hs_model::status::DeployedService {
        id: value("HomeStack service")?,
        repository: value("repository")?,
        image: value("image")?,
        digest: value("digest")?,
    })
}

/// HomeStack publication order: `<major>.<minor>.<patch>`, optionally
/// `-<build>` (a packaging of an upstream release, e.g. `0.18.1-2`); a tag
/// without a build sorts as build 0.
#[must_use]
pub fn version(tag: &str) -> Option<(u64, u64, u64, u64)> {
    let number = |part: &str| {
        (!part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && !(part.len() > 1 && part.starts_with('0')))
        .then(|| part.parse().ok())
        .flatten()
    };
    let (release, build) = match tag.split_once('-') {
        Some((release, build)) => (release, number(build).filter(|build| *build > 0)?),
        None => (tag, 0),
    };
    let mut parts = release.split('.');
    let result = (
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
        build,
    );
    parts.next().is_none().then_some(result)
}

/// Whether `offered` is a later HomeStack build in the same repository.
#[must_use]
pub fn newer(offered: &str, current: &str) -> bool {
    let (Some((host, path, tag)), Some((now_host, now_path, now_tag))) = (split(offered), split(current)) else {
        return false;
    };
    match (version(tag), version(now_tag)) {
        (Some(offered), Some(now)) => (host, path) == (now_host, now_path) && offered > now,
        _ => false,
    }
}

/// The newest HomeStack build tags, plus the installed tags in `also`.
/// Tags are selected by numeric version, independently of registry list order.
pub async fn recent(image: &str, count: usize, also: &[String]) -> Result<Vec<Published>, String> {
    let (host, path, _) = split(image).ok_or_else(|| format!("{image} is not REGISTRY/PATH:TAG"))?;
    let registry = std::sync::Arc::new(Registry::open(host, path).await?);
    let mut tags = Vec::new();
    let mut tail = "tags/list?n=1000".to_owned();
    loop {
        let listed = registry.get(&tail, None).await?;
        if listed.get("tags").is_some_and(Value::is_null) {
            break;
        }
        let page = listed["tags"].as_array().ok_or("Registry returned no tag list")?;
        let page: Vec<String> = page.iter().filter_map(|tag| tag.as_str().map(str::to_owned)).collect();
        let Some(last) = page.last() else { break };
        let next = format!("tags/list?n=1000&last={last}");
        if next == tail {
            return Err("Registry tag pagination did not advance".to_owned());
        }
        tags.extend(page);
        tail = next;
    }
    tags.retain(|tag| version(tag).is_some());
    tags.sort_by_key(|tag| std::cmp::Reverse(version(tag)));
    tags.dedup();
    let mut chosen: Vec<String> = tags.into_iter().take(count).collect();
    for other in also {
        if let Some((other_host, other_path, tag)) = split(other)
            && (other_host, other_path) == (host, path)
            && !chosen.iter().any(|known| known == tag)
        {
            chosen.push(tag.to_owned());
        }
    }
    let mut reads = tokio::task::JoinSet::new();
    for (index, tag) in chosen.into_iter().enumerate() {
        let registry = std::sync::Arc::clone(&registry);
        reads.spawn(async move {
            let created = registry.created(&tag).await.ok().flatten();
            (index, tag, created)
        });
    }
    let mut found = Vec::new();
    while let Some(read) = reads.join_next().await {
        found.push(read.map_err(|error| error.to_string())?);
    }
    found.sort_by_key(|(_, tag, _)| std::cmp::Reverse(version(tag)));
    Ok(found
        .into_iter()
        .map(|(_, tag, created)| Published {
            image: format!("{host}/{path}:{tag}"),
            created,
        })
        .collect())
}
