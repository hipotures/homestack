//! L4: the server against the real cluster. Read-only.
//!
//! Ignored by default; `scripts/check.sh --live` runs them with
//!
//! - `HOMESTACK_LIVE_TOKEN_FILE`: file with `USER@REALM!TOKENID=SECRET`,
//! - `HOMESTACK_LIVE_PVE_ENDPOINTS`: comma-separated API origins whose
//!   certificates verify against the system roots, e.g.
//!   `https://pve1.example:8006,https://pve2.example:8006`.

use std::net::TcpListener;
use std::path::PathBuf;

use hs_pve::{PveClient, PveClientConfig, PveToken};
use hs_server::{ApiToken, ServerConfig};
use reqwest::StatusCode;
use serde_json::Value;
use url::Url;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const API_TOKEN: &str = "live-api-token-0123456789abcdef0123456789";

fn live_token() -> Result<PveToken, Box<dyn std::error::Error>> {
    let path = std::env::var("HOMESTACK_LIVE_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TOKEN_FILE is not set")?;
    Ok(PveToken::from_file(&PathBuf::from(path))?)
}

fn live_endpoints() -> Result<Vec<Url>, Box<dyn std::error::Error>> {
    let list = std::env::var("HOMESTACK_LIVE_PVE_ENDPOINTS").map_err(|_| "HOMESTACK_LIVE_PVE_ENDPOINTS is not set")?;
    let endpoints = list
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    if endpoints.is_empty() {
        return Err("HOMESTACK_LIVE_PVE_ENDPOINTS is empty".into());
    }
    Ok(endpoints)
}

async fn serve(endpoints: Vec<Url>) -> Result<String, Box<dyn std::error::Error>> {
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?,
        pve: PveClientConfig::new(endpoints, live_token()?),
    })?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(format!("http://{addr}"))
}

async fn get(base: &str, path: &str) -> Result<(StatusCode, Value), reqwest::Error> {
    let response = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(API_TOKEN)
        .send()
        .await?;
    let status = response.status();
    Ok((status, response.json().await?))
}

/// An origin on this host where nothing listens.
fn dead_endpoint() -> Result<Url, Box<dyn std::error::Error>> {
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    Ok(Url::parse(&format!("https://localhost:{port}"))?)
}

/// The first endpoint addressed by IP instead of name: the certificate does
/// not cover it, so TLS verification must fail.
async fn wrong_name_endpoint(endpoints: &[Url]) -> Result<Url, Box<dyn std::error::Error>> {
    let first = endpoints.first().ok_or("no endpoint")?;
    let host = first.host_str().ok_or("endpoint without host")?;
    let port = first.port_or_known_default().ok_or("endpoint without port")?;
    let ip = tokio::net::lookup_host((host, port))
        .await?
        .map(|addr| addr.ip())
        .find(std::net::IpAddr::is_ipv4)
        .ok_or("endpoint has no IPv4 address")?;
    Ok(Url::parse(&format!("https://{ip}:{port}"))?)
}

#[tokio::test]
#[ignore = "live cluster: run through scripts/check.sh --live"]
async fn live_version_matches_proxmox_on_every_endpoint() -> TestResult {
    let endpoints = live_endpoints()?;
    let base = serve(endpoints.clone()).await?;
    let (status, served) = get(&base, "/v1/pve/version").await?;
    assert_eq!(status, StatusCode::OK, "{served}");
    for endpoint in endpoints {
        let direct = PveClient::new(PveClientConfig::new(vec![endpoint.clone()], live_token()?))?
            .version()
            .await?;
        assert_eq!(served["version"], direct.version, "{endpoint}");
        assert_eq!(served["release"], direct.release, "{endpoint}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live cluster: run through scripts/check.sh --live"]
async fn live_status_of_every_online_node() -> TestResult {
    let endpoints = live_endpoints()?;
    let nodes = PveClient::new(PveClientConfig::new(endpoints.clone(), live_token()?))?
        .nodes()
        .await?;
    let base = serve(endpoints).await?;
    for node in nodes.iter().filter(|node| node.online()) {
        let (status, body) = get(&base, &format!("/v1/pve/nodes/{}/status", node.node)).await?;
        assert_eq!(status, StatusCode::OK, "{}: {body}", node.node);
        assert_eq!(body["node"], node.node.as_str());
        assert!(
            body["pveversion"]
                .as_str()
                .is_some_and(|v| v.starts_with("pve-manager/")),
            "{body}"
        );
    }
    for node in nodes.iter().filter(|node| !node.online()) {
        eprintln!("skipped (offline): {}", node.node);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "live cluster: run through scripts/check.sh --live"]
async fn live_failover_skips_a_dead_first_endpoint() -> TestResult {
    let mut endpoints = vec![dead_endpoint()?];
    endpoints.extend(live_endpoints()?);
    let base = serve(endpoints).await?;
    let (status, body) = get(&base, "/v1/pve/version").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

#[tokio::test]
#[ignore = "live cluster: run through scripts/check.sh --live"]
async fn live_certificate_for_another_name_is_refused() -> TestResult {
    let endpoints = live_endpoints()?;
    let by_ip = wrong_name_endpoint(&endpoints).await?;

    let base = serve(vec![by_ip.clone()]).await?;
    let (status, body) = get(&base, "/v1/pve/version").await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "pve_unavailable");

    let mut with_fallback = vec![by_ip];
    with_fallback.extend(endpoints);
    let base = serve(with_fallback).await?;
    let (status, body) = get(&base, "/v1/pve/version").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}
