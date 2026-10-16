//! `workspace.create` and `workspace.destroy` through the API (phase 5):
//! plan, apply with the configuration version, the job's steps and result,
//! and the refusals.

mod common;

use std::time::Duration;

use common::{API_TOKEN, Error, TestResult};
use hs_core::lifecycle::LifecycleTiming;
use hs_core::verify::VerifyTimeouts;
use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, AppOptions, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};

const CLUSTER: &str = include_str!("../../hs-core/tests/data/lifecycle-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";

const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
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
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "developer"
uid = 1000
gid = 1000
"#;

struct Running {
    _sim: PveSim,
    base: String,
    version: String,
    _dir: tempfile::TempDir,
}

async fn start() -> Result<Running, Error> {
    start_with(CLUSTER, SERVER_TOML).await
}

async fn start_with(cluster: &str, server_toml: &str) -> Result<Running, Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(cluster)?).await?;
    let dir = tempfile::tempdir()?;
    let mut pve = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let timing = LifecycleTiming {
        poll: Duration::from_millis(5),
        verify: VerifyTimeouts {
            agent: Duration::from_secs(5),
            cloud_init: Duration::from_secs(5),
        },
    };
    let app = hs_server::app_with(
        ServerConfig {
            api_token: ApiToken::parse(API_TOKEN)?.into(),
            pve: pve.into(),
            config_file: dir.path().join("server.toml"),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools::default(),
        },
        AppOptions {
            operations: hs_server::lifecycle::operations(timing),
            ..AppOptions::default()
        },
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    let upload = client
        .put(format!("{base}/v1/config"))
        .bearer_auth(API_TOKEN)
        .header("if-none-match", "*")
        .body(server_toml.to_owned())
        .send()
        .await?;
    assert_eq!(upload.status(), StatusCode::OK);
    let version = hs_model::config::config_version(server_toml);
    Ok(Running {
        _sim: sim,
        base,
        version,
        _dir: dir,
    })
}

async fn send(request: reqwest::RequestBuilder) -> Result<(StatusCode, Value), Error> {
    let response = request.bearer_auth(API_TOKEN).send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

impl Running {
    async fn plan(&self, operation: &str, parameters: Value) -> Result<(StatusCode, Value), Error> {
        send(
            reqwest::Client::new()
                .post(format!("{}/v1/plans", self.base))
                .json(&json!({"operation": operation, "parameters": parameters})),
        )
        .await
    }

    async fn apply(&self, plan: &Value, version: Option<&str>) -> Result<(StatusCode, Value), Error> {
        let id = plan["id"].as_str().ok_or("no plan id")?;
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/plans/{id}/apply", self.base))
            .json(&json!({"digest": plan["digest"]}));
        if let Some(version) = version {
            request = request.header("homestack-config-version", version);
        }
        send(request).await
    }

    async fn finished(&self, job: &Value) -> Result<Value, Error> {
        let id = job["id"].as_str().ok_or("no job id")?;
        for _ in 0..1000 {
            let (_, job) = send(reqwest::Client::new().get(format!("{}/v1/jobs/{id}", self.base))).await?;
            if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
                return Ok(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err("job did not finish".into())
    }

    async fn run(&self, operation: &str, parameters: Value) -> Result<Value, Error> {
        let (status, plan) = self.plan(operation, parameters).await?;
        assert_eq!(status, StatusCode::CREATED, "{plan}");
        let (status, job) = self.apply(&plan, Some(&self.version)).await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
        self.finished(&job).await
    }
}

#[tokio::test]
async fn create_and_destroy_run_as_jobs_with_their_events() -> TestResult {
    let server = start().await?;
    let job = server
        .run("workspace.create", json!({"vmid": 240, "name": "hst-api"}))
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["event"], "workspace.created");
    assert_eq!(job["result"]["ip"], "192.0.2.240");
    assert_eq!(job["result"]["user"], "developer");
    assert_eq!(job["result"]["host_key_changed"], true);
    let steps: Vec<&str> = job["steps"]
        .as_array()
        .map(|steps| steps.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        steps,
        [
            "SSH keys",
            "Full clone",
            "Persistent home disk",
            "Clone identity",
            "Cloud-Init",
            "Boot",
            "Verify workspace"
        ]
    );
    let kinds: Vec<&str> = job["tasks"]
        .as_array()
        .map(|tasks| tasks.iter().filter_map(|t| t["kind"].as_str()).collect())
        .unwrap_or_default();
    assert!(kinds.contains(&"qmclone") && kinds.contains(&"qmstart"), "{kinds:?}");

    let job = server.run("workspace.destroy", json!({"vmid": 240})).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["event"], "workspace.destroyed");
    assert_eq!(job["result"]["status"], "absent");
    Ok(())
}

/// `create` with `node` (phase 10, decision 3 = A): the job clones on
/// Gold's node, migrates the clone and finishes on the target.
#[tokio::test]
async fn create_on_another_node_clones_then_migrates() -> TestResult {
    let cluster = include_str!("../../hs-core/tests/data/create-transfer-cluster.toml");
    let server_toml = SERVER_TOML.replace(
        "homestack-storage-1 = [\"local-lvm\"]",
        "homestack-storage-1 = [\"local-lvm\"]\nhomestack-storage-2 = [\"fast\"]",
    );
    let server = start_with(cluster, &server_toml).await?;
    let job = server
        .run(
            "workspace.create",
            json!({"vmid": 240, "name": "hst-api", "node": "pve-example-2"}),
        )
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["node"], "pve-example-2");
    assert_eq!(job["result"]["source_node"], "pve-example-1");
    assert_eq!(job["result"]["transfer_method"], "clone-migrate");
    assert_eq!(job["result"]["root_storage"], "fast");
    let kinds: Vec<&str> = job["tasks"]
        .as_array()
        .map(|tasks| tasks.iter().filter_map(|t| t["kind"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(kinds.first(), Some(&"qmclone"), "{kinds:?}");
    assert_eq!(kinds.get(1), Some(&"qmigrate"), "{kinds:?}");
    assert!(kinds.contains(&"qmstart"), "{kinds:?}");

    let (status, plan) = server
        .plan(
            "workspace.create",
            json!({"vmid": 241, "name": "hst-off", "node": "pve-example-3"}),
        )
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{plan}");
    Ok(())
}

/// `pool` names the server's pool or the test pool; no other.
#[tokio::test]
async fn create_goes_to_the_servers_pool_or_the_test_pool() -> TestResult {
    let server = start().await?;
    let (status, plan) = server
        .plan(
            "workspace.create",
            json!({"vmid": 240, "name": "hst-api", "pool": "homestack"}),
        )
        .await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["summary"]["pool"], "homestack", "{plan}");

    // The simulated cluster has no test pool: the plan is refused for it.
    let (status, body) = server
        .plan(
            "workspace.create",
            json!({"vmid": 241, "name": "hst-test", "pool": "homestack-test"}),
        )
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string().contains("pool homestack-test does not exist"),
        "{body}"
    );

    let (status, body) = server
        .plan(
            "workspace.create",
            json!({"vmid": 242, "name": "hst-gold", "pool": "homestack-gold"}),
        )
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    Ok(())
}

#[tokio::test]
async fn apply_needs_the_configuration_version() -> TestResult {
    let server = start().await?;
    let (status, plan) = server
        .plan("workspace.create", json!({"vmid": 240, "name": "hst-api"}))
        .await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let (status, body) = server.apply(&plan, None).await?;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{body}");
    let (status, body) = server.apply(&plan, Some("0000")).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "config_version_mismatch");
    Ok(())
}

