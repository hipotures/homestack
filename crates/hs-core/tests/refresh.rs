//! Critical refresh safety checks; simulated behavior is not live E2E evidence.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::{CreateRequest, LifecycleTiming, create_workspace, plan_create};
use hs_core::refresh::{Journal, plan_refresh, read_journal, refresh_workspace};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const NODE: &str = "pve-example-1";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";
const CLUSTER: &str = include_str!("data/lifecycle-cluster.toml");
const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
[root]
disk = "scsi0"
[home]
disk = "scsi1"
default_size = "20G"
[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]
[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "developer"
uid = 1000
gid = 1000
"#;

const OLD_ROOT: &str = "local-lvm:vm-200-disk-0";
const NEW_ROOT: &str = "local-lvm:vm-200-disk-1";
const HOME: &str = "local-lvm:vm-200-hs-home-developer";
const MARKER: &str = "/home/developer/marker";
const ROOT_MARKER: &str = "/root/marker";

fn config() -> Result<Config, Error> {
    Ok(Config::parse(SERVER_TOML)?)
}

fn client(sim: &PveSim, token: &str) -> Result<PveClient, Error> {
    let mut client = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    client.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(client)?)
}

fn quick() -> LifecycleTiming {
    LifecycleTiming {
        poll: Duration::from_millis(5),
        verify: VerifyTimeouts {
            agent: Duration::from_secs(5),
            cloud_init: Duration::from_secs(5),
        },
    }
}

fn ops(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

fn path(tail: &str) -> String {
    format!("/nodes/{NODE}/qemu/200/{tail}")
}

fn fault(op: &str, occurrence: u32) -> FaultSpec {
    FaultSpec {
        op: op.to_owned(),
        occurrence,
        status: 500,
        message: format!("injected failure of {op}"),
        applied: false,
        hang: false,
        task: None,
    }
}

async fn workspace() -> Result<(PveSim, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    let cfg = config()?;
    let request = CreateRequest {
        gold_vmid: None,
        vmid: 200,
        name: "test1".to_owned(),
        home_size: None,
        storage: None,
        node: None,
    };
    let plan = plan_create(&pve, &cfg, POOL, &request).await?;
    create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await?;
    for (file, content) in [(MARKER, "kept"), (ROOT_MARKER, "old root")] {
        pve.post_value(
            &["nodes", NODE, "qemu", "200", "agent", "file-write"],
            &[("file", file), ("content", content)],
        )
        .await?;
    }
    sim.clear_ops();
    Ok((sim, pve))
}

async fn vm_config(pve: &PveClient) -> Result<Value, Error> {
    Ok(pve.get_value(&["nodes", NODE, "qemu", "200", "config"], &[]).await?)
}

async fn status(pve: &PveClient) -> Result<String, Error> {
    let current = pve
        .get_value(&["nodes", NODE, "qemu", "200", "status", "current"], &[])
        .await?;
    Ok(current["status"].as_str().unwrap_or_default().to_owned())
}

async fn stop(pve: &PveClient) -> Result<(), Error> {
    pve.post_value(&["nodes", NODE, "qemu", "200", "status", "stop"], &[])
        .await?;
    for _ in 0..100 {
        if status(pve).await? == "stopped" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("VM 200 did not stop".into())
}

async fn guest_file(sim: &PveSim, file: &str) -> Result<Option<String>, Error> {
    let admin = client(sim, ADMIN)?;
    match admin
        .get_value(&["nodes", NODE, "qemu", "200", "agent", "file-read"], &[("file", file)])
        .await
    {
        Ok(read) => Ok(read["content"].as_str().map(str::to_owned)),
        Err(_) => Ok(None),
    }
}

async fn volumes(sim: &PveSim) -> Result<Vec<String>, Error> {
    let admin = client(sim, ADMIN)?;
    let list = admin
        .get_value(&["nodes", NODE, "storage", "local-lvm", "content"], &[])
        .await?;
    let mut found: Vec<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["volid"].as_str())
        .filter(|v| v.contains("vm-200-"))
        .map(str::to_owned)
        .collect();
    found.sort();
    Ok(found)
}

fn unused(config: &Value) -> Vec<String> {
    config
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.starts_with("unused"))
        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
        .collect()
}

