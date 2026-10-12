//! Behaviour of the simulated Proxmox API, checked over real HTTPS.
//!
//! Error shapes mirror what a live PVE 9.2 cluster returned for the same
//! requests: 401 with an empty body, 403 and 500 as
//! `{"data":null,"message":"...\n"}`.

use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TOKEN: &str = "homestack@pve!server=11111111-2222-3333-4444-555555555555";
const AUDIT_ONLY_VMS: &str = "audit@pve!vms=99999999-2222-3333-4444-555555555555";

fn spec() -> Result<ClusterSpec, toml::de::Error> {
    toml::from_str(
        r#"
        version = { release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }

        [[nodes]]
        name = "node-a"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve #1 SMP PREEMPT_DYNAMIC"
        uptime = 1000

        [[nodes]]
        name = "node-b"
        status = "offline"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve #1 SMP PREEMPT_DYNAMIC"
        uptime = 0

        [[tokens]]
        id = "homestack@pve!server"
        secret = "11111111-2222-3333-4444-555555555555"
        acl = { "/" = ["Sys.Audit", "VM.Audit"] }

        [[tokens]]
        id = "audit@pve!vms"
        secret = "99999999-2222-3333-4444-555555555555"
        acl = { "/vms" = ["VM.Audit"] }

        [[tasks]]
        node = "node-a"
        type = "qmclone"
        id = "101"
        user = "homestack@pve!server"
        log = ["create full clone of drive scsi0", "transferred 1.0 GiB of 2.0 GiB (50.00%)", "transferred 2.0 GiB of 2.0 GiB (100.00%)"]
        polls_until_done = 2

        [[faults]]
        op = "get{path=/version}"
        occurrence = 2
        status = 500
        message = "injected failure"
        "#,
    )
}

fn client(sim: &PveSim) -> Result<reqwest::Client, reqwest::Error> {
    let ca = reqwest::Certificate::from_pem(sim.ca_pem().as_bytes())?;
    reqwest::Client::builder().tls_certs_only([ca]).build()
}

async fn get(sim: &PveSim, token: Option<&str>, path: &str) -> Result<(StatusCode, String), reqwest::Error> {
    let mut request = client(sim)?.get(format!("{}/api2/json{path}", sim.base_url()));
    if let Some(token) = token {
        request = request.header("Authorization", format!("PVEAPIToken={token}"));
    }
    let response = request.send().await?;
    let status = response.status();
    Ok((status, response.text().await?))
}

#[tokio::test]
async fn requests_without_a_valid_token_get_401_with_an_empty_body() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    for token in [
        None,
        Some("homestack@pve!server=wrong"),
        Some("nobody@pve!x=11111111-2222-3333-4444-555555555555"),
    ] {
        let (status, body) = get(&sim, token, "/version").await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert_eq!(body, "");
    }
    assert!(sim.ops().is_empty(), "rejected calls are not operations");
    Ok(())
}

#[tokio::test]
async fn version_and_nodes_come_from_the_scenario() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, Some(TOKEN), "/version").await?;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body)?;
    assert_eq!(
        body,
        json!({"data": {"release": "9.2", "version": "9.2.21", "repoid": "4f6e0ac86f9e8c7f"}})
    );

    let (status, body) = get(&sim, Some(TOKEN), "/nodes").await?;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body)?;
    let nodes: Vec<(&str, &str)> = body["data"]
        .as_array()
        .ok_or("data is not a list")?
        .iter()
        .filter_map(|n| Some((n["node"].as_str()?, n["status"].as_str()?)))
        .collect();
    assert_eq!(nodes, [("node-a", "online"), ("node-b", "offline")]);
    Ok(())
}

#[tokio::test]
async fn node_status_needs_sys_audit_on_the_node() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, Some(AUDIT_ONLY_VMS), "/nodes/node-a/status").await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let body: Value = serde_json::from_str(&body)?;
    assert_eq!(
        body,
        json!({"data": null, "message": "Permission check failed (/nodes/node-a, Sys.Audit)\n"})
    );

    let (status, body) = get(&sim, Some(TOKEN), "/nodes/node-a/status").await?;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&body)?;
    assert_eq!(body["data"]["pveversion"], "pve-manager/9.2.21/4f6e0ac86f9e8c7f");
    assert_eq!(body["data"]["kversion"], "Linux 7.0.14-20-pve #1 SMP PREEMPT_DYNAMIC");
    assert_eq!(body["data"]["uptime"], 1000);
    Ok(())
}

#[tokio::test]
async fn unknown_and_offline_nodes_fail_like_the_proxy_does() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, Some(TOKEN), "/nodes/nosuchnode/status").await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value = serde_json::from_str(&body)?;
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("hostname lookup 'nosuchnode' failed")),
        "{body}"
    );

    let (status, _) = get(&sim, Some(TOKEN), "/nodes/node-b/status").await?;
    assert_eq!(status.as_u16(), 595);
    Ok(())
}

