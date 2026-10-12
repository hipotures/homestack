//! HTTP client with endpoint failover.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hs_model::NodeName;
use hs_model::pve::{NodeStatus, PveVersion};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Certificate, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::{EndpointFailure, PveError};
use crate::token::PveToken;

/// Status Proxmox uses when it cannot proxy a request to another node.
const PROXY_FAILURE: u16 = 595;

#[derive(Debug, Clone)]
pub struct PveClientConfig {
    /// API origins, e.g. `https://pve1.example:8006`, tried in order.
    pub endpoints: Vec<Url>,
    pub token: PveToken,
    /// PEM certificates trusted in addition to the system roots.
    pub extra_ca_pem: Option<String>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl PveClientConfig {
    #[must_use]
    pub fn new(endpoints: Vec<Url>, token: PveToken) -> Self {
        Self {
            endpoints,
            token,
            extra_ca_pem: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
        }
    }
}

pub struct PveClient {
    http: reqwest::Client,
    endpoints: Vec<Url>,
    authorization: HeaderValue,
    /// Index of the endpoint that answered last; tried first next time.
    preferred: AtomicUsize,
}

/// One entry of `GET /nodes`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NodeEntry {
    pub node: String,
    pub status: NodeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    Online,
    Offline,
    #[serde(other)]
    Unknown,
}

impl NodeEntry {
    #[must_use]
    pub fn online(&self) -> bool {
        self.status == NodeState::Online
    }
}

#[derive(Deserialize)]
struct NodeStatusData {
    pveversion: String,
    kversion: String,
    uptime: u64,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    message: Option<String>,
}

impl PveClient {
    pub fn new(config: PveClientConfig) -> Result<Self, PveError> {
        if config.endpoints.is_empty() {
            return Err(PveError::Config("at least one endpoint is required".to_owned()));
        }
        for endpoint in &config.endpoints {
            validate_endpoint(endpoint)?;
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout);
        if let Some(pem) = config.extra_ca_pem.as_deref().filter(|pem| !pem.trim().is_empty()) {
            let certs = Certificate::from_pem_bundle(pem.as_bytes())
                .map_err(|error| PveError::Config(format!("extra CA: {error}")))?;
            if certs.is_empty() {
                return Err(PveError::Config("extra CA: no certificate found".to_owned()));
            }
            builder = builder.tls_certs_merge(certs);
        }
        let http = builder.build().map_err(|error| PveError::Config(error_chain(&error)))?;
        let mut authorization = HeaderValue::from_str(&config.token.header_value())
            .map_err(|_| PveError::Config("the API token is not a valid header value".to_owned()))?;
        authorization.set_sensitive(true);
        Ok(Self {
            http,
            endpoints: config.endpoints,
            authorization,
            preferred: AtomicUsize::new(0),
        })
    }

    /// `GET /version`
    pub async fn version(&self) -> Result<PveVersion, PveError> {
        self.get(&["version"]).await
    }

    /// `GET /nodes`
    pub async fn nodes(&self) -> Result<Vec<NodeEntry>, PveError> {
        self.get(&["nodes"]).await
    }

    /// `GET /nodes/{node}/status`, reduced to what HomeStack reports.
    pub async fn node_status(&self, node: &NodeName) -> Result<NodeStatus, PveError> {
        let data: NodeStatusData = self.get(&["nodes", node.as_str(), "status"]).await?;
        Ok(NodeStatus {
            node: node.to_string(),
            pveversion: data.pveversion,
            kversion: data.kversion,
            uptime: data.uptime,
        })
    }

    /// `GET /access/permissions?path=...`: the privileges the token holds
    /// on one ACL path, with inheritance already applied by Proxmox. Works
    /// for any path, existing or not.
    pub async fn permissions(&self, path: &str) -> Result<BTreeSet<String>, PveError> {
        let mut by_path: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
            self.get_query(&["access", "permissions"], &[("path", path)]).await?;
        Ok(by_path
            .remove(path)
            .map(|privileges| privileges.into_keys().collect())
            .unwrap_or_default())
    }

    async fn get<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, PveError> {
        self.get_query(segments, &[]).await
    }

    /// GET an API path given as segments below `/api2/json`.
    ///
    /// Endpoints are tried starting with the one that answered last. Only a
    /// failure to connect (including TLS verification) moves on to the next
    /// endpoint: the request has not been sent then, so trying elsewhere is
    /// safe for any method. Once a request is sent, its outcome is final.
    async fn get_query<T: DeserializeOwned>(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<T, PveError> {
        let start = self.preferred.load(Ordering::Relaxed);
        let mut failures = Vec::new();
        for offset in 0..self.endpoints.len() {
            let index = (start + offset) % self.endpoints.len();
            let endpoint = &self.endpoints[index];
            let mut url = api_url(endpoint, segments)?;
            if !query.is_empty() {
                url.query_pairs_mut().extend_pairs(query);
            }
            let sent = self
                .http
                .get(url)
                .header(AUTHORIZATION, self.authorization.clone())
                .send()
                .await;
            match sent {
                Ok(response) => {
                    self.preferred.store(index, Ordering::Relaxed);
                    return read_response(response).await;
                }
                Err(error) if error.is_connect() => {
                    let failure = EndpointFailure {
                        endpoint: origin(endpoint),
                        detail: error_chain(&error),
                    };
                    tracing::warn!(endpoint = %failure.endpoint, detail = %failure.detail, "Proxmox endpoint unavailable");
                    failures.push(failure);
                }
                Err(error) => return Err(PveError::Response(error_chain(&error))),
            }
        }
        Err(PveError::Unreachable(failures))
    }
}

async fn read_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, PveError> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| PveError::Response(error_chain(&error)))?;
    if status.is_success() {
        let envelope: Envelope<T> =
            serde_json::from_slice(&body).map_err(|error| PveError::Response(format!("invalid JSON: {error}")))?;
        return Ok(envelope.data);
    }
    let message = serde_json::from_slice::<ErrorEnvelope>(&body)
        .ok()
        .and_then(|envelope| envelope.message)
        .map(|message| message.trim_end().to_owned())
        .unwrap_or_else(|| status.canonical_reason().unwrap_or_default().to_owned());
    Err(match status {
        StatusCode::UNAUTHORIZED => PveError::Unauthorized,
        StatusCode::FORBIDDEN => PveError::Forbidden { message },
        status if status.as_u16() == PROXY_FAILURE => PveError::NodeUnreachable { message },
        status => PveError::Api {
            status: status.as_u16(),
            message,
        },
    })
}

fn validate_endpoint(endpoint: &Url) -> Result<(), PveError> {
    let plain_origin = endpoint.scheme() == "https"
        && endpoint.host_str().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.path() == "/"
        && endpoint.query().is_none()
        && endpoint.fragment().is_none();
    if plain_origin {
        Ok(())
    } else {
        Err(PveError::Config(format!(
            "endpoint {} must be an https origin such as https://pve1.example:8006",
            origin(endpoint)
        )))
    }
}

fn api_url(endpoint: &Url, segments: &[&str]) -> Result<Url, PveError> {
    let mut url = endpoint.clone();
    url.path_segments_mut()
        .map_err(|()| PveError::Config(format!("endpoint {} cannot have a path", origin(endpoint))))?
        .clear()
        .extend(["api2", "json"])
        .extend(segments);
    Ok(url)
}

/// Scheme, host and port only, so nothing else from the URL is ever logged.
fn origin(endpoint: &Url) -> String {
    endpoint.origin().ascii_serialization()
}

/// An error and its sources on one line.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
