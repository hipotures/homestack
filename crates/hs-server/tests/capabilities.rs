//! `GET /v1/capabilities`: which operations the server's Proxmox token can
//! perform with the uploaded configuration, from `GET /access/permissions`
//! (mapping T3 and 8.5).

mod common;

use common::{API_TOKEN, PVE_TOKEN, SERVER_TOML, Server, TestResult, assert_error, spec};
use hs_model::config::config_version;
use pve_sim::PveSim;
use reqwest::StatusCode;
use serde_json::{Value, json};

const AUDITOR: &str =
    r#""/" = ["Sys.Audit", "VM.Audit", "Datastore.Audit", "VM.GuestAgent.Audit", "Pool.Audit", "SDN.Audit"]"#;

async fn configured(acl: &str, dir: &std::path::Path) -> Result<(PveSim, Server), common::Error> {
    let sim = PveSim::start(spec(acl)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir).await?;
    let reply = server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    Ok((sim, server))
}

fn operation<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["operations"]
        .as_array()
        .and_then(|ops| ops.iter().find(|op| op["operation"] == id))
        .unwrap_or(&Value::Null)
}

fn privilege(path: &str, privilege: &str) -> Value {
    json!({"path": path, "privilege": privilege})
}

#[tokio::test]
async fn capabilities_need_a_configuration() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = PveSim::start(spec(AUDITOR)?).await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let (status, headers, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, &headers, "config_missing", false);
    assert!(sim.ops().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_read_only_token_can_read_but_not_change_anything() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (sim, server) = configured(AUDITOR, dir.path()).await?;
    let (status, _, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["token_id"], "homestack@pve!server");
    assert_eq!(body["config_version"], config_version(SERVER_TOML));
    assert!(!body.to_string().contains("11111111-2222"), "the token secret leaked");

    let ids: Vec<&str> = body["operations"]
        .as_array()
        .map(|ops| ops.iter().filter_map(|op| op["operation"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        ids,
        [
            "read",
            "orphan_scan",
            "guest_read",
            "gold_readiness",
            "create",
            "refresh",
            "resize",
            "migrate",
            "destroy",
            "addkey"
        ]
    );
    for id in ["read", "guest_read", "gold_readiness"] {
        assert_eq!(operation(&body, id)["available"], true, "{id}: {body}");
        assert_eq!(operation(&body, id)["missing"], json!([]), "{id}");
    }
    assert_eq!(
        operation(&body, "orphan_scan")["missing"],
        json!([privilege("/vms", "VM.Config.Disk")]),
        "a read-only token cannot list VM volumes"
    );
    let read = &operation(&body, "read")["required"];
    for storage in ["fast", "bulk", "other"] {
        assert!(
            read.as_array()
                .is_some_and(|r| r.contains(&privilege(&format!("/storage/{storage}"), "Datastore.Audit"))),
            "{read}"
        );
    }

    let create = operation(&body, "create");
    assert_eq!(create["available"], false);
    let missing = create["missing"].as_array().cloned().unwrap_or_default();
    for expected in [
        privilege("/vms", "VM.Allocate"),
        privilege("/vms/100", "VM.Clone"),
        privilege("/storage/fast", "Datastore.AllocateSpace"),
        privilege("/storage/snippets", "Datastore.AllocateSpace"),
        privilege("/vms", "VM.GuestAgent.Unrestricted"),
    ] {
        assert!(missing.contains(&expected), "{expected} not in {missing:?}");
    }
    assert!(
        !missing.contains(&privilege("/storage/other", "Datastore.AllocateSpace")),
        "create only allocates on the Gold node's layout: {missing:?}"
    );
    for id in ["refresh", "resize", "migrate", "destroy", "addkey"] {
        assert_eq!(operation(&body, id)["available"], false, "{id}");
    }

    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/access/permissions,query_path=/nodes}",
            "get{path=/access/permissions,query_path=/storage/bulk}",
            "get{path=/access/permissions,query_path=/storage/fast}",
            "get{path=/access/permissions,query_path=/storage/other}",
            "get{path=/access/permissions,query_path=/storage/snippets}",
            "get{path=/access/permissions,query_path=/vms}",
            "get{path=/access/permissions,query_path=/vms/100}",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_full_token_can_do_everything() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (_sim, server) = configured(r#""/" = ["*"]"#, dir.path()).await?;
    let (status, _, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    for op in body["operations"].as_array().ok_or("no operations")? {
        assert_eq!(op["available"], true, "{op}");
        assert_eq!(op["missing"], json!([]), "{op}");
        assert!(op["required"].as_array().is_some_and(|r| !r.is_empty()), "{op}");
    }
    Ok(())
}

#[tokio::test]
async fn privileges_on_one_path_do_not_cover_another() -> TestResult {
    let dir = tempfile::tempdir()?;
    let acl = r#""/nodes" = ["Sys.Audit"], "/vms" = ["VM.Audit"], "/storage/fast" = ["Datastore.Audit"]"#;
    let (_sim, server) = configured(acl, dir.path()).await?;
    let (_, _, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    let read = operation(&body, "read");
    assert_eq!(read["available"], false);
    assert_eq!(
        read["missing"],
        json!([
            privilege("/storage/bulk", "Datastore.Audit"),
            privilege("/storage/other", "Datastore.Audit"),
        ])
    );
    Ok(())
}

#[tokio::test]
async fn proxmox_failures_use_the_usual_error_codes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (sim, _server) = configured(AUDITOR, dir.path()).await?;
    let down = Server::start(&sim, &[common::closed_port_url()?], PVE_TOKEN, dir.path()).await?;
    let (status, headers, body) = down.get("/v1/capabilities", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_error(&body, &headers, "pve_unavailable", true);
    Ok(())
}
