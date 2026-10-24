//! L4, the gate of phase 3: jobs on the real cluster, each followed from
//! the Proxmox UPID to its end through the server's job model and stream.
//!
//! This changes the cluster, so it runs only in the test pool (PLAN.md 2.4)
//! and enforces it: the VMID must be in 240–249 and free, the VM is created
//! in pool `homestack-test` with the test token (whose privileges cover that
//! pool only), every step checks the VM's `hst-` name and the test's marker
//! in its config, and the VM is removed afterwards, also after a failure.
//! (The test token cannot see `pool` in `cluster/resources`: that needs
//! `Pool.Audit`.)
//! The VM has no disk and no network and the default size (the test token
//! has no `VM.Config.Memory` or `VM.Config.CPU`): it starts, but no guest
//! answers the ACPI button, so a shutdown fails the way Python reports it.
//!
//! Ignored by default; `scripts/check.sh --live` runs it with
//!
//! - `HOMESTACK_LIVE_TEST_TOKEN_FILE`: the test token (`homestack@pve!test=...`),
//! - `HOMESTACK_LIVE_PVE_ENDPOINTS`: comma-separated API origins,
//! - `HOMESTACK_LIVE_TEST_NODE` (default `pve2`), `HOMESTACK_LIVE_TEST_VMID`
//!   (default 240).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use hs_core::power::{shutdown_vm, start_vm};
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_pve::{PveClient, PveClientConfig, PveToken};
use hs_server::operations::{BoxFuture, JobHandle, OpContext, OpFailure, Operation, Planned};
use hs_server::{ApiToken, AppOptions, ServerConfig};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

type Error = Box<dyn std::error::Error>;

const API_TOKEN: &str = "live-api-token-0123456789abcdef0123456789";
const POOL: &str = "homestack-test";
const NAME: &str = "hst-task";
const MARKER: &str = "HomeStack phase 3 live test; removed by the test";
const TEST_VMIDS: std::ops::RangeInclusive<u32> = 240..=249;

struct Target {
    node: String,
    vmid: u32,
}

fn target() -> Result<Target, Error> {
    let node = std::env::var("HOMESTACK_LIVE_TEST_NODE").unwrap_or_else(|_| "pve2".to_owned());
    let vmid: u32 = std::env::var("HOMESTACK_LIVE_TEST_VMID")
        .unwrap_or_else(|_| "240".to_owned())
        .parse()?;
    if !TEST_VMIDS.contains(&vmid) {
        return Err(format!("VMID {vmid} is outside the test range 240-249; refusing").into());
    }
    Ok(Target { node, vmid })
}

fn client_config() -> Result<PveClientConfig, Error> {
    let path =
        std::env::var("HOMESTACK_LIVE_TEST_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TEST_TOKEN_FILE is not set")?;
    let token = PveToken::from_file(&PathBuf::from(path))?;
    if !token.id().ends_with("!test") {
        return Err(format!("{} is not the test token; refusing", token.id()).into());
    }
    let endpoints = std::env::var("HOMESTACK_LIVE_PVE_ENDPOINTS")
        .map_err(|_| "HOMESTACK_LIVE_PVE_ENDPOINTS is not set")?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PveClientConfig::new(endpoints, token))
}

fn wait() -> TaskWait {
    TaskWait {
        poll: Duration::from_millis(500),
        timeout: Duration::from_secs(120),
    }
}

/// The test VM as Proxmox lists it, if it exists.
async fn listed(pve: &PveClient, vmid: u32) -> Result<Option<Value>, Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .find(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid)))
        .cloned())
}

/// Whether the VM's config carries the test's name and marker (a job's
/// lease may follow the marker in the description, phase 11).
async fn is_test_vm(pve: &PveClient, node: &str, vmid: u32) -> Result<bool, Error> {
    let config = pve
        .get_value(&["nodes", node, "qemu", &vmid.to_string(), "config"], &[])
        .await?;
    Ok(config.get("name").and_then(Value::as_str) == Some(NAME)
        && config
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|text| text.lines().next().map(str::trim) == Some(MARKER)))
}

