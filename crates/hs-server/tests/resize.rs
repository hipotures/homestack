//! `workspace.resize` through the API (phase 7): the job, and the LUKS
//! passphrase sent with the apply request (decision 6), which must never
//! appear in a plan, a job or a recorded Proxmox call.

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

const CLUSTER: &str = include_str!("../../hs-core/tests/data/resize-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const PASSPHRASE: &str = "correct horse battery staple";

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
    sim: PveSim,
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
        sim,
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
                .json(&json!({"operation": "workspace.resize", "parameters": parameters})),
        )
        .await
    }

    async fn apply(&self, plan: &Value, secrets: Option<Value>) -> Result<(StatusCode, Value), Error> {
        let id = plan["id"].as_str().ok_or("no plan id")?;
        let mut body = json!({"digest": plan["digest"]});
        if let Some(secrets) = secrets {
            body["secrets"] = secrets;
        }
        send(
            reqwest::Client::new()
                .post(format!("{}/v1/plans/{id}/apply", self.base))
                .header("homestack-config-version", &self.version)
                .json(&body),
        )
        .await
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

    async fn jobs(&self) -> Result<Value, Error> {
        Ok(send(reqwest::Client::new().get(format!("{}/v1/jobs", self.base)))
            .await?
            .1)
    }

    fn recorded(&self) -> String {
        self.sim
            .ops()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tokio::test]
async fn resize_runs_as_a_job_with_its_event() -> TestResult {
    let server = start().await?;
    let (status, plan) = server.plan(json!({"vmid": 200, "home_size": "21G"})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["summary"]["secrets_required"], json!([]));
    let (status, job) = server.apply(&plan, None).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = server.finished(&job).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["event"], "workspace.resized");
    assert_eq!(job["result"]["status"], "resized");
    assert_eq!(job["result"]["disk"], "scsi1");
    let steps: Vec<&str> = job["steps"]
        .as_array()
        .map(|steps| steps.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        steps,
        [
            "Grow the disk in Proxmox",
            "Grow the partition and filesystem in the guest"
        ]
    );
    let kinds: Vec<&str> = job["tasks"]
        .as_array()
        .map(|tasks| tasks.iter().filter_map(|t| t["kind"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(kinds, ["resize"]);
    Ok(())
}

#[tokio::test]
async fn invalid_parameters_and_shrinking_are_refused_when_planning() -> TestResult {
    let server = start().await?;
    for parameters in [
        json!({"vmid": 200}),
        json!({"vmid": 200, "root_size": "32G", "home_size": "32G"}),
        json!({"vmid": 200, "home_size": "21G", "force": true}),
    ] {
        let (status, body) = server.plan(parameters.clone()).await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{parameters}: {body}");
    }
    let (status, body) = server.plan(json!({"vmid": 200, "home_size": "8G"})).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "operation_refused");
    Ok(())
}

#[tokio::test]
async fn the_passphrase_comes_with_apply_and_is_never_kept() -> TestResult {
    let server = start().await?;
    let (status, plan) = server.plan(json!({"vmid": 202, "root_size": "32G"})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    assert_eq!(plan["summary"]["secrets_required"], json!(["luks_passphrase"]));

    let (status, body) = server.apply(&plan, None).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "secret_required");
    let (status, body) = server.apply(&plan, Some(json!({}))).await?;
    assert_eq!(body["error"]["code"], "secret_required", "{status} {body}");
    let (status, body) = server
        .apply(&plan, Some(json!({"luks_passphrase": PASSPHRASE, "other": "x"})))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "validation_failed");
    assert!(!server.recorded().contains("--key-file"), "nothing ran yet");

    let (status, job) = server
        .apply(&plan, Some(json!({"luks_passphrase": PASSPHRASE})))
        .await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = server.finished(&job).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert!(
        server
            .recorded()
            .contains("cryptsetup --batch-mode --key-file - resize cryptroot")
    );
    for (what, text) in [
        ("job", job.to_string()),
        ("jobs", server.jobs().await?.to_string()),
        ("plan", plan.to_string()),
        ("Proxmox calls", server.recorded()),
    ] {
        assert!(!text.contains("horse"), "the passphrase is in the {what}: {text}");
    }
    Ok(())
}

#[tokio::test]
async fn a_plan_without_a_passphrase_refuses_one() -> TestResult {
    let server = start().await?;
    let (status, plan) = server.plan(json!({"vmid": 201, "root_size": "16G"})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan}");
    let (status, body) = server
        .apply(&plan, Some(json!({"luks_passphrase": PASSPHRASE})))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "secret_required");
    assert!(!body.to_string().contains("horse"));
    Ok(())
}

#[tokio::test]
async fn a_wrong_passphrase_fails_the_job_before_any_change() -> TestResult {
    let server = start().await?;
    let (_, plan) = server.plan(json!({"vmid": 202, "root_size": "32G"})).await?;
    let (status, job) = server
        .apply(&plan, Some(json!({"luks_passphrase": "wrong horse"})))
        .await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job = server.finished(&job).await?;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(
        job["error"]["message"],
        "LUKS passphrase verification failed; no disk changes made"
    );
    assert!(!server.recorded().contains("/resize}"));
    assert!(!job.to_string().contains("horse"));
    Ok(())
}
