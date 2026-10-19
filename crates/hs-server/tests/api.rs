//! The HTTP API in-process, against the Proxmox simulator.

mod common;

use common::{API_TOKEN, PVE_TOKEN, Server, TestResult, assert_error, closed_port_url, spec};
use hs_server::ApiToken;
use pve_sim::PveSim;
use reqwest::StatusCode;
use serde_json::json;

#[tokio::test]
async fn liveness_needs_no_token_and_responses_are_not_cached() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    server.finish_startup(&sim).await?;
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
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    server.finish_startup(&sim).await?;
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
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
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
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
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
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    server.finish_startup(&sim).await?;
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
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/vms" = ["VM.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let (status, headers, body) = server.get("/v1/pve/nodes/node-a/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_permission_denied", false);

    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let (status, headers, body) = server.get("/v1/pve/nodes/node-b/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, &headers, "pve_node_unavailable", true);

    let (status, headers, body) = server.get("/v1/pve/nodes/nosuchnode/status", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_error", false);

    let wrong = Server::start(&sim, &[sim.base_url()], "homestack@pve!server=wrong", dir.path()).await?;
    let (status, headers, body) = wrong.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_error(&body, &headers, "pve_auth_failed", false);

    let down = Server::start(&sim, &[closed_port_url()?], PVE_TOKEN, dir.path()).await?;
    let (status, headers, body) = down.get("/v1/pve/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, &headers, "pve_unavailable", true);
    Ok(())
}

#[tokio::test]
async fn the_first_reachable_endpoint_serves_requests() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[closed_port_url()?, sim.base_url()], PVE_TOKEN, dir.path()).await?;
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
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json");
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

#[tokio::test]
async fn image_checks_start_without_a_status_request_and_progress_is_cached() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (status, _, progress) = server.get("/v1/images/check", Some(API_TOKEN)).await?;
            assert_eq!(status, StatusCode::OK);
            if progress["checking"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    assert!(
        sim.ops()
            .iter()
            .any(|op| op.params.get("path").is_some_and(|path| path == "/cluster/resources"))
    );
    sim.clear_ops();
    let (status, _, progress) = server.get("/v1/images/check", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(progress, json!({"checking": false}));
    assert!(sim.ops().is_empty(), "progress requests must only read the cache");
    let (status, _, _) = server.get("/v1/images/check", None).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}
