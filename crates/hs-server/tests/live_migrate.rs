//! L4, the gate of phase 8: `create` on Gold's node → a marker file in the
//! home → `migrate` of the running VM to another node (running there, the
//! marker still in the home) → shutdown → `migrate` of the stopped VM back
//! (stopped there) → start, the marker again → `destroy`.
//!
//! The other node is pve3, else pve1; with neither online the test is
//! skipped and says so. The task log of each migration is printed, so the
//! simulator's `qmigrate` lines can be compared with Proxmox's.
//!
//! `scripts/check.sh --live` runs it, with the rules of `live_refresh.rs`:
//! a free VMID in 240–249, the test token, pool `homestack-test`, the test
//! Gold 248. Whatever happens, the VM is destroyed at the end.

mod process;

use std::path::PathBuf;
use std::time::Duration;

use hs_core::guest::{GUEST_EXEC_TIMEOUT, guest_exec, read_text, wait_for_qga};
use hs_core::lifecycle::{LifecycleTiming, destroy_workspace, plan_destroy};
use hs_core::power::shutdown_vm;
use hs_core::tasks::{NoObserver, TaskWait};
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use process::{Error, Launch, ServerProcess};
use serde_json::{Value, json};
use url::Url;

const POOL: &str = "homestack-test";
const GOLD_POOL: &str = "homestack-gold";
const SERVER_TOML: &str = include_str!("data/live-server.toml");
const LIMIT: Duration = Duration::from_secs(3600);
const CANDIDATES: [&str; 2] = ["pve3", "pve1"];

fn test_vmid() -> Result<u32, Error> {
    let vmid: u32 = std::env::var("HOMESTACK_LIVE_TEST_VMID")
        .unwrap_or_else(|_| "240".to_owned())
        .parse()?;
    if !(240..=249).contains(&vmid) {
        return Err(format!("VMID {vmid} is outside the test range 240-249; refusing").into());
    }
    Ok(vmid)
}

fn token_file() -> Result<PathBuf, Error> {
    let path = PathBuf::from(
        std::env::var("HOMESTACK_LIVE_TEST_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TEST_TOKEN_FILE is not set")?,
    );
    let token = PveToken::from_file(&path)?;
    if !token.id().ends_with("!test") {
        return Err(format!("{} is not the test token; refusing", token.id()).into());
    }
    Ok(path)
}

fn endpoints() -> Result<String, Error> {
    std::env::var("HOMESTACK_LIVE_PVE_ENDPOINTS").map_err(|_| "HOMESTACK_LIVE_PVE_ENDPOINTS is not set".into())
}

fn client() -> Result<PveClient, Error> {
    let token = PveToken::from_file(&token_file()?)?;
    let urls = endpoints()?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PveClient::new(PveClientConfig::new(urls, token))?)
}

/// `(node, status)` of the VM in `cluster/resources`, if it exists.
async fn placed(pve: &PveClient, vmid: u32) -> Result<Option<(String, String)>, Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .find(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid)))
        .map(|vm| {
            (
                vm["node"].as_str().unwrap_or_default().to_owned(),
                vm["status"].as_str().unwrap_or_default().to_owned(),
            )
        }))
}

