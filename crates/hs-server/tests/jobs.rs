//! The operation model (mapping 8.4): a plan with an ID and a digest, a new
//! plan before execution, `202` and `GET /v1/jobs/{id}`, and the job's
//! WebSocket stream with a snapshot, changes and heartbeats.
//!
//! Phase 3 has no HomeStack operation yet; the tests register one that
//! starts and shuts down a VM, the steps every lifecycle operation uses.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{API_TOKEN, Error, TestResult, assert_error};
use hs_core::power::{shutdown_vm, start_vm};
use hs_core::tasks::TaskWait;
use hs_pve::{PveClientConfig, PveToken};
use hs_server::operations::{BoxFuture, JobHandle, OpContext, OpFailure, Operation, Planned};
use hs_server::{ApiToken, AppOptions, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const PVE_TOKEN: &str = "homestack@pve!test=00000000-0000-0000-0000-00000000000d";

const CLUSTER: &str = r#"
[pve]
version = { release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }

[[pve.nodes]]
name = "pve-example-1"
pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
kversion = "Linux 7.0.14-20-pve"
uptime = 1

[[pve.vms]]
vmid = 240
node = "pve-example-1"
status = "stopped"
config = { name = "hst-cycle" }

[[pve.vms]]
vmid = 241
node = "pve-example-1"
status = "running"
acpi = false
config = { name = "hst-stuck" }

[[pve.tokens]]
id = "homestack@pve!test"
secret = "00000000-0000-0000-0000-00000000000d"
acl = { "/" = ["Sys.Audit", "VM.Audit", "VM.Config.Options", "VM.PowerMgmt"] }
"#;

/// Start a VM and shut it down again: `{"vmid": N, "pause_ms": M}`, with
/// an optional pause before the first step.
struct PowerCycle;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    vmid: u32,
    #[serde(default)]
    pause_ms: u64,
}

fn wait() -> TaskWait {
    TaskWait {
        poll: Duration::from_millis(5),
        timeout: Duration::from_secs(10),
    }
}

impl Operation for PowerCycle {
    fn name(&self) -> &'static str {
        "test.power-cycle"
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let parameters: Parameters =
                serde_json::from_value(parameters.clone()).map_err(|_| OpFailure::invalid("Invalid parameters."))?;
            let status = hs_core::inventory::vm_status(ctx.pve.as_ref(), "pve-example-1", parameters.vmid).await;
            Ok(Planned {
                targets: vec![parameters.vmid],
                summary: json!({ "vmid": parameters.vmid, "status": status, "pause_ms": parameters.pause_ms }),
            })
        })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a OpContext,
        planned: &'a Planned,
        job: &'a JobHandle,
    ) -> BoxFuture<'a, Result<Value, OpFailure>> {
        Box::pin(async move {
            let vmid = planned.targets[0];
            let pause = planned.summary["pause_ms"].as_u64().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(pause)).await;
            job.step("Start", async {
                start_vm(ctx.pve.as_ref(), "pve-example-1", vmid, &wait(), job).await
            })
            .await?;
            job.step("Shut down", async {
                shutdown_vm(ctx.pve.as_ref(), "pve-example-1", vmid, 90, &wait(), job).await
            })
            .await?;
            Ok(json!({ "vmid": vmid, "status": "stopped" }))
        })
    }
}

struct Running {
    sim: PveSim,
    base: String,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(heartbeat: Duration) -> Result<Running, Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let dir = tempfile::tempdir()?;
    let mut pve = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let options = AppOptions {
        operations: vec![Arc::new(PowerCycle)],
        heartbeat,
        ..AppOptions::default()
    };
    let app = hs_server::app_with(
        ServerConfig {
            api_token: ApiToken::parse(API_TOKEN)?.into(),
            pve: pve.into(),
            config_file: dir.path().join("server.toml"),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools::default(),
        },
        options,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Running {
        sim,
        base: format!("http://{addr}"),
        _dir: dir,
        task,
    })
}

async fn post(base: &str, path: &str, body: &Value) -> Result<(StatusCode, reqwest::header::HeaderMap, Value), Error> {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(API_TOKEN)
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.json().await.unwrap_or(Value::Null);
    Ok((status, headers, body))
}

async fn get(base: &str, path: &str) -> Result<(StatusCode, Value), Error> {
    let response = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(API_TOKEN)
        .send()
        .await?;
    Ok((response.status(), response.json().await.unwrap_or(Value::Null)))
}

