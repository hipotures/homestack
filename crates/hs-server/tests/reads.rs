//! The read routes of phase 2 against the simulated cluster of
//! `scenarios/status-cluster.toml`. Field-by-field agreement with Python is
//! the parity test's job (`crates/hs-parity/tests/scenarios.rs`); this file
//! checks the API around it.

mod common;

use std::path::Path;

use common::{API_TOKEN, Server, TestResult, assert_error};
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};

const SERVER_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const AUDITOR_TOKEN: &str = "auditor@pve!server=00000000-0000-0000-0000-00000000000c";

fn repo() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// The scenario's cluster, plus a token with exactly PVEAuditor on `/`.
fn cluster() -> Result<ClusterSpec, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(repo().join("scenarios/status-cluster.toml"))?;
    let mut spec = ClusterSpec::from_scenario(&text)?;
    let auditor: pve_sim::scenario::TokenSpec = toml::from_str(
        r#"
        id = "auditor@pve!server"
        secret = "00000000-0000-0000-0000-00000000000c"
        acl = { "/" = ["Datastore.Audit", "Mapping.Audit", "Pool.Audit", "SDN.Audit", "Sys.Audit", "VM.Audit", "VM.GuestAgent.Audit"] }
        "#,
    )?;
    spec.tokens.push(auditor);
    Ok(spec)
}

/// A server with the server part of `python/config.example.toml`.
async fn configured(sim: &PveSim, token: &str, dir: &Path) -> Result<Server, common::Error> {
    let python = std::fs::read_to_string(repo().join("python/config.example.toml"))?;
    std::fs::write(dir.join("server.toml"), hs_config::split::split(&python)?.server_toml)?;
    Server::start(sim, &[sim.base_url()], token, dir).await
}

async fn get(server: &Server, path: &str) -> Result<(StatusCode, Value, reqwest::header::HeaderMap), reqwest::Error> {
    let (status, headers, body) = server.get(path, Some(API_TOKEN)).await?;
    Ok((status, body, headers))
}

#[tokio::test]
async fn global_status_reports_the_cluster() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(cluster()?).await?;
    let server = configured(&sim, SERVER_TOKEN, dir.path()).await?;
    let (status, body, _) = get(&server, "/v1/status").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["gold"]["vmid"], 100);
    let vmids: Vec<u64> = body["workspaces"]
        .as_array()
        .ok_or("no list")?
        .iter()
        .filter_map(|w| w["vmid"].as_u64())
        .collect();
    assert_eq!(vmids, [200, 201, 202, 203, 204, 205, 206]);
    assert_eq!(
        body["warnings"],
        json!([
            "Workspace VM 203: missing scsi1.",
            "Workspace VM 205: scsi1 serial 'HS_HOME_299' != 'HS_HOME_205'.",
            "Detached HomeStack volumes: example-storage:vm-209-hs-home-developer, example-storage:vm-210-hs-root-default",
        ])
    );
    assert_eq!(body["summary"]["homes"]["used_bytes"], 11_436_277_760_u64);
    assert!(body.get("ok").is_none() && body.get("execution").is_none());
    Ok(())
}

#[tokio::test]
async fn a_token_that_cannot_list_vm_volumes_makes_the_orphan_scan_incomplete() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(cluster()?).await?;
    let server = configured(&sim, AUDITOR_TOKEN, dir.path()).await?;
    let (status, body, _) = get(&server, "/v1/orphans").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["volumes"], json!([]));
    assert_eq!(body["complete"], false);
    assert_eq!(
        body["warnings"],
        json!([
            "The server's Proxmox token cannot list VM volumes on example-storage (pve-example-1): \
             it needs VM.Config.Disk on /vms or Datastore.Allocate on /storage/example-storage"
        ])
    );
    let (_, body, _) = get(&server, "/v1/status").await?;
    assert_eq!(body["summary"]["orphan_scan_complete"], false);
    assert!(
        body["warnings"].as_array().is_some_and(|w| w.iter().any(|w| w
            .as_str()
            .is_some_and(|w| w.starts_with("HomeStack orphan scan incomplete: The server's Proxmox token")))),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn workspace_targets_are_vmids_or_names() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(cluster()?).await?;
    let server = configured(&sim, SERVER_TOKEN, dir.path()).await?;
    let (status, by_id, _) = get(&server, "/v1/workspaces/200").await?;
    assert_eq!(status, StatusCode::OK, "{by_id}");
    let (_, by_name, _) = get(&server, "/v1/workspaces/test1").await?;
    assert_eq!(by_id, by_name);
    assert_eq!(by_id["home_mount"], "/dev/sdb ext4 /home/developer");
    assert_eq!(by_id["home_mount_ok"], true);
    assert!(by_id.get("mounted_home_label").is_none());

    for (path, status, code) in [
        ("/v1/workspaces/desktop", StatusCode::NOT_FOUND, "not_found"),
        ("/v1/workspaces/nosuch", StatusCode::NOT_FOUND, "not_found"),
        ("/v1/workspaces/999", StatusCode::NOT_FOUND, "not_found"),
        (
            "/v1/workspaces/1bad",
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
    ] {
        let (actual, body, headers) = get(&server, path).await?;
        assert_eq!(actual, status, "{path}: {body}");
        assert_error(&body, &headers, code, false);
        assert!(
            !body.to_string().contains("nosuch") && !body.to_string().contains("desktop"),
            "input echoed: {body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn storage_discovery_networks_and_gold() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(cluster()?).await?;
    let server = configured(&sim, SERVER_TOKEN, dir.path()).await?;

    let (_, storage, _) = get(&server, "/v1/storage").await?;
    assert_eq!(storage.as_array().map(Vec::len), Some(2));
    assert_eq!(storage[1]["node_status"], "offline");

    let (_, discovery, _) = get(&server, "/v1/discovery").await?;
    assert_eq!(discovery["gold_candidates"][0]["vmid"], 100);

    let (_, networks, _) = get(&server, "/v1/networks").await?;
    assert_eq!(networks[0]["cidr"], "192.0.2.0/24");

    let (_, gold, _) = get(&server, "/v1/gold/readiness").await?;
    assert_eq!((gold["vmid"].as_u64(), gold["ok"].as_bool()), (Some(100), Some(true)));
    let (status, body, headers) = get(&server, "/v1/gold/readiness?vmid=200").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false, "a workspace is no Gold");
    let (status, body, _) = get(&server, "/v1/gold/readiness?vmid=999").await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body, _) = get(&server, "/v1/gold/readiness?vmid=x").await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    drop(headers);
    Ok(())
}

#[tokio::test]
async fn reads_that_need_the_configuration_say_so() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(cluster()?).await?;
    let server = Server::start(&sim, &[sim.base_url()], SERVER_TOKEN, dir.path()).await?;
    for path in [
        "/v1/status",
        "/v1/workspaces/200",
        "/v1/storage",
        "/v1/orphans",
        "/v1/networks",
        "/v1/gold/readiness",
    ] {
        let (status, body, headers) = get(&server, path).await?;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert_error(&body, &headers, "config_missing", false);
    }
    let (status, body, _) = get(&server, "/v1/discovery").await?;
    assert_eq!(status, StatusCode::OK, "discovery needs no configuration: {body}");
    let (status, body, _) = get(&server, "/v1/gold/readiness?vmid=100").await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "an explicit Gold needs no configuration: {body}"
    );
    let (status, body, _) = get(&server, "/v1/networks?gold=100").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}