#[tokio::test]
async fn authenticated_calls_are_logged_as_canonical_operations() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    get(&sim, Some(TOKEN), "/version").await?;
    get(&sim, Some(TOKEN), "/nodes/node-a/status").await?;
    get(&sim, Some(AUDIT_ONLY_VMS), "/nodes/node-a/status").await?;
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/version}",
            "get{path=/nodes/node-a/status}",
            "get{path=/nodes/node-a/status}",
        ]
    );
    sim.clear_ops();
    assert!(sim.ops().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_fault_fails_only_the_chosen_occurrence_of_an_operation() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let statuses = [
        get(&sim, Some(TOKEN), "/version").await?.0,
        get(&sim, Some(TOKEN), "/version").await?.0,
        get(&sim, Some(TOKEN), "/version").await?.0,
    ];
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::INTERNAL_SERVER_ERROR, StatusCode::OK]
    );
    Ok(())
}

#[tokio::test]
async fn a_task_runs_until_polled_enough_and_reveals_its_log_progressively() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let upid = sim.task_upids().into_iter().next().ok_or("no task")?;
    assert!(upid.starts_with("UPID:node-a:"), "{upid}");
    assert!(upid.ends_with(":qmclone:101:homestack@pve!server:"), "{upid}");
    let status_path = format!("/nodes/node-a/tasks/{upid}/status");
    let log_path = format!("/nodes/node-a/tasks/{upid}/log");

    let first: Value = serde_json::from_str(&get(&sim, Some(TOKEN), &status_path).await?.1)?;
    assert_eq!(first["data"]["status"], "running");
    assert!(first["data"].get("exitstatus").is_none());
    let partial: Value = serde_json::from_str(&get(&sim, Some(TOKEN), &log_path).await?.1)?;
    assert_eq!(partial["data"].as_array().map(Vec::len), Some(1));

    let second: Value = serde_json::from_str(&get(&sim, Some(TOKEN), &status_path).await?.1)?;
    assert_eq!(second["data"]["status"], "stopped");
    assert_eq!(second["data"]["exitstatus"], "OK");
    let full: Value = serde_json::from_str(&get(&sim, Some(TOKEN), &log_path).await?.1)?;
    assert_eq!(
        full["data"],
        json!([
            {"n": 1, "t": "create full clone of drive scsi0"},
            {"n": 2, "t": "transferred 1.0 GiB of 2.0 GiB (50.00%)"},
            {"n": 3, "t": "transferred 2.0 GiB of 2.0 GiB (100.00%)"},
        ])
    );
    let from_two: Value = serde_json::from_str(&get(&sim, Some(TOKEN), &format!("{log_path}?start=2")).await?.1)?;
    assert_eq!(
        from_two["data"],
        json!([{"n": 3, "t": "transferred 2.0 GiB of 2.0 GiB (100.00%)"}])
    );
    Ok(())
}

#[tokio::test]
async fn unknown_api_paths_are_not_implemented() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, Some(TOKEN), "/cluster/nothing-here").await?;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let body: Value = serde_json::from_str(&body)?;
    // Observed on PVE 9.2: no trailing newline here, unlike 403 and 500.
    assert_eq!(
        body,
        json!({"data": null, "message": "Method 'GET /cluster/nothing-here' not implemented"})
    );
    Ok(())
}

#[test]
fn scenario_typos_are_rejected() {
    let typo = toml::from_str::<ClusterSpec>(
        r#"
        version = { release = "9.2", version = "9.2.21", repoid = "x" }
        [[nodes]]
        name = "node-a"
        pveversoin = "typo"
        kversion = "k"
        uptime = 1
        "#,
    );
    assert!(typo.is_err());
}

/// Shapes observed on PVE 9.2: `{"data":{"<path>":{"<privilege>":1,...}}}`,
/// computed for any path, existing or not. With `path` only that path is
/// returned; without it, every path with an ACL entry.
#[tokio::test]
async fn permissions_are_the_effective_privileges_on_a_path() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, Some(AUDIT_ONLY_VMS), "/access/permissions?path=/vms/100").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body)?,
        json!({"data": {"/vms/100": {"VM.Audit": 1}}})
    );

    let (_, body) = get(&sim, Some(AUDIT_ONLY_VMS), "/access/permissions?path=/nodes").await?;
    assert_eq!(serde_json::from_str::<Value>(&body)?, json!({"data": {"/nodes": {}}}));

    let (_, body) = get(&sim, Some(AUDIT_ONLY_VMS), "/access/permissions").await?;
    assert_eq!(
        serde_json::from_str::<Value>(&body)?,
        json!({"data": {"/vms": {"VM.Audit": 1}}})
    );

    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/access/permissions,query_path=/vms/100}",
            "get{path=/access/permissions,query_path=/nodes}",
            "get{path=/access/permissions}",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn the_all_privileges_wildcard_lists_every_privilege() -> TestResult {
    let mut spec = spec()?;
    spec.tokens[0].acl = [("/".to_owned(), vec!["*".to_owned()])].into_iter().collect();
    let sim = PveSim::start(spec).await?;
    let (_, body) = get(&sim, Some(TOKEN), "/access/permissions?path=/storage/local").await?;
    let body: Value = serde_json::from_str(&body)?;
    let privileges = body["data"]["/storage/local"].as_object().ok_or("no privileges")?;
    // The privileges of the Administrator role on PVE 9.2.21.
    assert_eq!(privileges.len(), 47);
    for name in [
        "Datastore.AllocateSpace",
        "VM.GuestAgent.Unrestricted",
        "Sys.Modify",
        "VM.Snapshot.Rollback",
    ] {
        assert_eq!(privileges.get(name), Some(&json!(1)), "{name}");
    }
    Ok(())
}
