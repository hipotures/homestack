//! Client of the HomeStack server API with several instances (mapping 8.3,
//! `docs/migration/phase-11.md`).
//!
//! - A read goes to the first instance that answers, starting from the one
//!   that answered last; one that cannot be reached, or whose proxy answers
//!   `502`–`504` without a HomeStack error document, is skipped.
//! - A plan, its apply and its job stay on the instance that made the plan
//!   ([`Session`]). If that instance stops answering, nothing is sent
//!   elsewhere: the client reports [`ClientError::Lost`], and the caller
//!   reads the state again and makes a new plan (never repeats the old
//!   request blindly).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

/// Header with the configuration version a change expects (mapping 8.2).
pub const CONFIG_VERSION_HEADER: &str = "homestack-config-version";

/// One instance: its URL, and the address its host name resolves to when
/// DNS should not be asked (the deploy command talks to each container by
/// its address, with the certificate's name).
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: Url,
    pub address: Option<SocketAddr>,
}

impl From<Url> for Endpoint {
    fn from(url: Url) -> Self {
        Self { url, address: None }
    }
}

/// How to reach the instances.
#[derive(Clone)]
pub struct ClientConfig {
    pub endpoints: Vec<Endpoint>,
    /// The client token (bearer).
    pub token: String,
    /// PEM certificates trusted besides the system roots.
    pub extra_ca_pem: Option<String>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl ClientConfig {
    #[must_use]
    pub fn new(endpoints: Vec<Endpoint>, token: String) -> Self {
        Self {
            endpoints,
            token,
            extra_ca_pem: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("endpoints", &self.endpoints)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid client configuration: {0}")]
    Config(String),
    /// No instance answered.
    #[error("no HomeStack instance answered: {0}")]
    Unreachable(String),
    /// An instance answered with an error document.
    #[error("{url} answered {status}: {code}: {message}")]
    Api {
        url: String,
        status: u16,
        code: String,
        message: String,
    },
    /// The instance that holds a plan or job stopped answering. Read the
    /// state again and plan anew.
    #[error("instance {url} stopped answering while it held the plan or job: {reason}")]
    Lost { url: String, reason: String },
}

impl ClientError {
    /// The API error code, if an instance answered with one.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// What one request to one instance gave.
enum Reply {
    Ok(Value),
    /// The instance answered with an error document.
    Api(ClientError),
    /// The instance could not be reached, or its proxy answered for it.
    Down(String),
}

fn transport_error(error: reqwest::Error) -> String {
    let kind = match (error.is_timeout(), error.is_connect()) {
        (true, true) => "connect timeout",
        (true, false) => "request timeout",
        (false, true) => "connection failure",
        (false, false) => "transport failure",
    };
    let error = error.without_url();
    let mut text = format!("{kind}: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

struct Instance {
    url: Url,
    http: reqwest::Client,
}

/// A client of every configured instance.
pub struct Client {
    instances: Vec<Instance>,
    token: String,
    /// The instance that answered last, asked first next time: a stopped
    /// one costs its timeout once, not on every request.
    preferred: AtomicUsize,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        if config.endpoints.is_empty() {
            return Err(ClientError::Config("no instance URL".to_owned()));
        }
        let mut instances = Vec::new();
        for endpoint in config.endpoints {
            let mut builder = reqwest::Client::builder()
                .connect_timeout(config.connect_timeout)
                .timeout(config.request_timeout)
                .https_only(endpoint.url.scheme() == "https");
            if let Some(pem) = &config.extra_ca_pem {
                for certificate in reqwest::Certificate::from_pem_bundle(pem.as_bytes())
                    .map_err(|error| ClientError::Config(format!("CA certificate: {error}")))?
                {
                    builder = builder.add_root_certificate(certificate);
                }
            }
            if let (Some(address), Some(host)) = (endpoint.address, endpoint.url.host_str()) {
                builder = builder.resolve(host, address);
            }
            let http = builder
                .build()
                .map_err(|error| ClientError::Config(error.to_string()))?;
            instances.push(Instance {
                url: endpoint.url,
                http,
            });
        }
        Ok(Self {
            instances,
            token: config.token,
            preferred: AtomicUsize::new(0),
        })
    }

    /// The URLs, in order.
    pub fn urls(&self) -> impl Iterator<Item = &Url> {
        self.instances.iter().map(|instance| &instance.url)
    }

    fn url(&self, index: usize, path: &str) -> String {
        let base = self.instances[index].url.as_str().trim_end_matches('/');
        format!("{base}{path}")
    }

    async fn send(&self, index: usize, method: Method, path: &str, body: Body<'_>, headers: &[(&str, &str)]) -> Reply {
        let instance = &self.instances[index];
        let mut request = instance
            .http
            .request(method, self.url(index, path))
            .bearer_auth(&self.token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request = match body {
            Body::None => request,
            Body::Json(value) => request.json(value),
            Body::Text(text, kind) => request.header("content-type", kind).body(text.to_owned()),
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Reply::Down(transport_error(error)),
        };
        let status = response.status();
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => return Reply::Down(transport_error(error)),
        };
        let value: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if status.is_success() {
            return Reply::Ok(value);
        }
        let error = &value["error"];
        let Some(code) = error["code"].as_str() else {
            // Not a HomeStack error document: the proxy in front of a
            // server that does not answer, or something else on the port.
            if matches!(status.as_u16(), 502..=504) {
                return Reply::Down(format!("{status} from the proxy"));
            }
            return Reply::Api(ClientError::Api {
                url: instance.url.to_string(),
                status: status.as_u16(),
                code: "unexpected_response".to_owned(),
                message: format!("{status} without an error document"),
            });
        };
        Reply::Api(ClientError::Api {
            url: instance.url.to_string(),
            status: status.as_u16(),
            code: code.to_owned(),
            message: error["message"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// Send to the first instance that answers; the instance's index and
    /// the body. Instances that are still unconfigured (`config_missing`,
    /// `pve_token_missing`, e.g. during a container replacement) or cannot
    /// reach Proxmox (`pve_unreachable`) are skipped in favor of a ready
    /// instance when one is available.
    async fn first(
        &self,
        method: Method,
        path: &str,
        body: Body<'_>,
        headers: &[(&str, &str)],
    ) -> Result<(usize, Value), ClientError> {
        let mut down = Vec::new();
        let mut unready: Option<ClientError> = None;
        let count = self.instances.len();
        let start = self.preferred.load(Ordering::Relaxed) % count;
        for index in (start..count).chain(0..start) {
            match self.send(index, method.clone(), path, body, headers).await {
                Reply::Ok(value) => {
                    self.preferred.store(index, Ordering::Relaxed);
                    return Ok((index, value));
                }
                Reply::Api(error)
                    if matches!(
                        error.code(),
                        Some("config_missing" | "pve_token_missing" | "pve_unreachable")
                    ) =>
                {
                    if let ClientError::Api { message, .. } = &error {
                        down.push(format!("{}: {message}", self.instances[index].url));
                    }
                    if unready.is_none() {
                        unready = Some(error);
                    }
                }
                Reply::Api(error) => {
                    self.preferred.store(index, Ordering::Relaxed);
                    return Err(error);
                }
                Reply::Down(reason) => down.push(format!("{}: {reason}", self.instances[index].url)),
            }
        }
        if let Some(error) = unready {
            return Err(error);
        }
        Err(ClientError::Unreachable(down.join("; ")))
    }

    /// `GET path` from the first instance that answers.
    pub async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.first(Method::GET, path, Body::None, &[])
            .await
            .map(|(_, value)| value)
    }

    /// `GET path` from the first instance that answers, with that
    /// instance's URL.
    pub async fn get_from(&self, path: &str) -> Result<(Url, Value), ClientError> {
        self.first(Method::GET, path, Body::None, &[])
            .await
            .map(|(index, value)| (self.instances[index].url.clone(), value))
    }

    /// `GET path` from instance `index` only.
    pub async fn get_on(&self, index: usize, path: &str) -> Result<Value, ClientError> {
        self.on(index, Method::GET, path, Body::None, &[]).await
    }

    async fn on(
        &self,
        index: usize,
        method: Method,
        path: &str,
        body: Body<'_>,
        headers: &[(&str, &str)],
    ) -> Result<Value, ClientError> {
        match self.send(index, method, path, body, headers).await {
            Reply::Ok(value) => Ok(value),
            Reply::Api(error) => Err(error),
            Reply::Down(reason) => Err(ClientError::Unreachable(format!(
                "{}: {reason}",
                self.instances[index].url
            ))),
        }
    }

    /// `PUT path` with a text body on instance `index` (deploy: secrets).
    pub async fn put_text_on(
        &self,
        index: usize,
        path: &str,
        text: &str,
        content_type: &str,
    ) -> Result<Value, ClientError> {
        self.on(index, Method::PUT, path, Body::Text(text, content_type), &[])
            .await
    }

    /// `PUT path` with a JSON body on instance `index`.
    pub async fn put_json_on(&self, index: usize, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.on(index, Method::PUT, path, Body::Json(body), &[]).await
    }

    /// Upload `server.toml` to instance `index` unless it holds it already;
    /// the version it holds then.
    pub async fn configure_on(&self, index: usize, toml: &str) -> Result<String, ClientError> {
        let current = self.config_version_on(index).await?;
        let precondition = match &current {
            None => ("if-none-match", "*".to_owned()),
            Some(version) => ("if-match", format!("\"{version}\"")),
        };
        let updated = self
            .on(
                index,
                Method::PUT,
                "/v1/config",
                Body::Text(toml, "application/toml"),
                &[(precondition.0, &precondition.1)],
            )
            .await?;
        updated["config_version"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ClientError::Unreachable(format!("{}: no config_version", self.instances[index].url)))
    }

    /// The configuration version of instance `index` (`None` before the
    /// first upload). A configured instance that answers becomes preferred
    /// for subsequent reads.
    pub async fn config_version_on(&self, index: usize) -> Result<Option<String>, ClientError> {
        let body = self.get_on(index, "/v1/config/version").await?;
        let version = body["config_version"].as_str().map(str::to_owned);
        if version.is_some() {
            self.preferred.store(index, Ordering::Relaxed);
        }
        Ok(version)
    }

    /// Every instance's configuration version, or why it gave none (mapping
    /// 8.2: the client checks the instances agree).
    pub async fn config_versions(&self) -> Vec<(Url, Result<Option<String>, ClientError>)> {
        let mut versions = Vec::new();
        for index in 0..self.instances.len() {
            versions.push((self.instances[index].url.clone(), self.config_version_on(index).await));
        }
        versions
    }

    /// Plan `operation` on the first instance that answers; the plan stays
    /// on it.
    pub async fn plan(&self, operation: &str, parameters: Value) -> Result<Session<'_>, ClientError> {
        let body = json!({"operation": operation, "parameters": parameters});
        let (index, plan) = self.first(Method::POST, "/v1/plans", Body::Json(&body), &[]).await?;
        Ok(Session {
            client: self,
            index,
            plan,
        })
    }
}

#[derive(Clone, Copy)]
enum Body<'a> {
    None,
    Json(&'a Value),
    Text(&'a str, &'a str),
}

/// A plan and its job, on the instance that made the plan.
pub struct Session<'a> {
    client: &'a Client,
    index: usize,
    pub plan: Value,
}

impl Session<'_> {
    /// The instance's URL.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.client.instances[self.index].url
    }

    fn lost(&self, error: ClientError) -> ClientError {
        match error {
            ClientError::Unreachable(reason) => ClientError::Lost {
                url: self.url().to_string(),
                reason,
            },
            other => other,
        }
    }

    /// Apply the plan with the configuration version it was made with; the
    /// job as accepted.
    pub async fn apply(&self, config_version: &str) -> Result<Value, ClientError> {
        self.apply_with(config_version, None).await
    }

    /// As [`apply`](Self::apply), with the secrets the plan's
    /// `secrets_required` names (decision 6: the LUKS passphrase).
    pub async fn apply_with(&self, config_version: &str, secrets: Option<&Value>) -> Result<Value, ClientError> {
        let id = self.plan["id"].as_str().unwrap_or_default();
        let mut body = json!({"digest": self.plan["digest"]});
        if let Some(secrets) = secrets {
            body["secrets"] = secrets.clone();
        }
        self.client
            .on(
                self.index,
                Method::POST,
                &format!("/v1/plans/{id}/apply"),
                Body::Json(&body),
                &[(CONFIG_VERSION_HEADER, config_version)],
            )
            .await
            .map_err(|error| self.lost(error))
    }

    /// The configuration version of the instance that holds the plan.
    pub async fn config_version(&self) -> Result<Option<String>, ClientError> {
        self.client
            .config_version_on(self.index)
            .await
            .map_err(|error| self.lost(error))
    }

    /// The job, from the instance that runs it.
    pub async fn job(&self, id: &str) -> Result<Value, ClientError> {
        self.client
            .get_on(self.index, &format!("/v1/jobs/{id}"))
            .await
            .map_err(|error| self.lost(error))
    }

    /// Follow the job until it ends, at most `limit`.
    pub async fn wait(&self, job: &Value, limit: Duration) -> Result<Value, ClientError> {
        let id = job["id"].as_str().unwrap_or_default().to_owned();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let job = self.job(&id).await?;
            if matches!(job["state"].as_str(), Some("succeeded" | "failed")) || tokio::time::Instant::now() >= deadline
            {
                return Ok(job);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
