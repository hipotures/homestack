//! L4, the gate of phase 4: a workspace boots with native Cloud-Init and the
//! static vendor snippet (decision 1 = A) and passes the guest verification
//! of `create` (D2): hostname, address on `eth0`, the home mounted from the
//! disk with serial `HS_HOME_<vmid>` and labelled so, the user's account and
//! keys, root's keys, no other regular account, no `sudo`. A second boot with
//! the `keep` snippet on a blank home must leave it blank and unmounted.
//!
//! This changes the cluster, so it runs only in the test pool (PLAN.md 2.4)
//! and enforces it as `live_jobs.rs` does: a VMID in 240–249 that is free,
//! the test token, pool `homestack-test`, the test's name and marker checked
//! before every change, and the VM removed afterwards, also after a failure.
//! Gold is cloned on its node with the test token (`VM.Clone` on Gold); the
//! keys come from `workspace_authorized_keys` with the same token, so Gold's
//! `sshkeys` must be usable (the token cannot read Gold's files).
//!
//! It needs both vendor snippets on Gold's node (`deploy/cloud-init/`, see
//! docs/migration/deploy.md); a missing snippet fails the start.
//!
//! Ignored by default; `scripts/check.sh --live` runs it with
//! `HOMESTACK_LIVE_TEST_TOKEN_FILE`, `HOMESTACK_LIVE_PVE_ENDPOINTS`,
//! optionally `HOMESTACK_LIVE_TEST_VMID` (default 240) and
//! `HOMESTACK_LIVE_CONFIG` (the Python `config.toml`, default its usual
//! path), whose server part gives Gold, network, storage and user.

use std::path::PathBuf;
use std::time::Duration;

use hs_core::cloudinit::{HomeFormat, cloud_init_settings};
use hs_core::guest::{read_text, wait_for_qga};
use hs_core::inventory::{cluster_vm_resources, resolve_homestack_storage, vm_status};
use hs_core::keys::workspace_authorized_keys;
use hs_core::power::start_vm;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_core::verify::{GuestExpectation, VerifyFailure, VerifyTimeouts, verify_guest};
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use serde_json::Value;
use url::Url;

type Error = Box<dyn std::error::Error>;

const POOL: &str = "homestack-test";
const NAME: &str = "hst-boot";
const MARKER: &str = "HomeStack phase 4 live test; removed by the test";
const TEST_VMIDS: std::ops::RangeInclusive<u32> = 240..=249;

fn test_vmid() -> Result<u32, Error> {
    let vmid: u32 = std::env::var("HOMESTACK_LIVE_TEST_VMID")
        .unwrap_or_else(|_| "240".to_owned())
        .parse()?;
    if !TEST_VMIDS.contains(&vmid) {
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

/// The server part of the owner's Python configuration.
fn config() -> Result<Config, Error> {
    let path = match std::env::var("HOMESTACK_LIVE_CONFIG") {
        Ok(path) => PathBuf::from(path),
        Err(_) => hs_config::paths::default_config_path(|name| std::env::var(name).ok())
            .ok_or("no default config path; set HOMESTACK_LIVE_CONFIG")?,
    };
    let split = hs_config::split::split(&std::fs::read_to_string(&path)?)?;
    Ok(Config::parse(&split.server_toml)?)
}

fn wait(seconds: u64) -> TaskWait {
    TaskWait {
        poll: Duration::from_millis(500),
        timeout: Duration::from_secs(seconds),
    }
}

/// Run a Proxmox call that returns a UPID and require the task to succeed.
async fn task(pve: &PveClient, what: &str, upid: Value) -> Result<(), Error> {
    let result = wait_task(pve, upid.as_str().unwrap_or_default(), &wait(600), &NoObserver).await?;
    if result.ok() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", result.exitstatus).into())
    }
}

async fn exists(pve: &PveClient, vmid: u32) -> Result<bool, Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid))))
}

/// Refuse anything but the VM this test created.
async fn ours(pve: &PveClient, node: &str, vmid: u32) -> Result<(), Error> {
    let config = pve
        .get_value(&["nodes", node, "qemu", &vmid.to_string(), "config"], &[])
        .await?;
    if config.get("name").and_then(Value::as_str) == Some(NAME)
        && config.get("description").and_then(Value::as_str).map(str::trim) == Some(MARKER)
    {
        Ok(())
    } else {
        Err(format!("VM {vmid} is not the test VM; leaving it alone").into())
    }
}

/// Stop and destroy the test VM if it exists and is ours.
async fn cleanup(pve: &PveClient, node: &str, vmid: u32) -> Result<(), Error> {
    if !exists(pve, vmid).await? {
        return Ok(());
    }
    ours(pve, node, vmid).await?;
    let id = vmid.to_string();
    if vm_status(pve, node, vmid).await == "running" {
        let upid = pve
            .post_value(&["nodes", node, "qemu", &id, "status", "stop"], &[])
            .await?;
        task(pve, "stopping the test VM", upid).await?;
    }
    let upid = pve
        .delete_value(
            &["nodes", node, "qemu", &id],
            &[("purge", "1"), ("destroy-unreferenced-disks", "1")],
        )
        .await?;
    task(pve, "destroying the test VM", upid).await?;
    if exists(pve, vmid).await? {
        return Err(format!("test VM {vmid} was not removed").into());
    }
    eprintln!("live cloud-init: test VM {vmid} removed");
    Ok(())
}

