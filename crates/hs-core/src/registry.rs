//! The images a registry publishes and when each was made: the tags of a
//! repository and the `created` time of each image's configuration, read
//! through the registry's API (`/v2/`), anonymously or with a challenge-driven
//! anonymous pull token. All endpoints and redirects require HTTPS. A token
//! realm may have a different origin; bearer tokens go only to the registry.
//! Tag continuations must keep the exact tag-list endpoint and origin.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use reqwest::{Response, Url, header};

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

const MAX_PAGES: usize = 100;
const MAX_TAGS: usize = 100_000;

/// Parse the quoted auth parameters without treating commas inside quotes as
/// separators (RFC 6750 / Registry v2 bearer challenge).
fn bearer_parameters(challenge: &str) -> Result<BTreeMap<String, String>, String> {
    let invalid = || "Registry returned an invalid bearer challenge".to_owned();
    let (scheme, mut rest) = challenge.split_once(' ').ok_or_else(invalid)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(invalid());
    }
    let mut parameters = BTreeMap::new();
    loop {
        rest = rest.trim_start();
        let (name, value) = rest.split_once('=').ok_or_else(invalid)?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(invalid());
        }
        rest = value.trim_start().strip_prefix('"').ok_or_else(invalid)?;
        let mut value = String::new();
        let mut chars = rest.char_indices();
        let end = loop {
            match chars.next() {
                Some((offset, '"')) => break offset + 1,
                Some((_, '\\')) => value.push(chars.next().ok_or_else(invalid)?.1),
                Some((_, ch)) if !ch.is_control() => value.push(ch),
                _ => return Err(invalid()),
            }
        };
        if parameters.insert(name, value).is_some() {
            return Err(invalid());
        }
        rest = rest[end..].trim_start();
        if rest.is_empty() {
            return Ok(parameters);
        }
        rest = rest.strip_prefix(',').ok_or_else(invalid)?;
    }
}

fn secure_url(url: &Url) -> bool {
    url.scheme() == "https" && url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
}

/// The registry of one repository. Tokens are ephemeral and never sent to
/// token services, redirected blob stores, or any unrelated origin.
struct Registry {
    http: reqwest::Client,
    base: Url,
    scope: String,
    token: Mutex<Option<String>>,
}

/// Distribution tag pagination uses Link, never the text of a returned tag.
fn next_page(headers: &header::HeaderMap, current: &Url, endpoint: &Url) -> Result<Option<Url>, String> {
    let mut next = None;
    for value in headers.get_all(header::LINK) {
        let value = value.to_str().map_err(|_| "Invalid registry pagination Link")?;
        // Commas in the URI are legal; split only after the closing angle bracket.
        let mut rest = value.trim();
        while !rest.is_empty() {
            let link = rest.strip_prefix('<').ok_or("Invalid registry pagination Link")?;
            let (target, parameters) = link.split_once('>').ok_or("Invalid registry pagination Link")?;
            let (parameters, remaining) = parameters.split_once(',').unwrap_or((parameters, ""));
            let is_next = parameters.split(';').skip(1).any(|parameter| {
                parameter.trim().split_once('=').is_some_and(|(key, value)| {
                    key.trim().eq_ignore_ascii_case("rel")
                        && value
                            .trim()
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|rel| rel == "next")
                })
            });
            if is_next {
                let url = current
                    .join(target)
                    .map_err(|_| "Invalid registry pagination continuation")?;
                if !secure_url(&url) || url.origin() != endpoint.origin() || url.path() != endpoint.path() {
                    return Err("Registry pagination continuation left its repository endpoint".to_owned());
                }
                if next.replace(url).is_some() {
                    return Err("Registry returned multiple next continuations".to_owned());
                }
            }
            rest = remaining.trim();
        }
    }
    Ok(next)
}

impl Registry {
    async fn open(host: &str, path: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Cannot construct registry client")?;
        let mut base = Url::parse(&format!("https://{host}")).map_err(|_| "Invalid registry host")?;
        if !secure_url(&base) || base.path() != "/" || base.query().is_some() {
            return Err("Invalid registry host".to_owned());
        }
        base.path_segments_mut()
            .map_err(|()| "Invalid registry URL")?
            .extend(["v2"])
            .extend(path.split('/'));
        Ok(Self {
            http,
            base,
            scope: format!("repository:{path}:pull"),
            token: Mutex::new(None),
        })
    }

