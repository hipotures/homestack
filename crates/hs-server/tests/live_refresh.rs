//! L4, the gate of phase 6: `create` → a file in the home → `refresh` → the
//! file is there and the root is new → a `refresh` whose server process is
//! killed (`SIGKILL`) in the middle → a new server process recovers it →
//! `destroy`.
//!
//! `scripts/check.sh --live` runs it. The rules of `live_lifecycle.rs`
//! apply: a free VMID in 240–249 (`HOMESTACK_LIVE_TEST_VMID`, default 240),
//! the test token (`HOMESTACK_LIVE_TEST_TOKEN_FILE`), pool `homestack-test`,
//! the test Gold 248 of `tests/data/live-server.toml`. The server runs as
//! its own process (`tests/process`). Whatever happens, the VM is destroyed
//! at the end.

mod process;

use std::path::PathBuf;
use std::time::Duration;

use hs_core::lifecycle::{LifecycleTiming, destroy_workspace, plan_destroy};
use hs_core::refresh::read_journal;
use hs_core::tasks::NoObserver;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use process::{Error, Launch, ServerProcess};
use serde_json::{Value, json};
use url::Url;

const POOL: &str = "homestack-test";
const GOLD_POOL: &str = "homestack-gold";
const SERVER_TOML: &str = include_str!("data/live-server.toml");
const HOME_MARKER: &str = "/home/user/hs-refresh-marker";
const ROOT_MARKER: &str = "/root/hs-refresh-marker";
const LIMIT: Duration = Duration::from_secs(1800);

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

fn config() -> Result<Config, Error> {
    Ok(Config::parse(SERVER_TOML)?)
}

struct Vm {
    pve: PveClient,
    node: String,
    vmid: u32,
}

