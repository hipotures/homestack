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
        response.json().await.map_err(|error| format!("{tail}: {error}"))
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

/// The images of the repository of `image`: the last `count` tags it lists
/// (in the order they were published) and those of `also` in the same
/// repository, newest first; an image whose time cannot be read comes last.
pub async fn recent(image: &str, count: usize, also: &[String]) -> Result<Vec<Published>, String> {
    let (host, path, _) = split(image).ok_or_else(|| format!("{image} is not REGISTRY/PATH:TAG"))?;
    let registry = std::sync::Arc::new(Registry::open(host, path).await?);
    let listed = registry.get("tags/list?n=1000", None).await?;
    let tags: Vec<String> = listed["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag.as_str().map(str::to_owned))
        .collect();
    let mut chosen: Vec<String> = tags[tags.len().saturating_sub(count)..].to_vec();
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
    // Newest first; the registry's order where the times are equal or unknown.
    found.sort_by(|a, b| match (&a.2, &b.2) {
        (Some(x), Some(y)) => y.cmp(x).then(b.0.cmp(&a.0)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.0.cmp(&a.0),
    });
    Ok(found
        .into_iter()
        .map(|(_, tag, created)| Published {
            image: format!("{host}/{path}:{tag}"),
            created,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_splits_into_registry_path_and_tag() {
        assert_eq!(
            split("ghcr.io/hipotures/homestack-server:0.1.0-5b8ded498d24"),
            Some(("ghcr.io", "hipotures/homestack-server", "0.1.0-5b8ded498d24"))
        );
        assert_eq!(split("ghcr.io/hipotures/homestack-server"), None);
        assert_eq!(split("registry:5000/image"), None);
        assert_eq!(split("alpine:3.24"), None);
    }
}
