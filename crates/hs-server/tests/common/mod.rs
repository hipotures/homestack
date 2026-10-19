//! The server in-process against the Proxmox simulator, shared by the API
//! tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::Value;

pub type TestResult = Result<(), Box<dyn std::error::Error>>;
pub type Error = Box<dyn std::error::Error>;

pub const API_TOKEN: &str = "test-api-token-0123456789abcdef0123456789";
pub const PVE_TOKEN: &str = "homestack@pve!server=11111111-2222-3333-4444-555555555555";

/// A cluster with an online `node-a`, an offline `node-b` and the server's
/// token with the given ACL (TOML inline-table body).
pub fn spec(token_acl: &str) -> Result<ClusterSpec, toml::de::Error> {
    toml::from_str(&format!(
        r#"
        version = {{ release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }}

        [[nodes]]
        name = "node-a"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 1000

        [[nodes]]
        name = "node-b"
        status = "offline"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 0

        [[tokens]]
        id = "homestack@pve!server"
        secret = "11111111-2222-3333-4444-555555555555"
        acl = {{ {token_acl} }}
        "#
    ))
}

/// A valid `server.toml` for the simulated cluster.
pub const SERVER_TOML: &str = r#"version = 1

[node]
name = "node-1"
gold_vmid = 100

[storage_layouts]
homestack-storage-1 = ["fast", "bulk"]
homestack-storage-2 = ["other"]

[root]
disk = "scsi0"

[home]
disk = "scsi1"
default_size = "20G"

[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]

[cloud_init]
snippet_storage = "snippets"
snippet_dir = "/var/lib/vz/snippets"

[user]
name = "developer"
uid = 1000
gid = 1000
"#;

pub struct Server {
    pub base: String,
    pub http: reqwest::Client,
    pub config_file: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Server {
    /// Start with the configuration file in `dir` (`server.toml`).
    pub async fn start(sim: &PveSim, pve_endpoints: &[String], pve_token: &str, dir: &Path) -> Result<Self, Error> {
        Self::start_with(sim, pve_endpoints, pve_token, &dir.join("server.toml")).await
    }

    pub async fn start_with(
        sim: &PveSim,
        pve_endpoints: &[String],
        pve_token: &str,
        config_file: &Path,
    ) -> Result<Self, Error> {
        let endpoints = pve_endpoints.iter().map(|e| e.parse()).collect::<Result<Vec<_>, _>>()?;
        let mut pve = PveClientConfig::new(endpoints, PveToken::parse(pve_token)?);
        pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
        let app = hs_server::app(ServerConfig {
            api_token: ApiToken::parse(API_TOKEN)?.into(),
            pve: pve.into(),
            config_file: config_file.to_owned(),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools::default(),
        })?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
            config_file: config_file.to_owned(),
            task,
        })
    }

    /// Isolate endpoint assertions from the automatic startup inventory read.
    pub async fn finish_startup(&self, sim: &PveSim) -> Result<(), Error> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let (status, _, progress) = self.get("/v1/images/check", Some(API_TOKEN)).await?;
                assert_eq!(status, StatusCode::OK);
                if progress["checking"] == false {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, Error>(())
        })
        .await??;
        sim.clear_ops();
        Ok(())
    }

    pub fn stop(self) {
        self.task.abort();
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<Reply, reqwest::Error> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.header("content-type", "application/toml").body(body.to_owned());
        }
        let response = request.send().await?;
        Ok(Reply {
            status: response.status(),
            headers: response.headers().clone(),
            text: response.text().await?,
        })
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> Result<(StatusCode, HeaderMap, Value), reqwest::Error> {
        let reply = self.send(reqwest::Method::GET, path, token, &[], None).await?;
        let body = reply.json();
        Ok((reply.status, reply.headers, body))
    }

    pub async fn put_config(&self, text: &str, precondition: (&str, &str)) -> Result<Reply, reqwest::Error> {
        self.send(
            reqwest::Method::PUT,
            "/v1/config",
            Some(API_TOKEN),
            &[precondition],
            Some(text),
        )
        .await
    }
}

pub fn closed_port_url() -> Result<String, std::io::Error> {
    let listener = std::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();
    Ok(format!("https://localhost:{port}"))
}

pub fn assert_error(body: &Value, headers: &HeaderMap, code: &str, retryable: bool) {
    assert_eq!(body["schema_version"], 1, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert_eq!(body["error"]["retryable"], retryable, "{body}");
    let request_id = headers.get("x-request-id").and_then(|v| v.to_str().ok());
    assert!(request_id.is_some_and(|id| !id.is_empty()));
    assert_eq!(body["error"]["request_id"].as_str(), request_id, "{body}");
}

/// Quoted entity tag for a configuration version.
pub fn etag(version: &str) -> String {
    format!("\"{version}\"")
}
