//! The HTTP API in-process, against the Proxmox simulator.

mod common;

use common::{PVE_TOKEN, Server, TestResult, assert_error, spec};
use pve_sim::PveSim;
use reqwest::StatusCode;

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
