//! `refresh` and its recovery (B3, B4, E3) on the simulator, and the Python
//! tests of phase 6 (`docs/migration/phase-6.md`).
//!
//! The cluster is the one of `lifecycle.rs`; each test makes workspace 200
//! with `create` first. Faults are injected while the simulator runs, so
//! they count only the calls of the refresh.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::{CreateRequest, LifecycleTiming, create_workspace, plan_create, verify_workspace_role_tags};
use hs_core::refresh::{Journal, Phase, plan_refresh, read_journal, refresh_workspace, require_refresh_unlocked};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

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

/// A cluster with workspace 200 (`test1`) made by `create`, running, with
/// a file in its home and one in root's.
async fn workspace() -> Result<(PveSim, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    let cfg = config()?;
    let request = CreateRequest {
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

/// A file in the guest, read with the admin token, so the read is no part
/// of what is tested.
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

/// The volumes of VM 200 on `local-lvm`.
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

/// The state Python's refresh leaves after rolling back: the original root
/// attached, nothing staged, no journal, the options of before, the power
/// state of before, the home and its files untouched.
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

/// The state of a finished refresh: Gold's copy is the root, the old root
/// is gone, no journal, the home kept.
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

// --- ports of the Python tests ---------------------------------------------

/// `RefreshLockTests::test_lock_is_exact_tag_and_error_explains_removal`
#[test]
fn lock_is_exact_tag_and_error_explains_removal() {
    let locked = [("tags".to_owned(), "homestack-ws; homestack-lock ".to_owned())].into();
    match require_refresh_unlocked(200, &locked) {
        Err(CoreError::Refused(message)) => {
            assert!(
                message.contains("homestack-lock") && message.contains("Remove"),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let other = [("tags".to_owned(), "homestack-ws;homestack-lock-extra".to_owned())].into();
    assert!(require_refresh_unlocked(200, &other).is_ok());
}

/// `RefreshLockTests::test_role_tags_allow_lock_but_reject_missing_gold_or_unknown`
#[test]
fn role_tags_allow_lock_but_reject_missing_gold_or_unknown() {
    let tags = |value: &str| [("tags".to_owned(), value.to_owned())].into();
    assert!(verify_workspace_role_tags(200, &tags("homestack-ws;homestack-lock")).is_ok());
    for bad in [
        "homestack-lock",
        "homestack-ws;homestack-gold",
        "homestack-ws;unexpected",
    ] {
        assert!(verify_workspace_role_tags(200, &tags(bad)).is_err(), "{bad}");
    }
}

async fn tag(pve: &PveClient, tags: &str) -> TestResult {
    pve.put_value(&["nodes", NODE, "qemu", "200", "config"], &[("tags", tags)])
        .await?;
    Ok(())
}

async fn describe(pve: &PveClient, text: &str) -> TestResult {
    pve.put_value(&["nodes", NODE, "qemu", "200", "config"], &[("description", text)])
        .await?;
    Ok(())
}

/// `RefreshLockTests::test_build_plan_rejects_lock_before_reading_journal`:
/// a broken journal is not even read when the lock is set.
#[tokio::test]
async fn build_plan_rejects_lock_before_reading_journal() -> TestResult {
    let (_sim, pve) = workspace().await?;
    describe(&pve, "<!-- homestack-journal: not base64! -->").await?;
    tag(&pve, "homestack-ws;homestack-lock").await?;
    match plan_refresh(&pve, &config()?, POOL, 200, false).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("homestack-lock"), "{message}"),
        other => return Err(format!("expected the lock, got {other:?}").into()),
    }
    Ok(())
}

/// The journal of an interrupted refresh: hang the call `op` (its
/// `occurrence`-th from now), with `applied` saying whether Proxmox
/// carried it out, and drop the refresh as if the server were killed.
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

/// `RefreshLockTests::test_direct_recovery_rejects_lock_before_recovering`
#[tokio::test]
async fn direct_recovery_rejects_lock_before_recovering() -> TestResult {
    let (sim, pve) = workspace().await?;
    interrupt(&sim, &pve, &format!("post{{path={}}}", path("config")), 1, false).await?;
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    assert_eq!(plan["mode"], "recover");
    tag(&pve, "homestack-ws;homestack-lock").await?;
    sim.clear_ops();
    match refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("homestack-lock"), "{message}"),
        other => return Err(format!("expected the lock, got {other:?}").into()),
    }
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "{:?}",
        ops(&sim)
    );
    Ok(())
}

