//! The OVH API, for the DNS-01 record of the certificate: signed requests
//! as `python-ovh` makes them (`X-Ovh-Signature` = `$1$` + SHA-1 of
//! `secret+consumer+METHOD+url+body+timestamp`, the timestamp corrected
//! by `GET /auth/time`).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};

/// The credentials of Proxmox's ACME plugin (`OVH_AK`, `OVH_AS`, `OVH_CK`,
/// `OVH_END_POINT`).
#[derive(Clone)]
pub struct OvhCredentials {
    pub application_key: String,
    pub application_secret: String,
    pub consumer_key: String,
    /// API base, e.g. `https://eu.api.ovh.com/1.0`.
    pub endpoint: String,
}

impl std::fmt::Debug for OvhCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OvhCredentials")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// `OVH_END_POINT` names as `python-ovh` knows them.
fn endpoint_url(name: &str) -> Option<&'static str> {
    match name {
        "ovh-eu" => Some("https://eu.api.ovh.com/1.0"),
        "ovh-ca" => Some("https://ca.api.ovh.com/1.0"),
        "ovh-us" => Some("https://api.us.ovhcloud.com/1.0"),
        _ => None,
    }
}

impl OvhCredentials {
    /// `KEY=value` lines; `OVH_END_POINT` defaults to `ovh-eu`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value = |key: &str| {
            text.lines()
                .filter_map(|line| line.trim().split_once('='))
                .find(|(name, _)| name.trim() == key)
                .map(|(_, value)| value.trim().trim_matches('"').to_owned())
                .filter(|value| !value.is_empty())
        };
        let required = |key: &str| value(key).ok_or_else(|| format!("{key} is missing"));
        let endpoint_name = value("OVH_END_POINT").unwrap_or_else(|| "ovh-eu".to_owned());
        let endpoint = if endpoint_name.starts_with("https://") {
            endpoint_name
        } else {
            endpoint_url(&endpoint_name)
                .ok_or_else(|| format!("unknown OVH_END_POINT {endpoint_name}"))?
                .to_owned()
        };
        Ok(Self {
            application_key: required("OVH_AK")?,
            application_secret: required("OVH_AS")?,
            consumer_key: required("OVH_CK")?,
            endpoint,
        })
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// `X-Ovh-Signature` of one request.
#[must_use]
pub fn signature(credentials: &OvhCredentials, method: &str, url: &str, body: &str, timestamp: i64) -> String {
    let text = format!(
        "{}+{}+{}+{}+{}+{}",
        credentials.application_secret, credentials.consumer_key, method, url, body, timestamp
    );
    let digest = Sha1::digest(text.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("$1${hex}")
}

pub struct OvhClient {
    http: reqwest::Client,
    credentials: OvhCredentials,
    time_delta: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

impl OvhClient {
    /// A client whose clock follows OVH's (`GET /auth/time`).
    pub async fn connect(credentials: OvhCredentials) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        let server: i64 = http
            .get(format!("{}/auth/time", credentials.endpoint))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("OVH /auth/time: {error}"))?
            .json()
            .await
            .map_err(|error| format!("OVH /auth/time: {error}"))?;
        Ok(Self {
            http,
            credentials,
            time_delta: server - now(),
        })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}{path}", self.credentials.endpoint);
        let body_text = body.map(Value::to_string).unwrap_or_default();
        let timestamp = now() + self.time_delta;
        let mut request = self
            .http
            .request(method.clone(), &url)
            .header("X-Ovh-Application", &self.credentials.application_key)
            .header("X-Ovh-Consumer", &self.credentials.consumer_key)
            .header("X-Ovh-Timestamp", timestamp.to_string())
            .header(
                "X-Ovh-Signature",
                signature(&self.credentials, method.as_str(), &url, &body_text, timestamp),
            );
        if body.is_some() {
            request = request.header("Content-Type", "application/json").body(body_text);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("OVH {method} {path}: {error}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("OVH {method} {path}: {status}: {}", text.trim()));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Authenticate and verify read access to a DNS zone without modifying it.
    pub async fn check_zone(&self, zone: &str) -> Result<(), String> {
        let response = self
            .call(reqwest::Method::GET, &format!("/domain/zone/{zone}"), None)
            .await?;
        if !response.is_object() {
            return Err("Invalid OVH zone response".to_owned());
        }
        Ok(())
    }

    /// Add a TXT record `sub` in `zone` and apply the zone; the record's id.
    pub async fn add_txt(&self, zone: &str, sub: &str, value: &str) -> Result<u64, String> {
        let record = self
            .call(
                reqwest::Method::POST,
                &format!("/domain/zone/{zone}/record"),
                Some(&json!({"fieldType": "TXT", "subDomain": sub, "target": value, "ttl": 60})),
            )
            .await?;
        let id = record["id"]
            .as_u64()
            .ok_or_else(|| format!("OVH gave no record id: {record}"))?;
        self.refresh(zone).await?;
        Ok(id)
    }

    /// Remove record `id` from `zone` and apply the zone.
    pub async fn remove(&self, zone: &str, id: u64) -> Result<(), String> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/domain/zone/{zone}/record/{id}"),
            None,
        )
        .await?;
        self.refresh(zone).await
    }

    /// The authoritative name servers OVH serves `zone` from.
    pub async fn name_servers(&self, zone: &str) -> Result<Vec<String>, String> {
        let info = self
            .call(reqwest::Method::GET, &format!("/domain/zone/{zone}"), None)
            .await?;
        Ok(info["nameServers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|name| name.trim_end_matches('.').to_owned())
            .collect())
    }

    async fn refresh(&self, zone: &str) -> Result<(), String> {
        self.call(reqwest::Method::POST, &format!("/domain/zone/{zone}/refresh"), None)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_read_like_the_acme_plugin_writes_them() -> Result<(), String> {
        let credentials = OvhCredentials::parse("OVH_AK=ak\nOVH_AS=as\nOVH_CK=ck\n")?;
        assert_eq!(credentials.endpoint, "https://eu.api.ovh.com/1.0");
        assert_eq!(credentials.application_key, "ak");
        assert!(!format!("{credentials:?}").contains("as"));
        let ca = OvhCredentials::parse("OVH_AK=a\nOVH_AS=b\nOVH_CK=c\nOVH_END_POINT=ovh-ca")?;
        assert_eq!(ca.endpoint, "https://ca.api.ovh.com/1.0");
        assert!(OvhCredentials::parse("OVH_AK=a\nOVH_AS=b").is_err_and(|e| e.contains("OVH_CK")));
        Ok(())
    }

    #[test]
    fn the_signature_is_sha1_of_the_joined_request() {
        let credentials = OvhCredentials {
            application_key: "ak".to_owned(),
            application_secret: "as".to_owned(),
            consumer_key: "ck".to_owned(),
            endpoint: "https://eu.api.ovh.com/1.0".to_owned(),
        };
        // `echo -n "as+ck+GET+https://eu.api.ovh.com/1.0/me++1700000000" | sha1sum`
        assert_eq!(
            signature(&credentials, "GET", "https://eu.api.ovh.com/1.0/me", "", 1_700_000_000),
            "$1$212ae9d571c1dbc085f3630b6c2b68d5b3fdc482"
        );
    }
}