/// Refuse anything but the VM this test created.
async fn ours(pve: &PveClient, node: &str, vmid: u32) -> Result<(), OpFailure> {
    match is_test_vm(pve, node, vmid).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(OpFailure::invalid("Not the test VM; refusing.")),
        Err(_) => Err(OpFailure::not_found("No test VM.")),
    }
}

/// `test.start` or `test.shutdown` of the test VM: `{"vmid": N}`.
struct Power {
    name: &'static str,
    node: String,
}

impl Operation for Power {
    fn name(&self) -> &'static str {
        self.name
    }

    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            let vmid = parameters["vmid"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| TEST_VMIDS.contains(v))
                .ok_or(OpFailure::invalid("A VMID of the test range is required."))?;
            ours(&ctx.pve, &self.node, vmid).await?;
            let status = hs_core::inventory::vm_status(ctx.pve.as_ref(), &self.node, vmid).await;
            Ok(Planned {
                targets: vec![vmid],
                summary: json!({ "vmid": vmid, "node": self.node, "status": status }),
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
            ours(&ctx.pve, &self.node, vmid).await?;
            let pve = ctx.pve.as_ref();
            if self.name == "test.start" {
                job.step("Start", start_vm(pve, &self.node, vmid, &wait(), job)).await?;
            } else {
                job.step("Shut down", shutdown_vm(pve, &self.node, vmid, 15, &wait(), job))
                    .await?;
            }
            Ok(json!({ "vmid": vmid }))
        })
    }
}