#[tokio::test]
async fn refusals_have_their_own_codes() -> TestResult {
    let server = start().await?;
    let (status, body) = server.plan("workspace.destroy", json!({"vmid": 150})).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "outside_pool");

    let (status, body) = server
        .plan("workspace.create", json!({"vmid": 150, "name": "dup"}))
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "operation_refused");
    assert_eq!(body["error"]["message"], "VMID 150 already exists");

    let (status, body) = server
        .plan("workspace.create", json!({"vmid": 240, "name": "9bad"}))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = server.plan("workspace.destroy", json!({"vmid": 999})).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = server
        .plan("workspace.create", json!({"vmid": 240, "name": "x", "extra": 1}))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    Ok(())
}

/// The server reads only the VMs of its pools: a workspace outside them is
/// refused even when the token could see it.
#[tokio::test]
async fn reads_of_vms_outside_the_pools_are_refused() -> TestResult {
    let server = start().await?;
    for path in [
        "/v1/workspaces/150",
        "/v1/gold/readiness?vmid=150",
        "/v1/networks?gold=150",
    ] {
        let (status, body) = send(reqwest::Client::new().get(format!("{}{path}", server.base))).await?;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert_eq!(body["error"]["code"], "outside_pool", "{path}");
    }
    let (status, body) = send(reqwest::Client::new().get(format!("{}/v1/workspaces/160", server.base))).await?;
    assert_ne!(status, StatusCode::CONFLICT, "VM 160 is in the pool: {body}");
    Ok(())
}

/// `workspace.refresh` through the API: the job's steps and result, and
/// its refusals.
#[tokio::test]
async fn refresh_runs_as_a_job_with_its_steps() -> TestResult {
    let server = start().await?;
    let job = server
        .run("workspace.create", json!({"vmid": 240, "name": "hst-api"}))
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");

    let (status, body) = server
        .plan("workspace.refresh", json!({"vmid": 240, "force": true}))
        .await?;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "force needs a journal: {body}"
    );
    let (status, body) = server
        .plan("workspace.refresh", json!({"vmid": 240, "extra": 1}))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = server.plan("workspace.refresh", json!({"vmid": 150})).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "outside_pool");

    let (status, plan) = server.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["summary"]["mode"], "replace", "{plan}");
    let (status, job) = server.apply(&plan, Some(&server.version)).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = server.finished(&job).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["event"], "workspace.refreshed");
    assert_eq!(job["result"]["status"], "running");
    assert_eq!(job["result"]["host_key_changed"], true);
    let steps: Vec<&str> = job["steps"]
        .as_array()
        .map(|steps| steps.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        steps,
        [
            "Import Gold root",
            "Stop workspace",
            "Switch root disks",
            "Cloud-Init",
            "Boot verification",
            "Restore power state",
            "Delete previous root"
        ]
    );
    Ok(())
}