/// `RefreshLockTests::test_direct_replace_rejects_lock_before_journal_or_import`
#[tokio::test]
async fn direct_replace_rejects_lock_before_journal_or_import() -> TestResult {
    let (sim, pve) = workspace().await?;
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    tag(&pve, "homestack-ws;homestack-lock").await?;
    sim.clear_ops();
    match refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("homestack-lock"), "{message}"),
        other => return Err(format!("expected the lock, got {other:?}").into()),
    }
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "{:?}",
        ops(&sim)
    );
    Ok(())
}

/// `LifecycleAlienGuardsTests::test_refresh_plan_rejects_alien_before_reading_recovery_journal`
/// (VM 160 is an alien VM in the pool).
#[tokio::test]
async fn refresh_plan_rejects_alien_before_reading_recovery_journal() -> TestResult {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    match plan_refresh(&pve, &config()?, POOL, 160, false).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("alien workspace role tag"), "{message}"),
        other => return Err(format!("expected the alien refusal, got {other:?}").into()),
    }
    Ok(())
}

/// `LifecycleAlienGuardsTests::test_refresh_recovery_rejects_alien_before_recovery`
#[tokio::test]
async fn refresh_recovery_rejects_alien_before_recovery() -> TestResult {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    let plan = json!({"mode": "recover", "vmid": 160, "node": NODE, "journal": {"node": NODE}});
    match refresh_workspace(&pve, &config()?, POOL, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("alien workspace role tag"), "{message}"),
        other => return Err(format!("expected the alien refusal, got {other:?}").into()),
    }
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "{:?}",
        ops(&sim)
    );
    Ok(())
}

/// `RefreshPowerStateTests::test_existing_journal_builds_recovery_plan_without_normal_resolution`
#[tokio::test]
async fn existing_journal_builds_recovery_plan() -> TestResult {
    let (sim, pve) = workspace().await?;
    // Killed while the old root was being unlinked (phase switching).
    interrupt(&sim, &pve, &format!("put{{path={}}}", path("unlink")), 2, false).await?;
    let plan = plan_refresh(&pve, &config()?, POOL, 200, false).await?;
    assert_eq!(plan["mode"], "recover");
    assert_eq!(plan["recovery_phase"], "switching");
    assert_eq!(plan["name"], "test1");
    assert_eq!(plan["home_volume"], HOME);
    assert_eq!(plan["mismatches"], json!([]));
    Ok(())
}

/// `RefreshPowerStateTests::test_recovery_rejects_journal_that_targets_persistent_home`
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

/// `RefreshPowerStateTests::test_stopped_workspace_is_booted_verified_then_stopped`
#[tokio::test]
async fn stopped_workspace_is_booted_verified_then_stopped() -> TestResult {
    let (sim, pve) = workspace().await?;
    stop(&pve).await?;
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    assert_eq!(plan["mode"], "replace");
    assert_eq!(plan["status"], "stopped");
    sim.clear_ops();
    let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["status"], "stopped");
    assert_eq!(result["guest_verified"], true);
    assert_eq!(result["power_state_preserved"], true);
    assert_eq!(result["event"], "workspace.refreshed");
    let calls = ops(&sim);
    let at = |needle: &str| calls.iter().position(|op| op.contains(needle));
    let start = at("status/start").ok_or("no start")?;
    let verify = at("agent/get-host-name").ok_or("no verification")?;
    let shutdown = at("status/shutdown").ok_or("no shutdown")?;
    let delete = calls.iter().rposition(|op| op.contains("/unlink")).ok_or("no unlink")?;
    assert!(start < verify && verify < shutdown && shutdown < delete, "{calls:?}");
    assert_refreshed(&sim, &pve, "stopped", "stopped").await
}

