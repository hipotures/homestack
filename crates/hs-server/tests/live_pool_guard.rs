//! L4, the gate before phase 5: with the production token, a VM outside the
//! HomeStack pools is invisible, reading, changing, starting or deleting it
//! fails with Proxmox's `403`, and the server refuses it itself, even though
//! its tag is `homestack-ws`.
//!
//! Needs a VM in the test range that is in no pool and tagged
//! `homestack-ws` (`HOMESTACK_LIVE_OUTSIDE_VMID`, on
//! `HOMESTACK_LIVE_OUTSIDE_NODE`, default pve2; made as root, e.g.
//! `qm create 249 --name hst-outside --tags homestack-ws`), the server's
//! token (`HOMESTACK_LIVE_TOKEN_FILE`) and `HOMESTACK_LIVE_PVE_ENDPOINTS`.
//! Nothing changes: every call is refused.

use std::path::PathBuf;

use hs_core::CoreError;
use hs_core::lifecycle::plan_destroy;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveError, PveToken};
use serde_json::Value;
use url::Url;

type Error = Box<dyn std::error::Error>;

fn client() -> Result<PveClient, Error> {
    let path = std::env::var("HOMESTACK_LIVE_TOKEN_FILE").map_err(|_| "HOMESTACK_LIVE_TOKEN_FILE is not set")?;
    let token = PveToken::from_file(&PathBuf::from(path))?;
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

fn forbidden<T: std::fmt::Debug>(what: &str, outcome: Result<T, PveError>) -> Result<(), Error> {
    match outcome {
        Err(PveError::Forbidden { message }) => {
            eprintln!("live pool guard: {what}: 403 {}", message.trim());
            Ok(())
        }
        other => Err(format!("{what}: expected 403, got {other:?}").into()),
    }
}

#[tokio::test]
#[ignore = "live cluster: needs a VM outside the pools, see the module docs"]
async fn live_changes_outside_the_pool_are_refused() -> Result<(), Error> {
    let vmid: u32 = std::env::var("HOMESTACK_LIVE_OUTSIDE_VMID")
        .map_err(|_| "HOMESTACK_LIVE_OUTSIDE_VMID is not set")?
        .parse()?;
    if !(240..=249).contains(&vmid) {
        return Err(format!("VMID {vmid} is outside the test range 240-249; refusing").into());
    }
    let pve = client()?;
    // The token sees no VM outside the HomeStack pools at all.
    let resources = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    if resources
        .as_array()
        .into_iter()
        .flatten()
        .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid)))
    {
        return Err(format!("the token can see VM {vmid}, which is in no HomeStack pool").into());
    }
    let node = std::env::var("HOMESTACK_LIVE_OUTSIDE_NODE").unwrap_or_else(|_| "pve2".to_owned());
    let id = vmid.to_string();
    forbidden(
        "reading its config",
        pve.get_value(&["nodes", &node, "qemu", &id, "config"], &[]).await,
    )?;
    forbidden(
        "changing its config",
        pve.post_value(
            &["nodes", &node, "qemu", &id, "config"],
            &[("description", "must be refused")],
        )
        .await,
    )?;
    forbidden(
        "starting it",
        pve.post_value(&["nodes", &node, "qemu", &id, "status", "start"], &[])
            .await,
    )?;
    forbidden(
        "deleting it",
        pve.delete_value(&["nodes", &node, "qemu", &id], &[("purge", "1")])
            .await,
    )?;
    match plan_destroy(&pve, &config()?, "homestack", vmid).await {
        Err(CoreError::OutsidePool(message) | CoreError::NotFound(message)) => {
            eprintln!("live pool guard: server: {message}");
        }
        other => return Err(format!("the server did not refuse: {other:?}").into()),
    }
    Ok(())
}
