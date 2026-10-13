//! `workspace.migrate` through the API (phase 8): the job with its steps
//! and tasks, the refusals when planning, and a failed migration that
//! starts the VM again on its node.

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

const CLUSTER: &str = include_str!("../../hs-core/tests/data/migrate-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";

const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
homestack-storage-2 = ["local-lvm", "fast"]
homestack-storage-3 = ["local-lvm"]
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
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
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
            api_token: ApiToken::parse(API_TOKEN)?,
            pve,
            config_file: dir.path().join("server.toml"),
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
    let upload = reqwest::Client::new()
        .put(format!("{base}/v1/config"))
        .bearer_auth(API_TOKEN)
        .header("if-none-match", "*")
        .body(SERVER_TOML)
        .send()
        .await?;
    assert_eq!(upload.status(), StatusCode::OK);
    Ok(Running {
        _sim: sim,
        base,
        version: hs_model::config::config_version(SERVER_TOML),
        _dir: dir,
    })
}

async fn send(request: reqwest::RequestBuilder) -> Result<(StatusCode, Value), Error> {
    let response = request.bearer_auth(API_TOKEN).send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

impl Running {
    async fn plan(&self, parameters: Value) -> Result<(StatusCode, Value), Error> {
        send(
            reqwest::Client::new()
                .post(format!("{}/v1/plans", self.base))
                .json(&json!({"operation": "workspace.migrate", "parameters": parameters})),
        )
        .await
    }

    async fn run(&self, parameters: Value) -> Result<Value, Error> {
        let (status, plan) = self.plan(parameters).await?;
        assert_eq!(status, StatusCode::CREATED, "{plan}");
        let id = plan["id"].as_str().ok_or("no plan id")?;
        let (status, job) = send(
            reqwest::Client::new()
                .post(format!("{}/v1/plans/{id}/apply", self.base))
                .header("homestack-config-version", &self.version)
                .json(&json!({"digest": plan["digest"]})),
        )
        .await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
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
}

fn names<'a>(job: &'a Value, list: &str, key: &str) -> Vec<&'a str> {
    job[list]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item[key].as_str()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn migrate_runs_as_a_job_with_its_event() -> TestResult {
    let server = start().await?;
    let job = server.run(json!({"vmid": 200, "target_node": "pve-example-2"})).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["event"], "workspace.migrated");
    assert_eq!(job["result"]["status"], "running");
    assert_eq!(job["result"]["target_node"], "pve-example-2");
    assert_eq!(
        names(&job, "steps", "name"),
        [
            "Shutdown workspace",
            "Migrate local disks",
            "Verify target configuration",
            "Start workspace",
            "Verify guest and persistent home",
            "Verify final power state"
        ]
    );
    assert_eq!(names(&job, "tasks", "kind"), ["qmshutdown", "qmigrate", "qmstart"]);
    Ok(())
}

#[tokio::test]
async fn invalid_parameters_and_refusals_when_planning() -> TestResult {
    let server = start().await?;
    for parameters in [
        json!({"vmid": 200}),
        json!({"vmid": 200, "target_node": "pve-example-2", "online": true}),
        json!({"vmid": 200, "target_node": "../pve-example-2"}),
        json!({"vmid": 200, "target_node": "pve-example-2", "target_storage": "local"}),
    ] {
        let (status, body) = server.plan(parameters.clone()).await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{parameters}: {body}");
    }
    let (status, body) = server
        .plan(json!({"vmid": 200, "target_node": "pve-example-1"}))
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "operation_refused");
    let (status, body) = server
        .plan(json!({"vmid": 999, "target_node": "pve-example-2"}))
        .await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

#[tokio::test]
async fn a_failed_migration_restarts_the_vm_and_fails_the_job() -> TestResult {
    let server = start().await?;
    let job = server.run(json!({"vmid": 203, "target_node": "pve-example-2"})).await?;
    assert_eq!(job["state"], "failed", "{job}");
    let message = job["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.ends_with("\nRecovery: VM 203 was restarted automatically on pve-example-1."),
        "{message}"
    );
    assert_eq!(names(&job, "tasks", "kind"), ["qmshutdown", "qmigrate", "qmstart"]);
    Ok(())
}