/// `RefreshPowerStateTests::test_failed_verification_restores_old_root_and_deletes_only_new_root`
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

// --- new on the simulator ----------------------------------------------------

/// A running workspace: the home and its files stay, the root is Gold's
/// again, the old root is deleted, the VM runs again; unlink is never sent
/// with `digest` (owner's decision).
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

#[tokio::test]
async fn refresh_refuses_a_workspace_outside_the_pool_and_force_without_journal() -> TestResult {
    let (sim, pve) = workspace().await?;
    let cfg = config()?;
    assert!(matches!(
        plan_refresh(&pve, &cfg, POOL, 150, false).await,
        Err(CoreError::OutsidePool(_))
    ));
    assert!(matches!(
        plan_refresh(&pve, &cfg, POOL, 200, true).await,
        Err(CoreError::Invalid(_))
    ));
    let forged = json!({"mode": "replace", "vmid": 150, "node": NODE});
    assert!(matches!(
        refresh_workspace(&pve, &cfg, POOL, &forged, &quick(), &NoObserver).await,
        Err(CoreError::OutsidePool(_))
    ));
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "{:?}",
        ops(&sim)
    );
    Ok(())
}

/// The calls of a refresh of a running workspace, in order, that change
/// something: (operation, occurrence from the start of the refresh, the
/// journal phase in the description when it is made).
fn changing_calls() -> Vec<(String, u32, &'static str)> {
    let config = format!("post{{path={}}}", path("config"));
    let journal = format!("put{{path={}}}", path("config"));
    let unlink = format!("put{{path={}}}", path("unlink"));
    let start = format!("post{{path={}}}", path("status/start"));
    let shutdown = format!("post{{path={}}}", path("status/shutdown"));
    vec![
        (journal.clone(), 1, "none (prepared written)"),
        (config.clone(), 1, "prepared (import)"),
        (journal.clone(), 2, "prepared (imported written)"),
        (shutdown, 1, "imported (shutdown)"),
        (journal.clone(), 3, "imported (switching written)"),
        (unlink.clone(), 1, "switching (staging unlink)"),
        (journal.clone(), 4, "switching (new unused key written)"),
        (unlink.clone(), 2, "switching (old root unlink)"),
        (journal.clone(), 5, "switching (old unused key written)"),
        (config.clone(), 2, "switching (new root attach)"),
        (journal.clone(), 6, "switching (switched written)"),
        (config, 3, "switched (Cloud-Init)"),
        (journal.clone(), 7, "switched (verifying written)"),
        (start, 1, "verifying (start)"),
        (journal.clone(), 8, "verifying (verified written)"),
        (unlink, 3, "verified (old root delete)"),
        (journal, 9, "verified (journal removal)"),
    ]
}

/// Python's outcome for an interruption at `index` of [`changing_calls`]:
/// rolled back before the `verified` journal is written, finished after.
const fn finished_after(index: usize, applied: bool) -> bool {
    index > 14 || (index == 14 && applied)
}

/// A failure of each changing call: the automatic recovery ends where
/// Python's does (rolled back before `verified`, the old root deleted
/// after), and a failed recovery leaves the journal for the next refresh.
#[tokio::test]
async fn a_failure_at_each_step_ends_as_in_python() -> TestResult {
    for (index, (op, occurrence, what)) in changing_calls().into_iter().enumerate() {
        let (sim, pve) = workspace().await?;
        let before = vm_config(&pve).await?;
        let cfg = config()?;
        let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
        sim.inject_fault(fault(&op, occurrence))?;
        let outcome = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await;
        let error = outcome.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(error.contains("injected failure"), "{what}: {error}");
        if index == 0 {
            // Nothing was written yet: nothing to recover.
            assert!(!error.contains("recovery"), "{what}: {error}");
            assert_rolled_back(&sim, &pve, &before, "running", what).await?;
        } else if index >= 15 {
            // The old root's deletion or the journal's removal failed after
            // `verified`: the recovery finishes the cleanup. (When the write
            // of `verified` itself fails, index 14, Python recovers from the
            // journal in its memory, already `verified`; the server from the
            // last one Proxmox kept, `verifying`, and rolls back.)
            assert!(
                error.contains("Automatic recovery succeeded: completed verified cleanup"),
                "{what}: {error}"
            );
            assert_refreshed(&sim, &pve, "running", what).await?;
        } else {
            assert!(
                error.contains("Automatic recovery succeeded: rolled back to original root"),
                "{what}: {error}"
            );
            assert_rolled_back(&sim, &pve, &before, "running", what).await?;
        }
    }
    Ok(())
}

