//! The Spaceship API, for the DNS-01 record of the certificate: the
//! `X-API-Key` and `X-API-Secret` headers and `/api/v1/dns/records/<zone>`,
//! as acme.sh's and Proxmox's `dns_spaceship` plugin use them.

use std::path::Path;

use serde_json::{Value, json};

const API: &str = "https://spaceship.dev/api/v1";

/// The credentials of Proxmox's ACME plugin (`SPACESHIP_API_KEY`,
/// `SPACESHIP_API_SECRET`).
#[derive(Clone)]
pub struct SpaceshipCredentials {
    pub key: String,
    pub secret: String,
}

impl std::fmt::Debug for SpaceshipCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpaceshipCredentials").finish_non_exhaustive()
    }
}

impl SpaceshipCredentials {
    /// `KEY=value` lines; `#` starts a comment.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value = |key: &str| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.starts_with('#'))
                .filter_map(|line| line.split_once('='))
                .find(|(name, _)| name.trim() == key)
                .map(|(_, value)| value.trim().trim_matches('"').to_owned())
                .filter(|value| !value.is_empty())
        };
        let required = |key: &str| value(key).ok_or_else(|| format!("{key} is missing"));
        Ok(Self {
            key: required("SPACESHIP_API_KEY")?,
            secret: required("SPACESHIP_API_SECRET")?,
        })
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// The body that adds TXT `sub` with `value`.
#[must_use]
pub fn add_body(sub: &str, value: &str) -> Value {
    json!({"force": true, "items": [{"type": "TXT", "name": sub, "value": value, "ttl": 600}]})
}

/// The body that removes TXT `sub` with `value`.
#[must_use]
pub fn remove_body(sub: &str, value: &str) -> Value {
    json!([{"type": "TXT", "name": sub, "value": value}])
}

pub struct SpaceshipClient {
    http: reqwest::Client,
    credentials: SpaceshipCredentials,
}

impl SpaceshipClient {
    pub fn new(credentials: SpaceshipCredentials) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { http, credentials })
    }

    async fn call(&self, method: reqwest::Method, zone: &str, body: &Value) -> Result<(), String> {
        let path = format!("/dns/records/{zone}");
        let response = self
            .http
            .request(method.clone(), format!("{API}{path}"))
            .header("X-API-Key", &self.credentials.key)
            .header("X-API-Secret", &self.credentials.secret)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| format!("Spaceship {method} {path}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Spaceship {method} {path}: {status}: {}", text.trim()));
        }
        Ok(())
    }

    /// Add TXT `sub` with `value` in `zone`.
    pub async fn add_txt(&self, zone: &str, sub: &str, value: &str) -> Result<(), String> {
        self.call(reqwest::Method::PUT, zone, &add_body(sub, value)).await
    }

    /// Remove TXT `sub` with `value` from `zone`.
    pub async fn remove_txt(&self, zone: &str, sub: &str, value: &str) -> Result<(), String> {
        self.call(reqwest::Method::DELETE, zone, &remove_body(sub, value)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_read_like_the_acme_plugin_writes_them() -> Result<(), String> {
        let credentials =
            SpaceshipCredentials::parse("# a comment\nSPACESHIP_API_KEY=key\nSPACESHIP_API_SECRET=\"secret\"\n")?;
        assert_eq!(
            (credentials.key.as_str(), credentials.secret.as_str()),
            ("key", "secret")
        );
        assert!(!format!("{credentials:?}").contains("secret"));
        assert!(
            SpaceshipCredentials::parse("SPACESHIP_API_KEY=key\nSPACESHIP_API_SECRET=")
                .is_err_and(|e| e.contains("SPACESHIP_API_SECRET"))
        );
        assert!(
            SpaceshipCredentials::parse("#SPACESHIP_API_KEY=key\nSPACESHIP_API_SECRET=s")
                .is_err_and(|e| e.contains("SPACESHIP_API_KEY"))
        );
        Ok(())
    }

    #[test]
    fn the_bodies_are_the_ones_the_acme_plugin_sends() {
        assert_eq!(
            add_body("_acme-challenge", "v").to_string(),
            r#"{"force":true,"items":[{"name":"_acme-challenge","ttl":600,"type":"TXT","value":"v"}]}"#
        );
        assert_eq!(
            remove_body("_acme-challenge", "v").to_string(),
            r#"[{"name":"_acme-challenge","type":"TXT","value":"v"}]"#
        );
    }
}