/// The first of [`CANDIDATES`] that is online.
async fn other_node(pve: &PveClient) -> Result<Option<String>, Error> {
    let nodes = pve.get_value(&["nodes"], &[]).await?;
    let online = |name: &str| {
        nodes
            .as_array()
            .into_iter()
            .flatten()
            .any(|node| node["node"] == name && node["status"] == "online")
    };
    Ok(CANDIDATES
        .iter()
        .find(|name| online(name))
        .map(|name| (*name).to_owned()))
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_migrate_there_and_back() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let cfg = Config::parse(SERVER_TOML)?;
    let pve = client()?;
    let Some(other) = other_node(&pve).await? else {
        eprintln!("live migrate: SKIPPED, neither of {CANDIDATES:?} is online");
        return Ok(());
    };
    if placed(&pve, vmid).await?.is_some() {
        return Err(format!("VMID {vmid} is in use; refusing").into());
    }
    let dir = tempfile::tempdir()?;
    let launch = Launch {
        endpoints: endpoints()?,
        pve_token_file: token_file()?,
        ca_pem: None,
        pool: POOL.to_owned(),
        gold_pool: GOLD_POOL.to_owned(),
    };
    let outcome = gate(&pve, &cfg, &launch, dir.path(), vmid, &other).await;
    if placed(&pve, vmid).await.map_or(true, |found| found.is_some()) {
        let timing = LifecycleTiming::default();
        let cleanup = async {
            let plan = plan_destroy(&pve, &cfg, POOL, vmid).await?;
            destroy_workspace(&pve, &cfg, POOL, &plan, &timing, &NoObserver).await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = cleanup {
            eprintln!("live migrate: cleanup of VM {vmid} failed: {error}");
        }
    }
    outcome
}

fn kinds(job: &Value) -> Vec<String> {
    job["tasks"]
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|t| t["kind"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

async fn migrate(
    server: &ServerProcess,
    version: &str,
    vmid: u32,
    target: &str,
    storage: &str,
) -> Result<Value, Error> {
    let job = server
        .run(
            "workspace.migrate",
            json!({"vmid": vmid, "target_node": target, "target_storage": storage}),
            version,
            LIMIT,
        )
        .await?;
    eprintln!("live migrate: the task log to {target}:");
    for line in job["log"].as_array().into_iter().flatten() {
        eprintln!("    {}", line.as_str().unwrap_or_default());
    }
    if job["state"] != "succeeded" {
        return Err(format!("migrate to {target} failed: {job}").into());
    }
    Ok(job)
}

async fn gate(
    pve: &PveClient,
    cfg: &Config,
    launch: &Launch,
    dir: &std::path::Path,
    vmid: u32,
    other: &str,
) -> Result<(), Error> {
    let gold_node = cfg.node.to_string();
    let other_storage = hs_core::inventory::resolve_homestack_storage(cfg, other, None)?;
    let home_storage = hs_core::inventory::resolve_homestack_storage(cfg, &gold_node, None)?;
    let server = ServerProcess::start(launch, dir).await?;
    let version = server.configure(SERVER_TOML).await?;
    let job = server
        .run(
            "workspace.create",
            json!({"vmid": vmid, "name": "hst-migrate"}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("create failed: {job}").into());
    }
    eprintln!("live migrate: created VM {vmid} on {gold_node}");

    // A marker in the home, written as root through the agent.
    let marker = format!("/home/{}/.hs-migrate-marker", cfg.user.name);
    let content = format!("hs-migrate {vmid}");
    let write = format!("printf '%s' '{content}' > {marker}");
    let written = guest_exec(pve, &gold_node, vmid, &write, true, GUEST_EXEC_TIMEOUT).await?;
    if !written.exited || written.exitcode != 0 {
        return Err(format!("writing the marker failed: {written:?}").into());
    }

    // There, running.
    let job = migrate(&server, &version, vmid, other, &other_storage).await?;
    eprintln!("live migrate: tasks {:?}, progress {}", kinds(&job), job["progress"]);
    if job["result"]["status"] != "running"
        || placed(pve, vmid).await? != Some((other.to_owned(), "running".to_owned()))
    {
        return Err(format!("after the first migration: {:?}, {job}", placed(pve, vmid).await?).into());
    }
    let read = read_text(pve, other, vmid, &marker).await?;
    if read.as_deref() != Some(content.as_str()) {
        return Err(format!("the marker on {other} reads {read:?}").into());
    }
    eprintln!("live migrate: running on {other}, the home kept the marker");

    // Back, stopped.
    shutdown_vm(
        pve,
        other,
        vmid,
        90,
        &TaskWait::up_to(Duration::from_secs(600)),
        &NoObserver,
    )
    .await?;
    let job = migrate(&server, &version, vmid, &gold_node, &home_storage).await?;
    if job["result"]["status"] != "stopped"
        || placed(pve, vmid).await? != Some((gold_node.clone(), "stopped".to_owned()))
        || kinds(&job) != ["qmigrate"]
    {
        return Err(format!("after the second migration: {:?}, {job}", placed(pve, vmid).await?).into());
    }
    eprintln!("live migrate: stopped on {gold_node} again");

    // The home is still the same.
    let id = vmid.to_string();
    let upid = pve
        .post_value(&["nodes", &gold_node, "qemu", &id, "status", "start"], &[])
        .await?;
    let upid = upid.as_str().ok_or("start returned no task id")?;
    let started = hs_core::tasks::wait_task(pve, upid, &TaskWait::up_to(Duration::from_secs(300)), &NoObserver).await?;
    if !started.ok() {
        return Err(format!("start on {gold_node} failed: {}", started.exitstatus).into());
    }
    wait_for_qga(pve, &gold_node, vmid, Duration::from_secs(600)).await?;
    let read = read_text(pve, &gold_node, vmid, &marker).await?;
    if read.as_deref() != Some(content.as_str()) {
        return Err(format!("the marker on {gold_node} reads {read:?}").into());
    }
    eprintln!("live migrate: the home kept the marker on {gold_node}");

    let job = server
        .run("workspace.destroy", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("destroy failed: {job}").into());
    }
    eprintln!("live migrate: destroyed VM {vmid}");
    Ok(())
}
