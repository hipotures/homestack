//! The HTTP API in-process, against the Proxmox simulator.

use std::net::{SocketAddr, TcpListener};

use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const API_TOKEN: &str = "test-api-token-0123456789abcdef0123456789";
const PVE_TOKEN: &str = "homestack@pve!server=11111111-2222-3333-4444-555555555555";

fn spec(token_acl: &str) -> Result<ClusterSpec, toml::de::Error> {
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

struct Server {
    base: String,
    http: reqwest::Client,
}

impl Server {
    async fn start(
        sim: &PveSim,
        pve_endpoints: &[String],
        pve_token: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let endpoints = pve_endpoints.iter().map(|e| e.parse()).collect::<Result<Vec<_>, _>>()?;
        let mut pve = PveClientConfig::new(endpoints, PveToken::parse(pve_token)?);
        pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
        let config = ServerConfig {
            api_token: ApiToken::parse(API_TOKEN)?,
            pve,
        };
        let app = hs_server::app(config)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Self {
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
        })
    }

    async fn get(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Value), reqwest::Error> {
        let mut request = self.http.get(format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, headers, body))
    }
}

fn closed_port_url() -> Result<String, std::io::Error> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();
    Ok(format!("https://localhost:{port}"))
}

fn assert_error(body: &Value, headers: &reqwest::header::HeaderMap, code: &str, retryable: bool) {
    assert_eq!(body["schema_version"], 1, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert_eq!(body["error"]["retryable"], retryable, "{body}");
    let request_id = headers.get("x-request-id").and_then(|v| v.to_str().ok());
    assert!(request_id.is_some_and(|id| !id.is_empty()));
    assert_eq!(body["error"]["request_id"].as_str(), request_id, "{body}");
}

#[tokio::test]
async fn liveness_needs_no_token_and_responses_are_not_cached() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, headers, body) = server.get("/health/live", None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status": "ok"}));
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert!(sim.ops().is_empty(), "liveness must not call Proxmox");
    Ok(())
}

#[tokio::test]
async fn api_routes_require_the_bearer_token() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    for token in [None, Some("wrong-token-wrong-token-wrong-token-xx")] {
        let (status, headers, body) = server.get("/v1/pve/version", token).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers.get("www-authenticate").and_then(|v| v.to_str().ok()),
            Some("Bearer")
        );
        assert_error(&body, &headers, "unauthorized", false);
    }
    assert!(sim.ops().is_empty());
    Ok(())
}

#[tokio::test]
async fn pve_version_is_passed_through() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, headers, body) = server.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"release": "9.2", "version": "9.2.21", "repoid": "4f6e0ac86f9e8c7f"})
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    Ok(())
}

#[tokio::test]
async fn node_status_is_reduced_to_reported_fields() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, _, body) = server.get("/v1/pve/nodes/node-a/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"node": "node-a", "pveversion": "pve-manager/9.2.21/4f6e0ac86f9e8c7f", "kversion": "Linux 7.0.14-20-pve", "uptime": 1000})
    );
    Ok(())
}

#[tokio::test]
async fn invalid_input_is_rejected_without_being_echoed() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, headers, body) = server
        .get("/v1/pve/nodes/evil_name%3Cscript%3E/status", Some(API_TOKEN))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_error(&body, &headers, "validation_failed", false);
    assert!(!body.to_string().contains("evil"), "{body}");

    let (status, headers, body) = server.get("/v1/nothing/evil-path", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, &headers, "not_found", false);
    assert!(!body.to_string().contains("evil"), "{body}");

    let response = server
        .http
        .post(format!("{}/v1/pve/version", server.base))
        .bearer_auth(API_TOKEN)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let headers = response.headers().clone();
    assert_error(&response.json().await?, &headers, "method_not_allowed", false);
    assert!(sim.ops().is_empty());
    Ok(())
}

#[tokio::test]
async fn proxmox_failures_map_to_stable_error_codes() -> TestResult {
    let sim = PveSim::start(spec(r#""/vms" = ["VM.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, headers, body) = server.get("/v1/pve/nodes/node-a/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_permission_denied", false);

    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN).await?;
    let (status, headers, body) = server.get("/v1/pve/nodes/node-b/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, &headers, "pve_node_unavailable", true);

    let (status, headers, body) = server.get("/v1/pve/nodes/nosuchnode/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_error", false);

    let wrong = Server::start(&sim, &[sim.base_url()], "homestack@pve!server=wrong").await?;
    let (status, headers, body) = wrong.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_auth_failed", false);

    let down = Server::start(&sim, &[closed_port_url()?], PVE_TOKEN).await?;
    let (status, headers, body) = down.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, &headers, "pve_unavailable", true);
    Ok(())
}

#[tokio::test]
async fn the_first_reachable_endpoint_serves_requests() -> TestResult {
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[closed_port_url()?, sim.base_url()], PVE_TOKEN).await?;
    let (status, _, _) = server.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[test]
fn api_tokens_must_be_long_enough() {
    assert!(ApiToken::parse("short").is_err());
    assert!(ApiToken::parse(&"x".repeat(31)).is_err());
    assert!(ApiToken::parse(&format!(" {}\n", "x".repeat(32))).is_ok());
    assert!(ApiToken::parse(&format!("{} {}", "x".repeat(32), "y")).is_err());
}

#[test]
fn checked_in_openapi_document_is_current() -> TestResult {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/openapi.json");
    let generated = hs_server::openapi_json()?;
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &generated)?;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "docs/openapi.json is out of date; run `UPDATE_OPENAPI=1 cargo test -p hs-server --test api checked_in_openapi`"
    );
    Ok(())
}