fn root(config: &Value) -> String {
    config["scsi0"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .to_owned()
}

async fn assert_rolled_back(sim: &PveSim, pve: &PveClient, before: &Value, power: &str, what: &str) -> TestResult {
    let after = vm_config(pve).await?;
    assert_eq!(root(&after), OLD_ROOT, "{what}: root");
    assert_eq!(unused(&after), Vec::<String>::new(), "{what}: unused");
    assert!(after.get("description").is_none(), "{what}: journal left: {after}");
    for key in [
        "boot",
        "ipconfig0",
        "nameserver",
        "ciuser",
        "ciupgrade",
        "cicustom",
        "scsi1",
        "tags",
    ] {
        assert_eq!(after.get(key), before.get(key), "{what}: {key}");
    }
    assert_eq!(
        volumes(sim).await?,
        ["local-lvm:vm-200-cloudinit", OLD_ROOT, HOME],
        "{what}: volumes"
    );
    assert_eq!(status(pve).await?, power, "{what}: power");
    if power == "running" {
        assert_eq!(guest_file(sim, MARKER).await?.as_deref(), Some("kept"), "{what}: home");
    }
    Ok(())
}

async fn assert_refreshed(sim: &PveSim, pve: &PveClient, power: &str, what: &str) -> TestResult {
    let after = vm_config(pve).await?;
    assert_eq!(root(&after), NEW_ROOT, "{what}: root");
    assert_eq!(unused(&after), Vec::<String>::new(), "{what}: unused");
    assert!(after.get("description").is_none(), "{what}: journal left: {after}");
    assert_eq!(
        after["cicustom"], "vendor=local:snippets/homestack-vendor-keep.yaml",
        "{what}"
    );
    assert_eq!(after["boot"], "order=scsi0;ide2;net0", "{what}: boot");
    assert_eq!(
        volumes(sim).await?,
        ["local-lvm:vm-200-cloudinit", NEW_ROOT, HOME],
        "{what}: volumes"
    );
    assert_eq!(status(pve).await?, power, "{what}: power");
    if power == "running" {
        assert_eq!(guest_file(sim, MARKER).await?.as_deref(), Some("kept"), "{what}: home");
        assert_eq!(guest_file(sim, ROOT_MARKER).await?, None, "{what}: the root is new");
    }
    Ok(())
}

async fn describe(pve: &PveClient, text: &str) -> TestResult {
    pve.put_value(&["nodes", NODE, "qemu", "200", "config"], &[("description", text)])
        .await?;
    Ok(())
}

async fn interrupt(sim: &PveSim, pve: &PveClient, op: &str, occurrence: u32, applied: bool) -> TestResult {
    let cfg = config()?;
    let plan = plan_refresh(pve, &cfg, POOL, 200, false).await?;
    let mut hang = fault(op, occurrence);
    hang.hang = true;
    hang.applied = applied;
    sim.inject_fault(hang)?;
    let timing = quick();
    let before = sim.hangs();
    let run = refresh_workspace(pve, &cfg, POOL, &plan, &timing, &NoObserver);
    let hung = async {
        while sim.hangs() == before {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::select! {
        result = run => Err(format!("the refresh was not interrupted at {op} #{occurrence}: {result:?}").into()),
        () = hung => Ok(()),
    }
}

#[tokio::test]
async fn recovery_rejects_journal_that_targets_persistent_home() -> TestResult {
    let (sim, pve) = workspace().await?;
    interrupt(&sim, &pve, &format!("post{{path={}}}", path("config")), 2, false).await?;
    let config_now = vm_config(&pve).await?;
    let vm_cfg = config_now
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
        .collect();
    let mut journal: Journal = read_journal(&config()?, 200, &vm_cfg)?.ok_or("no journal")?;
    journal.new_root_volume = HOME.to_owned();
    let payload = base64_json(&journal)?;
    describe(&pve, &format!("<!-- homestack-journal: {payload} -->")).await?;
    match plan_refresh(&pve, &config()?, POOL, 200, false).await {
        Err(CoreError::Unusable(message)) => assert!(message.contains("staged root identity"), "{message}"),
        other => return Err(format!("expected the identity refusal, got {other:?}").into()),
    }
    Ok(())
}

fn base64_json(journal: &Journal) -> Result<String, Error> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(journal)?))
}

#[tokio::test]
async fn failed_verification_restores_old_root_and_deletes_only_new_root() -> TestResult {
    let (sim, pve) = workspace().await?;
    stop(&pve).await?;
    let before = vm_config(&pve).await?;
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    sim.inject_fault(fault(&format!("get{{path={}}}", path("agent/get-host-name")), 1))?;
    let error = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("Automatic recovery succeeded"), "{error}");
    let calls = ops(&sim);
    assert!(!calls.iter().any(|op| op.contains("vm-200-disk-0")), "{calls:?}");
    assert_rolled_back(&sim, &pve, &before, "stopped", "failed verification").await
}

#[tokio::test]
async fn running_workspace_gets_golds_root_and_keeps_its_home() -> TestResult {
    let (sim, pve) = workspace().await?;
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    assert_eq!(plan["gold_root_volume"], "local-lvm:vm-101-disk-0");
    assert_eq!(plan["ip"], "192.0.2.200");
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "planning changes nothing"
    );
    let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["status"], "running");
    assert_eq!(result["new_root_volume"], NEW_ROOT);
    assert_eq!(result["host_key_changed"], true);
    assert_refreshed(&sim, &pve, "running", "running").await?;
    // A second refresh works on the result of the first.
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    let after = vm_config(&pve).await?;
    assert_eq!(root(&after), "local-lvm:vm-200-disk-0");
    assert_eq!(guest_file(&sim, MARKER).await?.as_deref(), Some("kept"));
    Ok(())
}