/// The server killed at each changing call, with and without Proxmox having
/// carried the call out (a lost acknowledgement): the next refresh plans a
/// recovery, finds the VM in agreement with the journal, and ends where
/// Python's recovery ends.
#[tokio::test]
async fn a_killed_refresh_is_recovered_as_in_python() -> TestResult {
    for (index, (op, occurrence, what)) in changing_calls().into_iter().enumerate() {
        for applied in [false, true] {
            let what = format!("{what}, applied: {applied}");
            let (sim, pve) = workspace().await?;
            let before = vm_config(&pve).await?;
            interrupt(&sim, &pve, &op, occurrence, applied).await?;
            let cfg = config()?;
            let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
            if index == 0 && !applied {
                // Killed before the first journal reached Proxmox.
                assert_eq!(plan["mode"], "replace", "{what}");
                assert_rolled_back(&sim, &pve, &before, "running", &what).await?;
                continue;
            }
            if index == 16 && applied {
                // Killed after the journal's removal: a finished refresh.
                assert_eq!(plan["mode"], "replace", "{what}");
                assert_refreshed(&sim, &pve, "running", &what).await?;
                continue;
            }
            assert_eq!(plan["mode"], "recover", "{what}: {plan}");
            assert_eq!(plan["mismatches"], json!([]), "{what}");
            let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
            assert_eq!(result["event"], "workspace.refresh_recovered", "{what}");
            if finished_after(index, applied) {
                assert_eq!(result["recovery"], "completed verified cleanup", "{what}");
                assert_refreshed(&sim, &pve, "running", &what).await?;
            } else {
                assert_eq!(result["recovery"], "rolled back to original root", "{what}");
                assert_rolled_back(&sim, &pve, &before, "running", &what).await?;
            }
        }
    }
    Ok(())
}

/// Rule 4 of decision 4: a VM that disagrees with its journal is reported
/// in full and left alone; rule 5: `force` attaches the new root and
/// deletes nothing.
#[tokio::test]
async fn a_disagreeing_vm_is_left_alone_unless_forced() -> TestResult {
    let (sim, pve) = workspace().await?;
    // Killed after the new root was attached (phase switched).
    interrupt(&sim, &pve, &format!("post{{path={}}}", path("config")), 3, false).await?;
    // Someone attaches the old root again by hand, in another slot.
    let admin = client(&sim, ADMIN)?;
    admin
        .post_value(&["nodes", NODE, "qemu", "200", "config"], &[("scsi5", OLD_ROOT)])
        .await?;
    let volumes_before = volumes(&sim).await?;
    let cfg = config()?;
    sim.clear_ops();
    let refused = plan_refresh(&pve, &cfg, POOL, 200, false).await;
    match refused {
        Err(CoreError::Refused(message)) => {
            assert!(message.contains("does not agree with its refresh journal"), "{message}");
            assert!(message.contains(OLD_ROOT), "{message}");
            assert!(message.contains("force"), "{message}");
        }
        other => return Err(format!("expected a refusal, got {other:?}").into()),
    }
    assert!(
        ops(&sim)
            .iter()
            .all(|op| op.starts_with("get{") || op.starts_with("guest_ping{")),
        "{:?}",
        ops(&sim)
    );

    let plan = plan_refresh(&pve, &cfg, POOL, 200, true).await?;
    assert_eq!(plan["force"], true);
    assert_ne!(plan["mismatches"], json!([]));
    let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["recovery"], "forced the new root");
    let after = vm_config(&pve).await?;
    assert_eq!(root(&after), NEW_ROOT);
    assert_eq!(after["scsi5"], OLD_ROOT, "the hand-made attachment stays");
    assert!(after.get("description").is_none());
    assert_eq!(volumes(&sim).await?, volumes_before, "force deletes no volume");
    assert_eq!(status(&pve).await?, "running");
    assert_eq!(guest_file(&sim, MARKER).await?.as_deref(), Some("kept"));
    Ok(())
}

