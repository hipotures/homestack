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
    // Read-only routes: the configuration file is never written.
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?,
        pve: PveClientConfig::new(endpoints, live_token()?),
        config_file: std::env::temp_dir().join("homestack-live-unused/server.toml"),
        pools: hs_server::Pools::default(),
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

/// A `server.toml` for the live cluster: its first online node with a
/// numeric suffix, `local` as the only storage. Only ACL paths depend on it,
/// and Proxmox answers permission queries for any path.
async fn live_server_toml(endpoints: &[Url]) -> Result<String, Box<dyn std::error::Error>> {
    let nodes = PveClient::new(PveClientConfig::new(endpoints.to_vec(), live_token()?))?
        .nodes()
        .await?;
    let (node, number) = nodes
        .iter()
        .filter(|node| node.online())
        .find_map(|node| {
            let digits = node.node.trim_start_matches(|c: char| !c.is_ascii_digit());
            let number = digits.trim_start_matches('0');
            (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
                .then(|| (node.node.clone(), number.to_owned()))
        })
        .ok_or("no online node with a numeric suffix")?;
    Ok(format!(
        "version = 1\n[node]\nname = \"{node}\"\ngold_vmid = 100\n[storage_layouts]\nhomestack-storage-{number} = [\"local\"]\n\
         [root]\ndisk = \"scsi0\"\n[home]\ndisk = \"scsi1\"\ndefault_size = \"20G\"\n\
         [network]\nprefix = \"192.0.2\"\ncidr = 24\ngateway = \"192.0.2.1\"\ndns = [\"192.0.2.53\"]\n\
         [cloud_init]\nsnippet_storage = \"local\"\nsnippet_dir = \"/var/lib/vz/snippets\"\n\
         [user]\nname = \"user\"\nuid = 1000\ngid = 1000\n"
    ))
}

/// Phase 1 gate: `GET /v1/capabilities` agrees with what Proxmox says the
/// token holds. Writes only to a temporary configuration file.
#[tokio::test]
#[ignore = "live cluster: run through scripts/check.sh --live"]
async fn live_capabilities_match_the_token_privileges() -> TestResult {
    let endpoints = live_endpoints()?;
    let state = tempfile::tempdir()?;
    let app = hs_server::app(ServerConfig {
        api_token: ApiToken::parse(API_TOKEN)?,
        pve: PveClientConfig::new(endpoints.clone(), live_token()?),
        config_file: state.path().join("server.toml"),
        pools: hs_server::Pools::default(),
    })?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });

    let upload = reqwest::Client::new()
        .put(format!("{base}/v1/config"))
        .bearer_auth(API_TOKEN)
        .header("if-none-match", "*")
        .body(live_server_toml(&endpoints).await?)
        .send()
        .await?;
    assert_eq!(upload.status(), StatusCode::OK, "{}", upload.text().await?);

    let (status, body) = get(&base, "/v1/capabilities").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = live_token()?;
    assert_eq!(body["token_id"], token.id());
    let direct = PveClient::new(PveClientConfig::new(endpoints, token))?;
    let operations = body["operations"].as_array().ok_or("no operations")?;
    assert_eq!(operations.len(), 10);
    for operation in operations {
        let mut expected_missing = Vec::new();
        for required in operation["required"].as_array().ok_or("no required list")? {
            let path = required["path"].as_str().ok_or("path")?;
            let privilege = required["privilege"].as_str().ok_or("privilege")?;
            if !direct.permissions(path).await?.contains(privilege) {
                expected_missing.push(required.clone());
            }
        }
        assert_eq!(
            operation["missing"],
            Value::Array(expected_missing.clone()),
            "{operation}"
        );
        assert_eq!(operation["available"], expected_missing.is_empty(), "{operation}");
        eprintln!(
            "{}: {}",
            operation["operation"],
            if expected_missing.is_empty() {
                "available".to_owned()
            } else {
                format!("{} missing", expected_missing.len())
            }
        );
    }
    Ok(())
}
