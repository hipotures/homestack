//! L4, the gate of phase 7: `create` → the home grown by 1G, the
//! filesystem sees the new size → the root grown by 1G (partition and
//! ext4) → the home again with the same size (finishes, no disk change) →
//! `destroy`.
//!
//! On the way it observes two things the simulator assumes: `PUT /resize`
//! with a stale `digest` starts a task that fails and changes nothing, and `input-data`
//! of `agent/exec` reaches the command's standard input byte for byte
//! (decision 6; no LUKS layout exists on the test Gold).
//!
//! `scripts/check.sh --live` runs it, with the rules of `live_refresh.rs`:
//! a free VMID in 240–249, the test token, pool `homestack-test`, the test
//! Gold 248. Whatever happens, the VM is destroyed at the end.

mod process;

use std::path::PathBuf;
use std::time::Duration;

use hs_core::guest::{GUEST_EXEC_TIMEOUT, guest_exec_with_input};
use hs_core::lifecycle::{LifecycleTiming, destroy_workspace, plan_destroy};
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use process::{Error, Launch, ServerProcess};
use serde_json::{Value, json};
use url::Url;

const POOL: &str = "homestack-test";
const GOLD_POOL: &str = "homestack-gold";
const SERVER_TOML: &str = include_str!("data/live-server.toml");
const LIMIT: Duration = Duration::from_secs(1800);
const GIB: u64 = 1024 * 1024 * 1024;

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

struct Vm {
    pve: PveClient,
    node: String,
    vmid: u32,
}

impl Vm {
    fn id(&self) -> String {
        self.vmid.to_string()
    }

    async fn config(&self) -> Result<Value, Error> {
        let id = self.id();
        Ok(self
            .pve
            .get_value(&["nodes", &self.node, "qemu", &id, "config"], &[])
            .await?)
    }

    /// The size of a disk slot in GiB (`size=NG`).
    async fn size_gib(&self, slot: &str) -> Result<u64, Error> {
        let config = self.config().await?;
        let value = config[slot].as_str().unwrap_or_default().to_owned();
        value
            .split(',')
            .find_map(|part| part.strip_prefix("size="))
            .and_then(|size| size.strip_suffix('G'))
            .and_then(|size| size.parse().ok())
            .ok_or_else(|| format!("{slot} has no size in whole GiB: {value}").into())
    }

    /// `get-fsinfo`'s size of the filesystem at `mount` (qemu-ga 8.2 of
    /// Ubuntu 24.04 has no `total-bytes-privileged`).
    async fn filesystem_bytes(&self, mount: &str) -> Result<u64, Error> {
        let id = self.id();
        let info = self
            .pve
            .get_value(&["nodes", &self.node, "qemu", &id, "agent", "get-fsinfo"], &[])
            .await?;
        info["result"]
            .as_array()
            .and_then(|list| list.iter().find(|fs| fs["mountpoint"] == mount))
            .and_then(|fs| {
                fs["total-bytes-privileged"]
                    .as_u64()
                    .or_else(|| fs["total-bytes"].as_u64())
            })
            .ok_or_else(|| {
                let mounts: Vec<String> = info["result"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|fs| format!("{} {}", fs["mountpoint"], fs["total-bytes-privileged"]))
                    .collect();
                format!("no filesystem at {mount}: {mounts:?}").into()
            })
    }

    async fn exists(&self) -> Result<bool, Error> {
        let list = self.pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(self.vmid))))
    }
}