async fn plan(base: &str, parameters: Value) -> Result<Value, Error> {
    let (status, _, body) = post(
        base,
        "/v1/plans",
        &json!({ "operation": "test.power-cycle", "parameters": parameters }),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok(body)
}

async fn apply(base: &str, plan: &Value) -> Result<(StatusCode, reqwest::header::HeaderMap, Value), Error> {
    let id = plan["id"].as_str().ok_or("no plan id")?;
    post(
        base,
        &format!("/v1/plans/{id}/apply"),
        &json!({ "digest": plan["digest"] }),
    )
    .await
}

async fn finished(base: &str, job_id: &str) -> Result<Value, Error> {
    for _ in 0..500 {
        let (_, job) = get(base, &format!("/v1/jobs/{job_id}")).await?;
        if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
            return Ok(job);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("job did not finish".into())
}

#[tokio::test]
async fn a_plan_is_applied_as_a_job_followed_to_the_end() -> TestResult {
    let server = start(Duration::from_secs(5)).await?;
    let plan = plan(&server.base, json!({ "vmid": 240 })).await?;
    assert_eq!(plan["operation"], "test.power-cycle");
    assert_eq!(
        plan["summary"],
        json!({ "vmid": 240, "status": "stopped", "pause_ms": 0 })
    );
    assert_eq!(plan["targets"], json!([240]));
    assert_eq!(plan["digest"].as_str().map(str::len), Some(64));
    let (status, fetched) = get(
        &server.base,
        &format!("/v1/plans/{}", plan["id"].as_str().unwrap_or("")),
    )
    .await?;
    assert_eq!((status, &fetched), (StatusCode::OK, &plan));

    let (status, headers, job) = apply(&server.base, &plan).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let job_id = job["id"].as_str().ok_or("no job id")?;
    assert_eq!(
        headers.get("location").and_then(|v| v.to_str().ok()),
        Some(format!("/v1/jobs/{job_id}").as_str())
    );
    assert_eq!(job["plan_id"], plan["id"]);
    assert!(matches!(job["state"].as_str(), Some("queued" | "running")), "{job}");

    let job = finished(&server.base, job_id).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"], json!({ "vmid": 240, "status": "stopped" }));
    assert_eq!(job["error"], Value::Null);
    let steps: Vec<(&str, &str)> = job["steps"]
        .as_array()
        .ok_or("no steps")?
        .iter()
        .filter_map(|s| Some((s["name"].as_str()?, s["state"].as_str()?)))
        .collect();
    assert_eq!(steps, [("Start", "done"), ("Shut down", "done")]);
    let tasks: Vec<(&str, &str, &str)> = job["tasks"]
        .as_array()
        .ok_or("no tasks")?
        .iter()
        .filter_map(|t| Some((t["kind"].as_str()?, t["status"].as_str()?, t["exitstatus"].as_str()?)))
        .collect();
    assert_eq!(tasks, [("qmstart", "stopped", "OK"), ("qmshutdown", "stopped", "OK")]);
    assert!(
        job["tasks"][0]["upid"]
            .as_str()
            .is_some_and(|u| u.starts_with("UPID:pve-example-1:")),
        "{job}"
    );
    for key in ["created_at", "started_at", "finished_at"] {
        assert!(job[key].as_str().is_some_and(|t| t.ends_with('Z')), "{key}: {job}");
    }

    let (_, list) = get(&server.base, "/v1/jobs").await?;
    assert_eq!(list[0]["id"], job["id"]);

    let (status, headers, body) = apply(&server.base, &plan).await?;
    assert_eq!(status, StatusCode::CONFLICT, "a plan runs once: {body}");
    assert_error(&body, &headers, "plan_applied", false);
    Ok(())
}

#[tokio::test]
async fn a_failing_step_fails_the_job_with_pythons_message() -> TestResult {
    let server = start(Duration::from_secs(5)).await?;
    let plan = plan(&server.base, json!({ "vmid": 241 })).await?;
    let (status, _, job) = apply(&server.base, &plan).await?;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job = finished(&server.base, job["id"].as_str().unwrap_or("")).await?;
    assert_eq!(job["state"], "failed");
    assert_eq!(job["error"]["code"], "operation_failed");
    assert_eq!(
        job["error"]["message"],
        "Proxmox task qmstart for VM 241 on pve-example-1 failed: VM 241 already running"
    );
    assert_eq!(job["steps"], json!([{ "name": "Start", "state": "failed" }]));
    Ok(())
}

#[tokio::test]
async fn a_plan_is_checked_again_before_it_runs() -> TestResult {
    let server = start(Duration::from_secs(5)).await?;
    let stale = plan(&server.base, json!({ "vmid": 240 })).await?;
    let fresh = plan(&server.base, json!({ "vmid": 240, "pause_ms": 300 })).await?;
    // Something else starts the VM after the first plan was made.
    let (status, _, job) = apply(&server.base, &fresh).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let (status, headers, body) = apply(&server.base, &stale).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_error(&body, &headers, "busy", true);
    finished(&server.base, job["id"].as_str().unwrap_or("")).await?;

    let mut config = PveClientConfig::new(vec![server.sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    config.extra_ca_pem = Some(server.sim.ca_pem().to_owned());
    let pve = hs_pve::PveClient::new(config)?;
    let stale = plan(&server.base, json!({ "vmid": 240 })).await?;
    start_vm(&pve, "pve-example-1", 240, &wait(), &hs_core::tasks::NoObserver).await?;
    let (status, headers, body) = apply(&server.base, &stale).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_error(&body, &headers, "plan_changed", false);

    let wrong = json!({ "id": stale["id"], "digest": "0".repeat(64) });
    let (status, headers, body) = apply(&server.base, &wrong).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_error(&body, &headers, "plan_changed", false);
    Ok(())
}

#[tokio::test]
async fn plan_requests_are_validated() -> TestResult {
    let server = start(Duration::from_secs(5)).await?;
    for (body, status, code) in [
        (
            json!({ "operation": "nosuch", "parameters": {} }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "operation": "test.power-cycle", "parameters": { "vmid": "x" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "operation": "test.power-cycle", "parameters": {}, "extra": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
    ] {
        let (actual, headers, reply) = post(&server.base, "/v1/plans", &body).await?;
        assert_eq!(actual, status, "{body}: {reply}");
        assert_error(&reply, &headers, code, false);
        assert!(!reply.to_string().contains("nosuch"), "input echoed: {reply}");
    }
    let missing = "00000000-0000-4000-8000-000000000000";
    let (status, _) = get(&server.base, &format!("/v1/plans/{missing}")).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&server.base, &format!("/v1/jobs/{missing}")).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = post(
        &server.base,
        &format!("/v1/plans/{missing}/apply"),
        &json!({ "digest": "0".repeat(64) }),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

async fn next_message(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<Option<Value>, Error> {
    use futures_util::StreamExt;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next()).await? {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Ok(Message::Text(text))) => return Ok(Some(serde_json::from_str(&text)?)),
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
        }
    }
}

#[tokio::test]
async fn the_job_stream_sends_a_snapshot_changes_and_heartbeats() -> TestResult {
    let server = start(Duration::from_millis(40)).await?;
    let plan = plan(&server.base, json!({ "vmid": 240, "pause_ms": 300 })).await?;
    let (_, _, job) = apply(&server.base, &plan).await?;
    let job_id = job["id"].as_str().ok_or("no job id")?;

    let url = format!("{}/v1/jobs/{job_id}/events", server.base.replace("http://", "ws://"));
    let mut unauthenticated = url.as_str().into_client_request()?;
    unauthenticated.headers_mut().remove("authorization");
    let refused = tokio_tungstenite::connect_async(unauthenticated).await;
    assert!(refused.is_err(), "a stream needs the token");

    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {API_TOKEN}").parse()?);
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;

    let mut messages = Vec::new();
    while let Some(message) = next_message(&mut socket).await? {
        messages.push(message);
    }
    let first = messages.first().ok_or("no message")?;
    assert_eq!(first["type"], "snapshot");
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["data"]["id"], job_id);
    let sequences: Vec<u64> = messages.iter().filter_map(|m| m["stream_sequence"].as_u64()).collect();
    assert_eq!(sequences, (1..=messages.len() as u64).collect::<Vec<_>>());
    assert!(messages.iter().any(|m| m["type"] == "heartbeat" && m["data"].is_null()));
    assert!(
        messages
            .iter()
            .all(|m| m["server_instance_id"] == first["server_instance_id"])
    );
    assert!(
        messages
            .iter()
            .all(|m| m["server_time"].as_str().is_some_and(|t| t.ends_with('Z')))
    );
    let last = messages
        .iter()
        .rev()
        .find(|m| m["type"] == "job")
        .ok_or("no job message")?;
    assert_eq!(
        last["data"]["state"], "succeeded",
        "the stream ends with the final state"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["type"] == "job" && m["data"]["state"] == "running"),
        "changes are streamed"
    );

    let (status, _) = get(&server.base, "/v1/jobs/not-a-uuid").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}