    fn resource(&self, kind: &str, reference: &str) -> Result<Url, String> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| "Invalid registry URL")?
            .extend([kind, reference]);
        Ok(url)
    }

    async fn send(&self, url: Url, accept: Option<&str>, token: Option<&str>) -> Result<Response, String> {
        // Bound the whole redirect chain with the existing per-request timeout.
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut url = url;
            for _ in 0..=5 {
                if !secure_url(&url) {
                    return Err("Registry endpoint or redirect must use HTTPS without URL credentials".to_owned());
                }
                let mut request = self.http.get(url.clone());
                if url.origin() == self.base.origin()
                    && let Some(token) = token
                {
                    request = request.bearer_auth(token);
                }
                if let Some(accept) = accept {
                    request = request.header(header::ACCEPT, accept);
                }
                let response = request.send().await.map_err(|_| "Registry request failed")?;
                if !response.status().is_redirection() {
                    return Ok(response);
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or("Registry redirect has no valid location")?;
                url = url.join(location).map_err(|_| "Invalid registry redirect")?;
            }
            Err("Registry redirect limit exceeded".to_owned())
        })
        .await
        .map_err(|_| "Registry request timed out")?
    }

    async fn response(&self, url: Url, accept: Option<&str>) -> Result<Response, String> {
        let token = self.token.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut response = self.send(url.clone(), accept, token.as_deref()).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if response.url().origin() != self.base.origin() {
                return Err("Redirected resource refused anonymous access".to_owned());
            }
            let challenge = response
                .headers()
                .get_all(header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .find(|value| {
                    value
                        .split_whitespace()
                        .next()
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer"))
                })
                .ok_or("Registry returned no bearer challenge")?;
            let parameters = bearer_parameters(challenge)?;
            let mut realm = parameters
                .get("realm")
                .and_then(|value| Url::parse(value).ok())
                .filter(secure_url)
                .ok_or("Registry token realm must use HTTPS without URL credentials")?;
            if parameters.get("scope").is_some_and(|scope| scope != &self.scope) {
                return Err("Registry bearer challenge requested an unexpected scope".to_owned());
            }
            {
                let mut query = realm.query_pairs_mut();
                if let Some(service) = parameters.get("service") {
                    query.append_pair("service", service);
                }
                query.append_pair("scope", &self.scope);
            }
            let token_response = self.send(realm, None, None).await?;
            if !token_response.status().is_success() {
                return Err(format!("Registry token endpoint: {}", token_response.status()));
            }
            let body: Value = token_response
                .json()
                .await
                .map_err(|_| "Invalid registry token response")?;
            let token = ["token", "access_token"]
                .into_iter()
                .find_map(|key| {
                    body[key]
                        .as_str()
                        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic()))
                })
                .ok_or("Registry token response contains no usable token")?
                .to_owned();
            response = self.send(url, accept, Some(&token)).await?;
            if response.status().is_success() {
                *self.token.lock().unwrap_or_else(PoisonError::into_inner) = Some(token);
            }
        }
        if !response.status().is_success() {
            return Err(format!("Registry resource: {}", response.status()));
        }
        Ok(response)
    }

    async fn get(&self, tail: &str, accept: Option<&str>) -> Result<Value, String> {
        self.get_digest(tail, accept).await.map(|(body, _)| body)
    }

    /// The body and the registry's `Docker-Content-Digest`.
    async fn get_digest(&self, tail: &str, accept: Option<&str>) -> Result<(Value, Option<String>), String> {
        let (kind, reference) = tail.split_once('/').ok_or("Invalid registry resource")?;
        let response = self.response(self.resource(kind, reference)?, accept).await?;
        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.json().await.map_err(|_| "Invalid registry resource JSON")?;
        Ok((body, digest))
    }

    async fn tags(&self) -> Result<Vec<String>, String> {
        let endpoint = self.resource("tags", "list")?;
        let mut url = endpoint.clone();
        url.query_pairs_mut().append_pair("n", "1000");
        let mut visited = HashSet::new();
        let mut tags = Vec::new();
        for _ in 0..MAX_PAGES {
            // Query ordering must not hide a repeated continuation.
            let mut query: Vec<_> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            query.sort();
            if !visited.insert(query) {
                return Err("Registry tag pagination repeated a continuation".to_owned());
            }
            let response = self.response(url.clone(), None).await?;
            if response.url().origin() != endpoint.origin() || response.url().path() != endpoint.path() {
                return Err("Registry tag list redirected outside its repository endpoint".to_owned());
            }
            let next = next_page(response.headers(), &url, &endpoint)?;
            let body: Value = response.json().await.map_err(|_| "Invalid registry tag list JSON")?;
            if !body["tags"].is_null() {
                let page = body["tags"].as_array().ok_or("Registry returned no tag list")?;
                if tags.len().saturating_add(page.len()) > MAX_TAGS {
                    return Err(format!("Registry tag pagination exceeded {MAX_TAGS} tags"));
                }
                for tag in page {
                    tags.push(tag.as_str().ok_or("Registry returned a non-string tag")?.to_owned());
                }
            } else if body.get("tags").is_none() {
                return Err("Registry returned no tag list".to_owned());
            }
            let Some(next) = next else { return Ok(tags) };
            url = next;
        }
        Err(format!("Registry tag pagination exceeded {MAX_PAGES} pages"))
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
    let mut tags = registry.tags().await?;
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
