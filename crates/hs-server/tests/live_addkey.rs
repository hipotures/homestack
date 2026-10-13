//! L4, the gate of phase 9: `create` on Gold's node → `addkey` of a test
//! key on the running VM (in root's and the user's `authorized_keys` at
//! once, nothing removed) → `addkey` with a restart (the keys still there,
//! new SSH host keys) → shutdown → `addkey` of a second key on the stopped
//! VM → start: the key step of the vendor snippet on the node adds it →
//! `destroy`.
//!
//! The keys are made with `ssh-keygen` in a temporary directory and never
//! leave the test. Proxmox's pending changes after `sshkeys` is set on the
//! running VM are printed, for the report.
//!
//! `scripts/check.sh --live` runs it, with the rules of `live_refresh.rs`:
//! a free VMID in 240–249, the test token, pool `homestack-test`, the test
//! Gold 248, and the vendor snippets of this commit on Gold's node
//! (`deploy/pve/homestack-pve.sh install`). Whatever happens, the VM is
//! destroyed at the end.

mod process;

use std::path::{Path, PathBuf};
use std::time::Duration;

use hs_core::guest::{read_text, wait_for_qga};
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
const HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key.pub";

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

async fn exists(pve: &PveClient, vmid: u32) -> Result<bool, Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid))))
}

/// A new ed25519 key without a passphrase; its public line.
fn new_key(dir: &Path, name: &str) -> Result<String, Error> {
    let path = dir.join(name);
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&path)
        .status()?;
    if !made.success() {
        return Err(format!("ssh-keygen failed for {name}").into());
    }
    Ok(std::fs::read_to_string(path.with_extension("pub"))?.trim().to_owned())
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_addkey_running_restarted_and_stopped() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let cfg = Config::parse(SERVER_TOML)?;
    let pve = client()?;
    if exists(&pve, vmid).await? {
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
    let outcome = gate(&pve, &cfg, &launch, dir.path(), vmid).await;
    if exists(&pve, vmid).await.unwrap_or(true) {
        let timing = LifecycleTiming::default();
        let cleanup = async {
            let plan = plan_destroy(&pve, &cfg, POOL, vmid).await?;
            destroy_workspace(&pve, &cfg, POOL, &plan, &timing, &NoObserver).await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = cleanup {
            eprintln!("live addkey: cleanup of VM {vmid} failed: {error}");
        }
    }
    outcome
}

async fn addkey(server: &ServerProcess, version: &str, parameters: Value) -> Result<Value, Error> {
    let job = server
        .run("workspace.addkey", parameters.clone(), version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("addkey {parameters} failed: {job}").into());
    }
    eprintln!("live addkey: {} -> {}", parameters, job["result"]);
    Ok(job["result"].clone())
}

/// Root's and the user's `authorized_keys`.
async fn key_files(pve: &PveClient, node: &str, vmid: u32, user: &str) -> Result<[String; 2], Error> {
    let mut files = [String::new(), String::new()];
    for (slot, path) in files.iter_mut().zip([
        "/root/.ssh/authorized_keys".to_owned(),
        format!("/home/{user}/.ssh/authorized_keys"),
    ]) {
        *slot = read_text(pve, node, vmid, &path).await?.unwrap_or_default();
    }
    Ok(files)
}

fn blob(key: &str) -> &str {
    key.split_whitespace().nth(1).unwrap_or_default()
}

