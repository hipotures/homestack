//! L4, the gate of phase 5: `create` → verification → `destroy` of an
//! `hst-*` VM in the test pool, twice in a row, with nothing left.
//!
//! Not yet in `scripts/check.sh --live`: it clones Gold, and the owner has
//! not yet said whether the live checks may (2026-10-13). Run it by hand
//! once they have, with the test token after `homestack-pve.sh install
//! --test`:
//!
//! ```text
//! HOMESTACK_LIVE_TEST_TOKEN_FILE=... HOMESTACK_LIVE_PVE_ENDPOINTS=... \
//!     cargo test -p hs-server --test live_lifecycle -- --ignored --nocapture
//! ```
//!
//! The rules of `live_cloudinit.rs` apply: a free VMID in 240–249
//! (`HOMESTACK_LIVE_TEST_VMID`, default 240), the test token, pool
//! `homestack-test`. The test token sees the volumes of its pool's VMs only,
//! so what is left after `destroy` is checked by the volumes the VM had.

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

fn config() -> Result<Config, Error> {
    let path = match std::env::var("HOMESTACK_LIVE_CONFIG") {
        Ok(path) => PathBuf::from(path),
        Err(_) => hs_config::paths::default_config_path(|name| std::env::var(name).ok())
            .ok_or("no default config path; set HOMESTACK_LIVE_CONFIG")?,
    };
    let split = hs_config::split::split(&std::fs::read_to_string(&path)?)?;
    Ok(Config::parse(&split.server_toml)?)
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
#[ignore = "live cluster, changes the test pool and clones Gold: run by hand once the owner agrees"]
async fn live_create_and_destroy_twice_leave_nothing() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let pve = client()?;
    let cfg = config()?;
    let timing = LifecycleTiming::default();
    for round in 1..=2 {
        if exists(&pve, vmid).await? {
            return Err(format!("VMID {vmid} is in use; refusing").into());
        }
        let request = CreateRequest {
            vmid,
            name: format!("hst-cycle{round}"),
            home_size: Some("1G".to_owned()),
            storage: None,
            node: None,
        };
        let plan = plan_create(&pve, &cfg, POOL, &request).await?;
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
        let pool = pve.get_value(&["pools"], &[("poolid", POOL)]).await?;
        if pool.to_string().contains(&format!("\"vmid\":{vmid}")) {
            return Err(format!("pool {POOL} still lists {vmid}").into());
        }
    }
    Ok(())
}