/// The journal shares the description with the owner's notes, which stay;
/// it goes through Python's phases in order.
#[tokio::test]
async fn the_journal_keeps_the_owners_notes_and_pythons_phases() -> TestResult {
    let (sim, pve) = workspace().await?;
    describe(&pve, "owner's notes").await?;
    let cfg = config()?;
    let mut seen = Vec::new();
    for occurrence in 1..=8 {
        let (sim, pve) = workspace().await?;
        describe(&pve, "owner's notes").await?;
        interrupt(&sim, &pve, &format!("put{{path={}}}", path("config")), occurrence, true).await?;
        let config_now = vm_config(&pve).await?;
        let description = config_now["description"].as_str().unwrap_or_default();
        assert!(
            description.starts_with("owner's notes\n<!-- homestack-journal: "),
            "{description}"
        );
        let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
        seen.push(plan["recovery_phase"].as_str().unwrap_or_default().to_owned());
    }
    seen.dedup();
    let phases: Vec<&str> = [
        Phase::Prepared,
        Phase::Imported,
        Phase::Switching,
        Phase::Switched,
        Phase::Verifying,
        Phase::Verified,
    ]
    .iter()
    .map(|phase| phase.as_str())
    .collect();
    assert_eq!(seen, phases);

    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(vm_config(&pve).await?["description"], "owner's notes");
    drop(sim);
    Ok(())
}

/// An unlink cut off inside Proxmox leaves its deletion pending (seen on
/// the cluster when the server was killed): `GET config` no longer shows
/// the disk, but nothing put it in `unusedN` yet. The recovery counts it
/// where Proxmox will put it, applies it, and ends as Python's.
#[tokio::test]
async fn a_pending_unlink_is_finished_by_the_recovery() -> TestResult {
    // The copy's unlink from the staging slot, left pending.
    let (sim, pve) = workspace().await?;
    let before = vm_config(&pve).await?;
    interrupt(&sim, &pve, &format!("put{{path={}}}", path("unlink")), 1, false).await?;
    let staging = vm_config(&pve)
        .await?
        .as_object()
        .into_iter()
        .flatten()
        .find(|(_, value)| value.as_str().is_some_and(|v| v.starts_with(NEW_ROOT)))
        .map(|(key, _)| key.clone())
        .ok_or("no staged copy")?;
    sim.set_pending_delete(200, &staging, false)?;
    assert!(vm_config(&pve).await?.get(&staging).is_none());
    let cfg = config()?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    assert_eq!(plan["recovery_phase"], "switching");
    assert_eq!(plan["mismatches"], json!([]), "{plan}");
    let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["recovery"], "rolled back to original root");
    assert_rolled_back(&sim, &pve, &before, "running", "pending staging unlink").await?;

    // The old root's deletion, left pending after `verified`.
    let (sim, pve) = workspace().await?;
    interrupt(&sim, &pve, &format!("put{{path={}}}", path("unlink")), 3, false).await?;
    let old = unused(&vm_config(&pve).await?)
        .into_iter()
        .find(|entry| entry.ends_with(OLD_ROOT))
        .ok_or("no unused old root")?;
    let key = old.split('=').next().unwrap_or_default().to_owned();
    sim.set_pending_delete(200, &key, true)?;
    let plan = plan_refresh(&pve, &cfg, POOL, 200, false).await?;
    assert_eq!(plan["recovery_phase"], "verified");
    assert_eq!(plan["mismatches"], json!([]), "{plan}");
    let result = refresh_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["recovery"], "completed verified cleanup");
    assert_refreshed(&sim, &pve, "running", "pending old root deletion").await
}