async fn serve(node: &str) -> Result<String, Error> {
    let options = AppOptions {
        operations: vec![
            Arc::new(Power {
                name: "test.start",
                node: node.to_owned(),
            }),
            Arc::new(Power {
                name: "test.shutdown",
                node: node.to_owned(),
            }),
        ],
        ..AppOptions::default()
    };
    let app = hs_server::app_with(
        ServerConfig {
            heartbeat: std::sync::Arc::default(),
            api_token: ApiToken::parse(API_TOKEN)?.into(),
            pve: client_config()?.into(),
            config_file: std::env::temp_dir().join("homestack-live-unused/server.toml"),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools {
                workspaces: POOL.to_owned(),
                ..hs_server::Pools::default()
            },
        },
        options,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(format!("http://{addr}"))
}

async fn post(base: &str, path: &str, body: &Value) -> Result<(u16, Value), Error> {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(API_TOKEN)
        .json(body)
        .send()
        .await?;
    Ok((response.status().as_u16(), response.json().await?))
}

/// Plan, apply, and follow the job's stream to its last message.
async fn run_job(base: &str, operation: &str, vmid: u32) -> Result<Value, Error> {
    let (status, plan) = post(
        base,
        "/v1/plans",
        &json!({ "operation": operation, "parameters": { "vmid": vmid } }),
    )
    .await?;
    if status != 201 {
        return Err(format!("plan {operation}: {status} {plan}").into());
    }
    let id = plan["id"].as_str().ok_or("no plan id")?;
    let (status, job) = post(
        base,
        &format!("/v1/plans/{id}/apply"),
        &json!({ "digest": plan["digest"] }),
    )
    .await?;
    if status != 202 {
        return Err(format!("apply {operation}: {status} {job}").into());
    }
    let url = format!(
        "{}/v1/jobs/{}/events",
        base.replace("http://", "ws://"),
        job["id"].as_str().ok_or("no job id")?
    );
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {API_TOKEN}").parse()?);
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
    let mut last = Value::Null;
    let mut messages = 0;
    while let Some(message) = tokio::time::timeout(Duration::from_secs(180), socket.next()).await? {
        match message? {
            Message::Text(text) => {
                let message: Value = serde_json::from_str(&text)?;
                messages += 1;
                if !message["data"].is_null() {
                    last = message["data"].clone();
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    eprintln!(
        "live job {operation}: {} after {messages} stream messages; tasks {}; log {}",
        last["state"], last["tasks"], last["log"]
    );
    Ok(last)
}

fn check(condition: bool, what: &str, job: &Value) -> Result<(), Error> {
    if condition {
        Ok(())
    } else {
        Err(format!("{what}: {job}").into())
    }
}

async fn gate(base: &str, target: &Target) -> Result<(), Error> {
    let started = run_job(base, "test.start", target.vmid).await?;
    check(started["state"] == "succeeded", "start job", &started)?;
    let task = &started["tasks"][0];
    check(
        task["kind"] == "qmstart"
            && task["status"] == "stopped"
            && task["exitstatus"] == "OK"
            && task["upid"]
                .as_str()
                .is_some_and(|upid| upid.starts_with(&format!("UPID:{}:", target.node))),
        "start task",
        &started,
    )?;

    check(
        started["log"].as_array().and_then(|log| log.last()) == Some(&json!("TASK OK")),
        "start task log",
        &started,
    )?;

    let stopped = run_job(base, "test.shutdown", target.vmid).await?;
    let expected = format!(
        "VM {} did not shut down cleanly on {}. Current status: running. HomeStack will not force-stop it automatically.",
        target.vmid, target.node
    );
    check(
        stopped["state"] == "failed" && stopped["error"]["message"] == expected.as_str(),
        "shutdown job without a guest",
        &stopped,
    )?;
    let task = &stopped["tasks"][0];
    check(
        task["kind"] == "qmshutdown" && task["status"] == "stopped" && task["exitstatus"] != "OK",
        "shutdown task",
        &stopped,
    )?;
    let last_line = format!("TASK ERROR: {}", task["exitstatus"].as_str().unwrap_or_default());
    check(
        stopped["log"].as_array().and_then(|log| log.last()) == Some(&json!(last_line))
            && !stopped["log"].to_string().contains("no content"),
        "shutdown task log",
        &stopped,
    )?;
    Ok(())
}

/// Stop and destroy the test VM if it is ours.
async fn cleanup(pve: &PveClient, target: &Target) -> Result<(), Error> {
    if listed(pve, target.vmid).await?.is_none() {
        return Ok(());
    }
    if !is_test_vm(pve, &target.node, target.vmid).await? {
        return Err(format!("VM {} is not the test VM; leaving it alone", target.vmid).into());
    }
    let base = ["nodes", target.node.as_str(), "qemu"];
    let vmid = target.vmid.to_string();
    // `cluster/resources` lags behind a new VM; ask the VM itself.
    if hs_core::inventory::vm_status(pve, &target.node, target.vmid).await == "running" {
        let upid = pve
            .post_value(&[base[0], base[1], base[2], &vmid, "status", "stop"], &[])
            .await?;
        wait_task(pve, upid.as_str().unwrap_or_default(), &wait(), &NoObserver).await?;
    }
    let upid = pve
        .delete_value(&[base[0], base[1], base[2], &vmid], &[("purge", "1")])
        .await?;
    let result = wait_task(pve, upid.as_str().unwrap_or_default(), &wait(), &NoObserver).await?;
    if !result.ok() || listed(pve, target.vmid).await?.is_some() {
        return Err(format!("test VM {} was not removed: {}", target.vmid, result.exitstatus).into());
    }
    eprintln!("live job: test VM {} removed", target.vmid);
    Ok(())
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_jobs_follow_tasks_in_the_test_pool() -> Result<(), Error> {
    let target = target()?;
    let pve = PveClient::new(client_config()?)?;
    if let Some(vm) = listed(&pve, target.vmid).await? {
        return Err(format!("VMID {} is in use ({vm}); refusing", target.vmid).into());
    }
    let vmid = target.vmid.to_string();
    let created = pve
        .post_value(
            &["nodes", &target.node, "qemu"],
            &[("vmid", &vmid), ("name", NAME), ("pool", POOL), ("description", MARKER)],
        )
        .await?;
    let outcome = async {
        let result = wait_task(&pve, created.as_str().unwrap_or_default(), &wait(), &NoObserver).await?;
        if !result.ok() {
            return Err::<(), Error>(format!("creating the test VM failed: {}", result.exitstatus).into());
        }
        let base = serve(&target.node).await?;
        gate(&base, &target).await
    }
    .await;
    let cleaned = cleanup(&pve, &target).await;
    outcome?;
    cleaned
}
