//! L4, the gate of phase 10: `create` on a node other than Gold's →
//! verification → `destroy`, through the server's API.
//!
//! The target is pve3, else pve1; with neither online the test is skipped
//! and says so. Decision 3 = A: the job clones Gold on its node, migrates
//! the clone and makes the workspace on the target, so its tasks are
//! `qmclone` then `qmigrate`, and nothing of the VMID may be left on Gold's
//! node. The test token sees the volumes of its pool's VMs only, so what is
//! left after `destroy` is read with the server's read-only token
//! (`HOMESTACK_LIVE_TOKEN_FILE`), which lists the volumes of deleted VMs.
//!
//! `scripts/check.sh --live` runs it, with the rules of `live_migrate.rs`:
//! a free VMID in 240–249, the test token, pool `homestack-test`, the test
//! Gold 248. Whatever happens, the VM is destroyed at the end.

mod process;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hs_core::lifecycle::{LifecycleTiming, destroy_workspace, plan_destroy};
use hs_core::tasks::NoObserver;
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

fn client_with(token_path: &Path) -> Result<PveClient, Error> {
    let token = PveToken::from_file(token_path)?;
    let urls = endpoints()?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PveClient::new(PveClientConfig::new(urls, token))?)
}

/// The server's read-only token, which lists every VM volume.
fn reader() -> Result<PveClient, Error> {
    let path = std::env::var("HOMESTACK_LIVE_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TOKEN_FILE is not set")?;
    client_with(&PathBuf::from(path))
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

/// Volumes named for `vmid` on `storage` of `node`.
async fn volumes(pve: &PveClient, node: &str, storage: &str, vmid: u32) -> Result<Vec<String>, Error> {
    let list = pve
        .get_value(&["nodes", node, "storage", storage, "content"], &[])
        .await?;
    let marker = format!("vm-{vmid}-");
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("volid").and_then(Value::as_str))
        .filter(|volid| volid.contains(&marker))
        .map(str::to_owned)
        .collect())
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_create_on_another_node() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let cfg = Config::parse(SERVER_TOML)?;
    let pve = client_with(&token_file()?)?;
    let Some(other) = other_node(&pve).await? else {
        eprintln!("live create on another node: SKIPPED, neither of {CANDIDATES:?} is online");
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
            eprintln!("live create on another node: cleanup of VM {vmid} failed: {error}");
        }
    }
    outcome
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
    let target_storage = hs_core::inventory::resolve_homestack_storage(cfg, other, None)?;
    let clone_storage = hs_core::inventory::resolve_homestack_storage(cfg, &gold_node, None)?;
    let server = ServerProcess::start(launch, dir).await?;
    let version = server.configure(SERVER_TOML).await?;

    let started = Instant::now();
    let job = server
        .run(
            "workspace.create",
            json!({"vmid": vmid, "name": "hst-node", "node": other}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("create on {other} failed: {job}").into());
    }
    let kinds: Vec<&str> = job["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["kind"].as_str())
        .collect();
    eprintln!(
        "live create on another node: VM {vmid} on {other} in {:.0} s, tasks {kinds:?}",
        started.elapsed().as_secs_f64()
    );
    let clone_at = kinds.iter().position(|k| *k == "qmclone");
    let migrate_at = kinds.iter().position(|k| *k == "qmigrate");
    if !matches!((clone_at, migrate_at), (Some(c), Some(m)) if c < m) {
        return Err(format!("expected qmclone then qmigrate: {kinds:?}").into());
    }
    let result = &job["result"];
    if result["node"] != other || result["source_node"] != gold_node || result["transfer_method"] != "clone-migrate" {
        return Err(format!("unexpected result: {result}").into());
    }
    if placed(pve, vmid).await? != Some((other.to_owned(), "running".to_owned())) {
        return Err(format!("after create: {:?}", placed(pve, vmid).await?).into());
    }

    // While the VM exists its volumes are seen with the test token (VM.Audit
    // on its pool); after destroy, orphans only with the reader.
    let mut on_target = volumes(pve, other, &target_storage, vmid).await?;
    on_target.sort();
    eprintln!("live create on another node: volumes on {other} {target_storage}: {on_target:?}");
    let home = format!("{target_storage}:vm-{vmid}-hs-home-{}", cfg.user.name);
    if !on_target.contains(&home) || !on_target.iter().any(|v| v.ends_with(&format!("vm-{vmid}-disk-0"))) {
        return Err(format!("the root or the home is missing on {other}: {on_target:?}").into());
    }
    let on_gold = volumes(pve, &gold_node, &clone_storage, vmid).await?;
    if !on_gold.is_empty() {
        return Err(format!("the clone left volumes on {gold_node}: {on_gold:?}").into());
    }

    let job = server
        .run("workspace.destroy", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("destroy failed: {job}").into());
    }
    let reader = reader()?;
    let mut left = volumes(&reader, other, &target_storage, vmid).await?;
    left.extend(volumes(&reader, &gold_node, &clone_storage, vmid).await?);
    if placed(pve, vmid).await?.is_some() || !left.is_empty() {
        return Err(format!("left after destroy: {:?}, {left:?}", placed(pve, vmid).await?).into());
    }
    eprintln!("live create on another node: VM {vmid} destroyed, nothing left");
    Ok(())
}