impl Vm {
    fn path<'a>(&'a self, id: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
        let mut path = vec!["nodes", self.node.as_str(), "qemu", id];
        path.extend_from_slice(tail);
        path
    }

    async fn config(&self) -> Result<Value, Error> {
        let id = self.vmid.to_string();
        Ok(self.pve.get_value(&self.path(&id, &["config"]), &[]).await?)
    }

    async fn root(&self) -> Result<String, Error> {
        let config = self.config().await?;
        Ok(config["scsi0"]
            .as_str()
            .unwrap_or_default()
            .split(',')
            .next()
            .unwrap_or_default()
            .to_owned())
    }

    async fn write_file(&self, file: &str, content: &str) -> Result<(), Error> {
        let id = self.vmid.to_string();
        self.pve
            .post_value(
                &self.path(&id, &["agent", "file-write"]),
                &[("file", file), ("content", content)],
            )
            .await?;
        Ok(())
    }

    /// A file in the guest, once its agent answers (at most 5 minutes after
    /// a start); `None` when the agent answers and the file is missing.
    async fn read_file(&self, file: &str) -> Result<Option<String>, Error> {
        let id = self.vmid.to_string();
        let mut answered = false;
        for _ in 0..300 {
            if self
                .pve
                .post_value(&self.path(&id, &["agent", "ping"]), &[])
                .await
                .is_ok()
            {
                answered = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !answered {
            return Err(format!("the guest agent of VM {} does not answer", self.vmid).into());
        }
        Ok(self
            .pve
            .get_value(&self.path(&id, &["agent", "file-read"]), &[("file", file)])
            .await
            .ok()
            .and_then(|read| read["content"].as_str().map(str::to_owned)))
    }

    /// [`Self::read_file`] until the file is there (the home is mounted
    /// during boot, possibly after the agent answers), at most 2 minutes.
    async fn wait_file(&self, file: &str) -> Result<Option<String>, Error> {
        for _ in 0..60 {
            if let Some(content) = self.read_file(file).await? {
                return Ok(Some(content));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Ok(None)
    }

    /// The journal phase in the VM's description, if any.
    async fn journal_phase(&self, cfg: &Config) -> Result<Option<String>, Error> {
        let config = self.config().await?;
        let vm_cfg = config
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
            .collect();
        Ok(read_journal(cfg, self.vmid, &vm_cfg)?.map(|journal| journal.phase.as_str().to_owned()))
    }

    /// Wait until no task of the VM runs any more (a task the killed server
    /// started goes on in Proxmox).
    async fn wait_idle(&self) -> Result<(), Error> {
        let id = self.vmid.to_string();
        for _ in 0..900 {
            let active = self
                .pve
                .get_value(
                    &["nodes", self.node.as_str(), "tasks"],
                    &[("vmid", id.as_str()), ("source", "active")],
                )
                .await?;
            let running = active.as_array().is_some_and(|tasks| !tasks.is_empty());
            let locked = self.config().await?.get("lock").is_some();
            if !running && !locked {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(format!("VM {} stayed busy", self.vmid).into())
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
async fn live_refresh_and_recovery_of_a_killed_server() -> Result<(), Error> {
    let vmid = test_vmid()?;
    let cfg = config()?;
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
    // Whatever happened: the VM goes.
    if vm.exists().await.unwrap_or(true) {
        let timing = LifecycleTiming::default();
        let cleanup = async {
            vm.wait_idle().await?;
            let plan = plan_destroy(&vm.pve, &cfg, POOL, vmid).await?;
            destroy_workspace(&vm.pve, &cfg, POOL, &plan, &timing, &NoObserver).await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = cleanup {
            eprintln!("live refresh: cleanup of VM {vmid} failed: {error}");
        }
    }
    outcome
}

async fn gate(vm: &Vm, cfg: &Config, launch: &Launch, dir: &std::path::Path) -> Result<(), Error> {
    let vmid = vm.vmid;
    let server = ServerProcess::start(launch, dir).await?;
    let version = server.configure(SERVER_TOML).await?;

    let job = server
        .run(
            "workspace.create",
            json!({"vmid": vmid, "name": "hst-refresh"}),
            &version,
            LIMIT,
        )
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("create failed: {job}").into());
    }
    vm.write_file(HOME_MARKER, "kept across refresh\n").await?;
    vm.write_file(ROOT_MARKER, "old root\n").await?;
    let first_root = vm.root().await?;
    eprintln!("live refresh: created VM {vmid}, root {first_root}");

    let job = server
        .run("workspace.refresh", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("refresh failed: {job}").into());
    }
    let second_root = vm.root().await?;
    if second_root == first_root {
        return Err(format!("the root is still {first_root}").into());
    }
    if vm.wait_file(HOME_MARKER).await?.as_deref() != Some("kept across refresh\n") {
        return Err("the file in the home is gone".into());
    }
    if vm.read_file(ROOT_MARKER).await?.is_some() {
        return Err("the file in the old root is still there".into());
    }
    eprintln!("live refresh: refreshed, root {second_root}, home file kept, root file gone");

    // A refresh killed in the middle: as soon as the journal is past
    // `prepared`, the server process gets SIGKILL.
    let (status, plan) = server.plan("workspace.refresh", json!({"vmid": vmid})).await?;
    if !status.is_success() {
        return Err(format!("plan: {status} {plan}").into());
    }
    server.apply(&plan, &version).await?;
    let mut killed_at = None;
    for _ in 0..3000 {
        if let Some(phase) = vm.journal_phase(cfg).await?
            && phase != "prepared"
        {
            killed_at = Some(phase);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let killed_at = killed_at.ok_or("the refresh never got past prepared")?;
    server.kill()?;
    eprintln!("live refresh: server killed at journal phase {killed_at}");
    vm.wait_idle().await?;

    let server = ServerProcess::start(launch, dir).await?;
    let (status, plan) = server.plan("workspace.refresh", json!({"vmid": vmid})).await?;
    if !status.is_success() || plan["summary"]["mode"] != "recover" {
        return Err(format!("recovery plan: {status} {plan}").into());
    }
    eprintln!(
        "live refresh: recovery plan from phase {}, mismatches {}",
        plan["summary"]["recovery_phase"], plan["summary"]["mismatches"]
    );
    let job = server.apply(&plan, &version).await?;
    let job = server.finished(&job, LIMIT).await?;
    if job["state"] != "succeeded" {
        return Err(format!("recovery failed: {job}").into());
    }
    let recovery = job["result"]["recovery"].as_str().unwrap_or_default().to_owned();
    let root = vm.root().await?;
    let expected = match recovery.as_str() {
        "rolled back to original root" => second_root.clone(),
        "completed verified cleanup" => root.clone(),
        other => return Err(format!("unexpected recovery {other}").into()),
    };
    if root != expected {
        return Err(format!("after {recovery} the root is {root}, expected {expected}").into());
    }
    let config = vm.config().await?;
    if config["description"]
        .as_str()
        .unwrap_or_default()
        .contains("homestack-journal")
    {
        return Err(format!("the journal is still there: {config}").into());
    }
    let unused: Vec<&String> = config
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.starts_with("unused"))
        .map(|(key, _)| key)
        .collect();
    if !unused.is_empty() {
        return Err(format!("volumes left as unused disks: {unused:?}").into());
    }
    if vm.wait_file(HOME_MARKER).await?.as_deref() != Some("kept across refresh\n") {
        return Err("the file in the home is gone after the recovery".into());
    }
    eprintln!("live refresh: recovered ({recovery}), root {root}, home file kept");

    let job = server
        .run("workspace.destroy", json!({"vmid": vmid}), &version, LIMIT)
        .await?;
    if job["state"] != "succeeded" {
        return Err(format!("destroy failed: {job}").into());
    }
    if vm.exists().await? {
        return Err(format!("VM {vmid} is still there after destroy").into());
    }
    eprintln!("live refresh: destroyed VM {vmid}");
    Ok(())
}
