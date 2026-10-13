//! `GET /v1/capabilities`: which operations the server's Proxmox token can
//! perform with the uploaded configuration, from `GET /access/permissions`
//! (mapping T3 and 8.5), on the instance's pool (phase 4b), and what of the
//! installation (`deploy/pve/homestack-pve.sh`) the token can see.

mod common;

use common::{API_TOKEN, PVE_TOKEN, SERVER_TOML, Server, TestResult, assert_error, spec};
use hs_model::config::config_version;
use pve_sim::PveSim;
use pve_sim::scenario::{PoolSpec, VmSpec};
use reqwest::StatusCode;
use serde_json::{Value, json};

const AUDITOR: &str =
    r#""/" = ["Sys.Audit", "VM.Audit", "Datastore.Audit", "VM.GuestAgent.Audit", "Pool.Audit", "SDN.Audit"]"#;

async fn configured(acl: &str, dir: &std::path::Path) -> Result<(PveSim, Server), common::Error> {
    configured_with(acl, &[], dir).await
}

/// Gold (VM 100) exists; `pools` are (poolid, VMIDs).
async fn configured_with(
    acl: &str,
    pools: &[(&str, &[u32])],
    dir: &std::path::Path,
) -> Result<(PveSim, Server), common::Error> {
    let mut cluster = spec(acl)?;
    cluster.vms.push(toml::from_str::<VmSpec>(
        r#"vmid = 100
node = "node-a"
config = { name = "gold", tags = "homestack-gold" }"#,
    )?);
    cluster.pools = pools
        .iter()
        .map(|(poolid, vms)| PoolSpec {
            poolid: (*poolid).to_owned(),
            comment: String::new(),
            vms: vms.to_vec(),
        })
        .collect();
    let sim = PveSim::start(cluster).await?;
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
        json!([privilege("/pool/homestack", "VM.Config.Disk")]),
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
        privilege("/pool/homestack", "VM.Allocate"),
        privilege("/vms/100", "VM.Clone"),
        privilege("/storage/fast", "Datastore.AllocateSpace"),
        privilege("/storage/snippets", "Datastore.AllocateSpace"),
        privilege("/pool/homestack", "VM.GuestAgent.FileRead"),
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

    let installation = &body["installation"];
    assert_eq!(installation["pool"], "homestack");
    assert_eq!(installation["gold_pool"], "homestack-gold");
    assert_eq!(installation["complete"], false);
    assert_eq!(
        installation["checks"].as_array().map(|checks| checks
            .iter()
            .map(|c| (c["item"].clone(), c["status"].clone()))
            .collect::<Vec<_>>()),
        Some(vec![
            (json!("pool"), json!("missing")),
            (json!("gold_pool"), json!("missing")),
            (json!("gold_in_pool"), json!("missing")),
            (json!("snippets"), json!("unknown")),
        ])
    );

    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/access/permissions,query_path=/nodes}",
            "get{path=/access/permissions,query_path=/pool/homestack}",
            "get{path=/access/permissions,query_path=/storage/bulk}",
            "get{path=/access/permissions,query_path=/storage/fast}",
            "get{path=/access/permissions,query_path=/storage/other}",
            "get{path=/access/permissions,query_path=/storage/snippets}",
            "get{path=/access/permissions,query_path=/vms}",
            "get{path=/access/permissions,query_path=/vms/100}",
            "get{path=/pools}",
        ]
    );
    Ok(())
}

/// After `homestack-pve.sh install`: the pools exist, Gold is in its pool and
/// the roles on the pools give everything. Gold's privileges come from its
/// pool, which Proxmox adds to `/vms/<gold>`.
#[tokio::test]
async fn an_installed_pool_token_can_do_everything() -> TestResult {
    let dir = tempfile::tempdir()?;
    let workspace = r#"["Pool.Audit", "VM.Allocate", "VM.Audit", "VM.Config.Cloudinit", "VM.Config.Disk", "VM.Config.Options", "VM.GuestAgent.Audit", "VM.GuestAgent.FileRead", "VM.GuestAgent.Unrestricted", "VM.Migrate", "VM.PowerMgmt"]"#;
    let gold = r#"["Pool.Audit", "VM.Audit", "VM.Clone", "VM.GuestAgent.Audit", "VM.GuestAgent.FileRead", "VM.GuestAgent.Unrestricted"]"#;
    let storage = r#"["Datastore.AllocateSpace", "Datastore.Audit"]"#;
    let acl = format!(
        r#""/" = ["Sys.Audit", "VM.Audit", "Datastore.Audit", "VM.GuestAgent.Audit", "Pool.Audit", "SDN.Audit"],
        "/pool/homestack" = {workspace}, "/pool/homestack-gold" = {gold},
        "/storage/fast" = {storage}, "/storage/bulk" = {storage}, "/storage/other" = {storage},
        "/storage/snippets" = {storage}"#
    );
    let (_sim, server) = configured_with(
        &acl,
        &[("homestack", &[]), ("homestack-gold", &[100]), ("someone-else", &[])],
        dir.path(),
    )
    .await?;
    let (status, _, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    for op in body["operations"].as_array().ok_or("no operations")? {
        assert_eq!(op["available"], true, "{op}");
    }
    let installation = &body["installation"];
    assert_eq!(installation["complete"], true, "{installation}");
    let statuses: Vec<&str> = installation["checks"]
        .as_array()
        .map(|checks| checks.iter().filter_map(|c| c["status"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(statuses, ["ok", "ok", "ok", "unknown"]);
    Ok(())
}

#[tokio::test]
async fn gold_outside_its_pool_is_reported() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (_sim, server) = configured_with(AUDITOR, &[("homestack", &[]), ("homestack-gold", &[])], dir.path()).await?;
    let (_, _, body) = server.get("/v1/capabilities", Some(API_TOKEN)).await?;
    let checks = body["installation"]["checks"].as_array().cloned().unwrap_or_default();
    assert_eq!(checks[0]["status"], "ok");
    assert_eq!(checks[1]["status"], "ok");
    assert_eq!(checks[2]["status"], "missing");
    assert!(
        checks[2]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("100") && d.contains("homestack-gold")),
        "{checks:?}"
    );
    assert_eq!(body["installation"]["complete"], false);
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