async fn wait_cloud_init(pve: &PveClient, node: &str, vmid: u32) -> Result<String, Error> {
    for _ in 0..150 {
        if let Some(result) = read_text(pve, node, vmid, "/run/cloud-init/result.json").await? {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!("Cloud-Init of VM {vmid} wrote no result").into())
}

async fn gate(pve: &PveClient, cfg: &Config, launch: &Launch, dir: &Path, vmid: u32) -> Result<(), Error> {
    let node = cfg.node.to_string();
    let user = cfg.user.name.clone();
    let id = vmid.to_string();
    let server = ServerProcess::start(launch, dir).await?;
    let version = server.configure(SERVER_TOML).await?;
    let job = server
        .run(
            "workspace.create",
            json!({"vmid": vmid, "name": "hst-addkey"}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("create failed: {job}").into());
    }
    eprintln!("live addkey: created VM {vmid} on {node}");
    let [_, home_before] = key_files(pve, &node, vmid, &user).await?;
    let host_before = read_text(pve, &node, vmid, HOST_KEY).await?.unwrap_or_default();

    // The running VM gets the key at once.
    let first = new_key(dir, "hs-addkey-1")?;
    let result = addkey(&server, &version, json!({"vmid": vmid, "key": first})).await?;
    if result["verified"] != true || result["restarted"] != false || result["pending_restart"] != false {
        return Err(format!("addkey on the running VM: {result}").into());
    }
    let vm_cfg = pve.get_value(&["nodes", &node, "qemu", &id, "config"], &[]).await?;
    let pending = pve.get_value(&["nodes", &node, "qemu", &id, "pending"], &[]).await?;
    let changed: Vec<&Value> = pending
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["key"] == "sshkeys" || entry["key"] == "cicustom")
        .collect();
    eprintln!("live addkey: pending sshkeys/cicustom after the change on the running VM: {changed:?}");
    if !hs_core::keys::unquote(vm_cfg["sshkeys"].as_str().unwrap_or_default()).contains(blob(&first))
        || vm_cfg["cicustom"] != "vendor=local:snippets/homestack-vendor-keep.yaml"
    {
        return Err(format!("the config after addkey: {vm_cfg}").into());
    }
    let [root, home] = key_files(pve, &node, vmid, &user).await?;
    if !root.contains(blob(&first)) || !home.contains(blob(&first)) || !home.starts_with(home_before.trim_end()) {
        return Err(format!("after addkey: root {root:?}, home {home:?}, home before {home_before:?}").into());
    }
    eprintln!("live addkey: the key is in both files without a restart; the home kept its keys");

    // The restart: a new instance, the keys stay.
    let result = addkey(&server, &version, json!({"vmid": vmid, "restart": true})).await?;
    if result["verified"] != true || result["restarted"] != true {
        return Err(format!("addkey with a restart: {result}").into());
    }
    let [root, home] = key_files(pve, &node, vmid, &user).await?;
    if !root.contains(blob(&first)) || !home.contains(blob(&first)) || !home.starts_with(home_before.trim_end()) {
        return Err(format!("after the restart: root {root:?}, home {home:?}").into());
    }
    let host_after = read_text(pve, &node, vmid, HOST_KEY).await?.unwrap_or_default();
    eprintln!(
        "live addkey: after the restart the host key {}",
        if host_after == host_before {
            "is the same"
        } else {
            "changed"
        }
    );
    if host_after == host_before {
        return Err("the new sshkeys did not start a new Cloud-Init instance (same host key)".into());
    }

    // The stopped VM gets the key from the snippet at its next start. Right
    // after a start with a new Cloud-Init instance PVE 9.2 keeps the VM's
    // config lock for a while ("can't lock file ... got timeout"); the
    // shutdown is tried again and the time it took is printed.
    let since = std::time::Instant::now();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let stopped = shutdown_vm(
            pve,
            &node,
            vmid,
            90,
            &TaskWait::up_to(Duration::from_secs(600)),
            &NoObserver,
        )
        .await;
        match stopped {
            Ok(()) => break,
            Err(error) if attempt < 12 => {
                eprintln!("live addkey: shutdown attempt {attempt} failed: {error}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    eprintln!(
        "live addkey: shut down after {attempt} attempt(s), {} s after the restart's check",
        since.elapsed().as_secs()
    );
    let second = new_key(dir, "hs-addkey-2")?;
    let result = addkey(&server, &version, json!({"vmid": vmid, "key": second, "restart": true})).await?;
    if result["restarted"] != false || result["verified"] != false || result["pending_restart"] != false {
        return Err(format!("addkey on the stopped VM: {result}").into());
    }
    let upid = pve
        .post_value(&["nodes", &node, "qemu", &id, "status", "start"], &[])
        .await?;
    let upid = upid.as_str().ok_or("start returned no task id")?;
    let started = hs_core::tasks::wait_task(pve, upid, &TaskWait::up_to(Duration::from_secs(300)), &NoObserver).await?;
    if !started.ok() {
        return Err(format!("start failed: {}", started.exitstatus).into());
    }
    wait_for_qga(pve, &node, vmid, Duration::from_secs(600)).await?;
    let cloud_init = wait_cloud_init(pve, &node, vmid).await?;
    eprintln!("live addkey: Cloud-Init result after the start: {}", cloud_init.trim());
    let [root, home] = key_files(pve, &node, vmid, &user).await?;
    for key in [&first, &second] {
        if !root.contains(blob(key)) || !home.contains(blob(key)) {
            return Err(format!("after the start: root {root:?}, home {home:?}").into());
        }
    }
    if !home.starts_with(home_before.trim_end()) {
        return Err(format!("the home lost keys: {home:?}, before {home_before:?}").into());
    }
    eprintln!("live addkey: the snippet's key step added the second key to both files");

    let job = server
        .run("workspace.destroy", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("destroy failed: {job}").into());
    }
    eprintln!("live addkey: destroyed VM {vmid}");
    Ok(())
}