struct Boot<'a> {
    pve: &'a PveClient,
    cfg: &'a Config,
    node: String,
    vmid: u32,
    storage: String,
    keys: String,
}

impl Boot<'_> {
    fn address(&self) -> String {
        format!("{}.{}", self.cfg.network.prefix, self.vmid)
    }

    /// Clone Gold, give it the Cloud-Init settings of `format` and a new
    /// 1 GiB home disk, and start it.
    async fn boot(&self, format: HomeFormat) -> Result<(), Error> {
        let (pve, node, id) = (self.pve, self.node.as_str(), self.vmid.to_string());
        let gold = self.cfg.gold_vmid.to_string();
        let upid = pve
            .post_value(
                &["nodes", node, "qemu", &gold, "clone"],
                &[
                    ("newid", &id),
                    ("name", NAME),
                    ("pool", POOL),
                    ("full", "1"),
                    ("storage", &self.storage),
                    ("description", MARKER),
                ],
            )
            .await?;
        task(pve, "cloning Gold", upid).await?;
        ours(pve, node, self.vmid).await?;

        let keys = (format == HomeFormat::Create).then_some(self.keys.as_str());
        let mut settings = cloud_init_settings(
            self.cfg,
            format,
            &self.address(),
            self.cfg.network.cidr,
            &self.cfg.network.gateway,
            keys,
        )?;
        let home = format!(
            "{}:1,serial=HS_HOME_{},discard=on,iothread=1,ssd=1",
            self.storage, self.vmid
        );
        settings.push(("tags", "homestack-test".to_owned()));
        let disk = self.cfg.home_disk.clone();
        let mut form: Vec<(&str, &str)> = settings.iter().map(|(key, value)| (*key, value.as_str())).collect();
        form.push((&disk, &home));
        let upid = pve.post_value(&["nodes", node, "qemu", &id, "config"], &form).await?;
        task(pve, "configuring the test VM", upid).await?;
        ours(pve, node, self.vmid).await?;
        start_vm(pve, node, self.vmid, &wait(120), &NoObserver).await?;
        Ok(())
    }

    fn expectation(&self) -> GuestExpectation {
        GuestExpectation {
            name: NAME.to_owned(),
            address: format!("{}/{}", self.address(), self.cfg.network.cidr),
            user: self.cfg.user.clone(),
            home_label: format!("HS_HOME_{}", self.vmid),
        }
    }
}

async fn gate(boot: &Boot<'_>) -> Result<(), Error> {
    let timeouts = VerifyTimeouts {
        agent: Duration::from_secs(300),
        cloud_init: Duration::from_secs(180),
    };

    boot.boot(HomeFormat::Create).await?;
    let verified = verify_guest(boot.pve, &boot.node, boot.vmid, &boot.expectation(), &timeouts)
        .await?
        .map_err(|failure| failure.create_message(&boot.expectation()))?;
    if verified.user_authorized_keys != boot.keys {
        return Err(format!(
            "the home's authorized_keys differ from Gold's keys:\n{}\nexpected\n{}",
            verified.user_authorized_keys, boot.keys
        )
        .into());
    }
    eprintln!(
        "live cloud-init: create verified (cloud-id {:?}, {} key(s))",
        verified.cloud_id,
        boot.keys.lines().count()
    );
    cleanup(boot.pve, &boot.node, boot.vmid).await?;

    boot.boot(HomeFormat::Keep).await?;
    wait_for_qga(boot.pve, &boot.node, boot.vmid, timeouts.agent).await?;
    let failure = verify_guest(boot.pve, &boot.node, boot.vmid, &boot.expectation(), &timeouts).await?;
    let home = format!("/home/{}", boot.cfg.user.name);
    match failure {
        Err(VerifyFailure::HomeMount { path, found }) if path == home && found.is_empty() => {}
        other => return Err(format!("keep on a blank home: expected no home mount, got {other:?}").into()),
    }
    let result = read_text(boot.pve, &boot.node, boot.vmid, "/run/cloud-init/result.json")
        .await?
        .unwrap_or_default();
    if !result.contains("runcmd") {
        return Err(format!("keep on a blank home: cloud-init reported no runcmd failure: {result}").into());
    }
    eprintln!("live cloud-init: keep refused to format a blank home");
    Ok(())
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_native_cloud_init_boots_a_workspace() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let pve = client()?;
    let cfg = config()?;
    if exists(&pve, vmid).await? {
        return Err(format!("VMID {vmid} is in use; refusing").into());
    }
    let node = cluster_vm_resources(&pve)
        .await?
        .into_iter()
        .find(|vm| vm.vmid == cfg.gold_vmid)
        .map(|vm| vm.node)
        .ok_or("Gold is not in the cluster")?;
    let storage = resolve_homestack_storage(&cfg, &node, None)?;
    let keys = workspace_authorized_keys(&pve, &cfg).await?;
    let boot = Boot {
        pve: &pve,
        cfg: &cfg,
        node,
        vmid,
        storage,
        keys: keys.keys,
    };
    let outcome = gate(&boot).await;
    let cleaned = cleanup(&pve, &boot.node, vmid).await;
    outcome?;
    cleaned
}
