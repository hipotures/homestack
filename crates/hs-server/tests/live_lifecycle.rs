//! L4, the gate of phase 5: `create` → verification → `destroy` of an
//! `hst-*` VM in the test pool, twice in a row, with nothing left.
//!
//! `scripts/check.sh --live` runs it. The rules of `live_cloudinit.rs`
//! apply: a free VMID in 240–249 (`HOMESTACK_LIVE_TEST_VMID`, default 240),
//! the test token, pool `homestack-test`; Gold is only the clone's source.
//! The test token sees the volumes of its pool's VMs only, so what is left
//! after `destroy` is read with the server's read-only token
//! (`HOMESTACK_LIVE_TOKEN_FILE`), which may list every VM volume.

use std::path::PathBuf;

use hs_core::lifecycle::{
    CreateRequest, LifecycleTiming, create_workspace, destroy_workspace, plan_create, plan_destroy,
};
use hs_core::tasks::NoObserver;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use serde_json::Value;
use url::Url;

type Error = Box<dyn std::error::Error>;

const POOL: &str = "homestack-test";

fn test_vmid() -> Result<u32, Error> {
    let vmid: u32 = std::env::var("HOMESTACK_LIVE_TEST_VMID")
        .unwrap_or_else(|_| "240".to_owned())
        .parse()?;
    if !(240..=249).contains(&vmid) {
        return Err(format!("VMID {vmid} is outside the test range 240-249; refusing").into());
    }
    Ok(vmid)
}

fn client() -> Result<PveClient, Error> {
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
    Ok(PveClient::new(PveClientConfig::new(endpoints, token))?)
}

/// The live tests' own configuration: the test Gold 248, never the
/// owner's (`tests/data/live-server.toml`).
fn config() -> Result<Config, Error> {
    Ok(Config::parse(include_str!("data/live-server.toml"))?)
}

/// The server's read-only token, which lists every VM volume.
fn reader() -> Result<PveClient, Error> {
    let path = std::env::var("HOMESTACK_LIVE_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TOKEN_FILE is not set")?;
    let token = PveToken::from_file(&PathBuf::from(path))?;
    let endpoints = std::env::var("HOMESTACK_LIVE_PVE_ENDPOINTS")?
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PveClient::new(PveClientConfig::new(endpoints, token))?)
}

/// Volumes named for `vmid` on `storage` of `node`.
async fn volumes(pve: &PveClient, node: &str, storage: &str, vmid: u32) -> Result<Vec<String>, Error> {
    let list = pve
        .get_value(&["nodes", node, "storage", storage, "content"], &[])
        .await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("volid").and_then(Value::as_str))
        .filter(|volid| volid.contains(&format!(":vm-{vmid}-")))
        .map(str::to_owned)
        .collect())
}

async fn exists(pve: &PveClient, vmid: u32) -> Result<bool, Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid))))
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_create_and_destroy_twice_leave_nothing() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let pve = client()?;
    let read = reader()?;
    let cfg = config()?;
    let timing = LifecycleTiming::default();
    for round in 1..=2 {
        if exists(&pve, vmid).await? {
            return Err(format!("VMID {vmid} is in use; refusing").into());
        }
        let request = CreateRequest {
            gold_vmid: None,
            vmid,
            name: format!("hst-cycle{round}"),
            home_size: Some("1G".to_owned()),
            storage: None,
            node: None,
        };
        let plan = plan_create(&pve, &cfg, POOL, &request).await?;
        let node = plan["node"].as_str().ok_or("plan without node")?.to_owned();
        let storage = plan["root_storage"].as_str().ok_or("plan without storage")?.to_owned();
        let before = volumes(&read, &node, &storage, vmid).await?;
        if !before.is_empty() {
            return Err(format!("volumes of VMID {vmid} exist before create: {before:?}").into());
        }
        let created = create_workspace(&pve, &cfg, &plan, &timing, &NoObserver).await?;
        eprintln!(
            "live lifecycle {round}: created {} ({})",
            created["name"], created["ip"]
        );
        let plan = plan_destroy(&pve, &cfg, POOL, vmid).await?;
        let destroyed = destroy_workspace(&pve, &cfg, POOL, &plan, &timing, &NoObserver).await?;
        eprintln!("live lifecycle {round}: {}", destroyed["event"]);
        if exists(&pve, vmid).await? {
            return Err(format!("VM {vmid} is still there after destroy").into());
        }
        let left = volumes(&read, &node, &storage, vmid).await?;
        if !left.is_empty() {
            return Err(format!("volumes left after destroy: {left:?}").into());
        }
        let pool = pve.get_value(&["pools"], &[("poolid", POOL)]).await?;
        if pool.to_string().contains(&format!("\"vmid\":{vmid}")) {
            return Err(format!("pool {POOL} still lists {vmid}").into());
        }
    }
    Ok(())
}