#[tokio::test]
#[ignore = "live cluster, changes the test pool: run through scripts/check.sh --live"]
async fn live_resize_of_home_and_root() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let cfg = Config::parse(SERVER_TOML)?;
    let vm = Vm {
        pve: client()?,
        node: cfg.node.to_string(),
        vmid,
    };
    if vm.exists().await? {
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
    let outcome = gate(&vm, &cfg, &launch, dir.path()).await;
    if vm.exists().await.unwrap_or(true) {
        let timing = LifecycleTiming::default();
        let cleanup = async {
            let plan = plan_destroy(&vm.pve, &cfg, POOL, vmid).await?;
            destroy_workspace(&vm.pve, &cfg, POOL, &plan, &timing, &NoObserver).await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = cleanup {
            eprintln!("live resize: cleanup of VM {vmid} failed: {error}");
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

async fn gate(vm: &Vm, cfg: &Config, launch: &Launch, dir: &std::path::Path) -> Result<(), Error> {
    let vmid = vm.vmid;
    let id = vm.id();
    let server = ServerProcess::start(launch, dir).await?;
    let version = server.configure(SERVER_TOML).await?;
    let job = server
        .run(
            "workspace.create",
            json!({"vmid": vmid, "name": "hst-resize"}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("create failed: {job}").into());
    }
    let home = cfg.home_disk.clone();
    let root = cfg.root_disk.clone();
    let mount = format!("/home/{}", cfg.user.name);
    eprintln!(
        "live resize: created VM {vmid}, root {}G, home {}G",
        vm.size_gib(&root).await?,
        vm.size_gib(&home).await?
    );

    // A stale digest: Proxmox starts the task, which fails; nothing changes.
    let before = vm.size_gib(&home).await?;
    let stale = vm
        .pve
        .put_value(
            &["nodes", &vm.node, "qemu", &id, "resize"],
            &[("disk", home.as_str()), ("size", "+1G"), ("digest", &"0".repeat(40))],
        )
        .await;
    let upid = stale?;
    let upid = upid.as_str().ok_or("PUT /resize returned no task id")?;
    let wait = TaskWait {
        poll: Duration::from_secs(1),
        timeout: Duration::from_secs(120),
    };
    let result = wait_task(&vm.pve, upid, &wait, &NoObserver).await?;
    if result.ok() || !result.exitstatus.contains("checksum mismatch") {
        return Err(format!("the stale digest ended as {:?}", result.exitstatus).into());
    }
    eprintln!("live resize: stale digest refused by the task: {}", result.exitstatus);
    if vm.size_gib(&home).await? != before {
        return Err("the refused resize changed the disk".into());
    }

    // input-data reaches standard input as it is.
    let echoed = guest_exec_with_input(
        &vm.pve,
        &vm.node,
        vmid,
        "od -An -c",
        Some("pass phrase\twith tab"),
        true,
        GUEST_EXEC_TIMEOUT,
    )
    .await?;
    let echoed = echoed.out.split_whitespace().collect::<Vec<_>>().join(" ");
    if echoed != "p a s s p h r a s e \\t w i t h t a b" {
        return Err(format!("input-data arrived as {echoed:?}").into());
    }
    eprintln!("live resize: input-data arrives byte for byte");

    // The gate: the home grows by 1G.
    let home_before = vm.filesystem_bytes(&mount).await?;
    let target = before + 1;
    let job = server
        .run(
            "workspace.resize",
            json!({"vmid": vmid, "home_size": format!("{target}G")}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("home resize failed: {job}").into());
    }
    let home_after = vm.filesystem_bytes(&mount).await?;
    if vm.size_gib(&home).await? != target || home_after < home_before + GIB * 9 / 10 {
        return Err(format!("the home filesystem is {home_after} bytes, was {home_before}").into());
    }
    eprintln!(
        "live resize: home {before}G -> {target}G, filesystem {home_before} -> {home_after} bytes, tasks {:?}",
        kinds(&job)
    );

    // The root: partition and ext4.
    let root_size = vm.size_gib(&root).await?;
    let root_before = vm.filesystem_bytes("/").await?;
    let job = server
        .run(
            "workspace.resize",
            json!({"vmid": vmid, "root_size": format!("{}G", root_size + 1)}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("root resize failed: {job}").into());
    }
    let root_after = vm.filesystem_bytes("/").await?;
    if root_after < root_before + GIB * 9 / 10 {
        return Err(format!("the root filesystem is {root_after} bytes, was {root_before}").into());
    }
    eprintln!(
        "live resize: root {root_size}G -> {}G, filesystem {root_before} -> {root_after} bytes",
        root_size + 1
    );

    // The same size again: the guest steps only.
    let job = server
        .run(
            "workspace.resize",
            json!({"vmid": vmid, "home_size": format!("{target}G")}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" || !kinds(&job).is_empty() {
        return Err(format!("the repeated resize: {job}").into());
    }
    eprintln!("live resize: the same size again changed nothing in Proxmox");

    let job = server
        .run("workspace.destroy", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("destroy failed: {job}").into());
    }
    eprintln!("live resize: destroyed VM {vmid}");
    Ok(())
}
