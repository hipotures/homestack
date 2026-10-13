//! `create` on another node than Gold's (B2, phase 10) on the simulator:
//! decision 3 = A, a full clone on Gold's node migrated offline to the
//! target. The Python tests of `test_create_transfer.py` and the new ones
//! of the variant (`docs/migration/phase-10.md`).

use std::sync::Mutex;
use std::time::Duration;

use hs_core::CoreError;
use hs_core::create_transfer::{capture_gold_metadata, check_migrated_clone};
use hs_core::inventory::VmConfig;
use hs_core::lifecycle::{CreateRequest, LifecycleTiming, create_workspace, plan_create};
use hs_core::tasks::{NoObserver, Observer};
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_model::jobs::JobEvent;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const GOLD_NODE: &str = "pve-example-1";
const TARGET: &str = "pve-example-2";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
/// Sees everything, for checking what is left.
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";

const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
homestack-storage-2 = ["fast"]
homestack-storage-3 = ["local-lvm"]
homestack-storage-4 = ["local-lvm"]
homestack-storage-5 = ["local-lvm"]
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

const CLUSTER: &str = include_str!("data/create-transfer-cluster.toml");

fn config() -> Result<Config, Error> {
    Ok(Config::parse(SERVER_TOML)?)
}

fn client(sim: &PveSim, token: &str) -> Result<PveClient, Error> {
    let mut client = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    client.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(client)?)
}

async fn cluster() -> Result<(PveSim, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    Ok((sim, pve))
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

fn request(node: Option<&str>) -> CreateRequest {
    CreateRequest {
        vmid: 200,
        name: "test1".to_owned(),
        home_size: None,
        storage: None,
        node: node.map(str::to_owned),
    }
}

fn ops(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

fn fault(op: &str, task: Option<&str>) -> FaultSpec {
    FaultSpec {
        op: op.to_owned(),
        occurrence: 1,
        status: 500,
        message: "injected migration failure".to_owned(),
        applied: false,
        hang: false,
        task: task.map(str::to_owned),
    }
}

/// What the simulator holds for VMID 200 on any node.
async fn traces(sim: &PveSim) -> Result<Vec<String>, Error> {
    let pve = client(sim, ADMIN)?;
    let mut found = Vec::new();
    let resources = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    for vm in resources.as_array().into_iter().flatten() {
        if vm.get("vmid").and_then(Value::as_u64) == Some(200) {
            found.push(format!("VM 200 on {}", vm["node"].as_str().unwrap_or_default()));
        }
    }
    for (node, storage) in [(GOLD_NODE, "local-lvm"), (TARGET, "fast")] {
        let volumes = pve
            .get_value(&["nodes", node, "storage", storage, "content"], &[])
            .await?;
        for volume in volumes.as_array().into_iter().flatten() {
            let volid = volume.get("volid").and_then(Value::as_str).unwrap_or_default();
            if volid.contains("vm-200-") {
                found.push(format!("{node} {volid}"));
            }
        }
    }
    Ok(found)
}

struct Steps(Mutex<Vec<(String, Option<bool>)>>);

impl Observer for Steps {
    fn event(&self, event: JobEvent) {
        if let Ok(mut steps) = self.0.lock() {
            match event {
                JobEvent::StepStarted { name } => steps.push((name, None)),
                JobEvent::StepFinished { name, ok } => {
                    if let Some(step) = steps.iter_mut().rev().find(|(n, done)| *n == name && done.is_none()) {
                        step.1 = Some(ok);
                    }
                }
                _ => {}
            }
        }
    }
}

impl Steps {
    fn names(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|steps| steps.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    }
}

fn gold_config() -> VmConfig {
    [
        ("tags", "homestack-gold"),
        ("scsi0", "source-storage:vm-101-disk-0,size=16G"),
        ("ide2", "source-storage:vm-101-cloudinit,media=cdrom"),
        ("net0", "virtio=02:00:00:00:01:01,bridge=vmbr0"),
        ("boot", "order=scsi0;ide2;net0"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn vm_config(pairs: &[(&str, &str)]) -> VmConfig {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The plan `_verify_streamed_restore` is tested with in Python.
fn restore_plan() -> Value {
    json!({
        "vmid": 200,
        "node": "example-node-2",
        "name": "test1",
        "root_storage": "example-storage-b",
        "source_metadata": {
            "root_config": "example-storage-a:vm-101-disk-0,size=16G,format=qcow2,cache=none",
            "cloudinit_slots": ["ide2"],
            "bridges": {"net0": "vmbr0"},
            "volumes": [
                {"slot": "scsi0", "volume": "example-storage-a:vm-101-disk-0", "storage": "example-storage-a"},
                {"slot": "ide2", "volume": "example-storage-a:vm-101-cloudinit", "storage": "example-storage-a"},
            ],
        },
    })
}

// --- ports of the Python tests ---------------------------------------------

/// `CrossNodeValidationTests::test_validates_online_target_and_captures_safe_metadata`
#[tokio::test]
async fn validates_online_target_and_captures_safe_metadata() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan_create(&pve, &config()?, POOL, &request(Some(TARGET))).await?;
    assert_eq!(plan["root_storage"], "fast");
    let metadata = &plan["source_metadata"];
    assert_eq!(metadata["root_disk"], "scsi0");
    assert_eq!(metadata["cloudinit_slots"], json!(["ide2"]));
    assert_eq!(metadata["bridges"], json!({"net0": "vmbr0"}));
    assert!(metadata.get("tags").is_none(), "{metadata}");
    let ops = ops(&sim);
    assert!(
        ops.iter().all(|op| op.starts_with("get{")),
        "planning changes nothing: {ops:?}"
    );
    Ok(())
}

/// `CrossNodeValidationTests::test_rejects_offline_target_before_storage_mutation`:
/// nothing on the target node is read after its status.
#[tokio::test]
async fn rejects_offline_target_before_storage_mutation() -> TestResult {
    let (sim, pve) = cluster().await?;
    let result = plan_create(&pve, &config()?, POOL, &request(Some("pve-example-3"))).await;
    match result {
        Err(CoreError::Refused(message)) => assert_eq!(message, "Target node 'pve-example-3' is offline"),
        other => return Err(format!("expected a refusal, got {other:?}").into()),
    }
    let ops = ops(&sim);
    assert!(
        !ops.iter().any(|op| op.contains("/nodes/pve-example-3/")),
        "the offline node is not read: {ops:?}"
    );
    Ok(())
}

/// `CrossNodeValidationTests::test_rejects_extra_disk_and_unbacked_required_devices`,
/// the extra disks; `backup=0` is no longer refused (a migration copies
/// every disk).
#[test]
fn rejects_extra_disk_and_unbacked_required_devices() -> TestResult {
    let mut extra = gold_config();
    extra.insert("scsi1".to_owned(), "source-storage:vm-101-data,size=1G".to_owned());
    match capture_gold_metadata(101, &extra, "scsi0") {
        Err(CoreError::Failed(message)) => {
            assert_eq!(message, "Gold VM 101 has unsupported extra data disks: scsi1");
        }
        other => return Err(format!("expected a failure, got {other:?}").into()),
    }
    let mut unused = gold_config();
    unused.insert("unused0".to_owned(), "source-storage:vm-101-disk-9".to_owned());
    assert!(matches!(
        capture_gold_metadata(101, &unused, "scsi0"),
        Err(CoreError::Failed(message)) if message.ends_with("extra data disks: unused0")
    ));
    let mut unbacked = gold_config();
    unbacked.insert(
        "scsi0".to_owned(),
        "source-storage:vm-101-disk-0,size=16G,backup=0".to_owned(),
    );
    assert!(capture_gold_metadata(101, &unbacked, "scsi0").is_ok());
    let mut no_drive = gold_config();
    no_drive.remove("ide2");
    assert!(matches!(
        capture_gold_metadata(101, &no_drive, "scsi0"),
        Err(CoreError::Failed(message)) if message == "Gold VM 101 has no Cloud-Init drive"
    ));
    Ok(())
}

/// `CreatePlanTests::test_default_target_preserves_clone_plan_and_source_metadata`
#[tokio::test]
async fn default_target_preserves_clone_plan_and_source_metadata() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let cfg = config()?;
    for node in [None, Some(GOLD_NODE), Some(" pve-example-1 ")] {
        let plan = plan_create(&pve, &cfg, POOL, &request(node)).await?;
        assert_eq!(plan["node"], GOLD_NODE);
        assert_eq!(plan["source_node"], GOLD_NODE);
        assert_eq!(plan["transfer_method"], "clone");
        assert!(plan.get("source_metadata").is_none(), "{plan}");
    }
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(Some("  "))).await,
        Err(CoreError::Invalid(message)) if message == "Create target node cannot be empty"
    ));
    Ok(())
}

/// `CreatePlanTests::test_target_uses_stream_and_target_storage`: the
/// clone is migrated (decision 3 = A).
#[tokio::test]
async fn target_uses_migration_and_target_storage() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(Some(TARGET))).await?;
    assert_eq!(plan["node"], TARGET);
    assert_eq!(plan["source_node"], GOLD_NODE);
    assert_eq!(plan["transfer_method"], "clone-migrate");
    assert_eq!(plan["clone_storage"], "local-lvm");
    assert_eq!(plan["root_storage"], "fast");
    assert_eq!(plan["home_storage"], "fast");
    assert_eq!(plan["root_disk_gb"], 16.0);

    let mut other = request(Some(TARGET));
    other.storage = Some("local-lvm".to_owned());
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &other).await,
        Err(CoreError::Invalid(message)) if message.starts_with("Storage 'local-lvm' is not assigned to node 'pve-example-2'")
    ));
    Ok(())
}

/// `RestoreVerificationTests::test_source_disk_change_requires_a_new_plan`:
/// Gold changed after the plan; nothing is cloned.
#[tokio::test]
async fn source_disk_change_requires_a_new_plan() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(Some(TARGET))).await?;
    let admin = client(&sim, ADMIN)?;
    admin
        .post_value(
            &["nodes", GOLD_NODE, "qemu", "101", "config"],
            &[("scsi0", "local-lvm:vm-101-disk-0,cache=none,iothread=1,size=16G")],
        )
        .await?;
    let before = ops(&sim).len();
    match create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Refused(message)) => assert_eq!(
            message,
            "Gold VM source configuration changed after create confirmation; re-run create and review the new plan"
        ),
        other => return Err(format!("expected a refusal, got {other:?}").into()),
    }
    let after: Vec<String> = ops(&sim).into_iter().skip(before).collect();
    assert!(
        after.iter().all(|op| op.starts_with("get{")),
        "nothing is changed: {after:?}"
    );
    Ok(())
}

/// `RestoreVerificationTests::test_unplanned_home_disk_is_rejected_before_allocation`
#[test]
fn unplanned_home_disk_is_rejected_before_allocation() -> TestResult {
    let restored = vm_config(&[
        ("name", "test1"),
        ("boot", "order=scsi0;ide2;net0"),
        ("scsi0", "example-storage-b:vm-200-disk-0,cache=none,size=16G"),
        ("ide2", "example-storage-b:vm-200-cloudinit,media=cdrom"),
        ("scsi1", "example-storage-b:vm-200-disk-1,size=500G"),
        ("net0", "virtio=02:00:00:00:02:00,bridge=vmbr0"),
    ]);
    match check_migrated_clone(&config()?, &restore_plan(), "example-node-2", "stopped", &restored) {
        Err(CoreError::Failed(message)) => assert!(message.contains("unexpected scsi1"), "{message}"),
        other => return Err(format!("expected a failure, got {other:?}").into()),
    }
    Ok(())
}

/// `RestoreVerificationTests::test_accepts_target_owned_volumes_and_stopped_vm`
#[test]
fn accepts_target_owned_volumes_and_stopped_vm() -> TestResult {
    let restored = vm_config(&[
        ("name", "test1"),
        ("template", "0"),
        ("boot", "order=scsi0;ide2;net0"),
        ("scsi0", "example-storage-b:vm-200-disk-0,cache=none,size=16G"),
        ("ide2", "example-storage-b:vm-200-cloudinit,media=cdrom"),
        ("net0", "virtio=02:00:00:00:02:00,bridge=vmbr0"),
    ]);
    check_migrated_clone(&config()?, &restore_plan(), "example-node-2", "stopped", &restored)?;
    assert!(matches!(
        check_migrated_clone(&config()?, &restore_plan(), "example-node-2", "running", &restored),
        Err(CoreError::Failed(message)) if message.contains("with status 'running'; expected stopped")
    ));
    assert!(matches!(
        check_migrated_clone(&config()?, &restore_plan(), "example-node-1", "stopped", &restored),
        Err(CoreError::Failed(message)) if message.contains("is on 'example-node-1', expected 'example-node-2'")
    ));
    Ok(())
}

/// `RestoreVerificationTests::test_rejects_source_volume_ownership`
#[test]
fn rejects_source_volume_ownership() -> TestResult {
    let mut plan = restore_plan();
    plan["root_storage"] = json!("example-storage-a");
    let restored = vm_config(&[
        ("name", "test1"),
        ("boot", "order=scsi0;ide2"),
        (
            "scsi0",
            "example-storage-a:vm-101-disk-0,size=16G,format=qcow2,cache=none",
        ),
        ("ide2", "example-storage-b:vm-200-cloudinit,media=cdrom"),
        ("net0", "virtio=02:00:00:00:02:00,bridge=vmbr0"),
    ]);
    match check_migrated_clone(&config()?, &plan, "example-node-2", "stopped", &restored) {
        Err(CoreError::Failed(message)) => assert!(message.contains("source volume"), "{message}"),
        other => return Err(format!("expected a failure, got {other:?}").into()),
    }
    Ok(())
}

/// `CreateRestoreFailureTests::test_stream_failure_does_not_allocate_home_or_start`:
/// a failed migration allocates no home and starts nothing; the clone it
/// left on Gold's node is removed.
#[tokio::test]
async fn migration_failure_does_not_allocate_home_or_start() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(Some(TARGET))).await?;
    sim.inject_fault(fault(
        &format!("post{{path=/nodes/{GOLD_NODE}/qemu/200/migrate}}"),
        Some("qmigrate"),
    ))?;
    match create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Failed(message)) => {
            assert!(message.contains("injected migration failure"), "{message}");
            assert!(!message.contains("Cleanup report"), "{message}");
        }
        other => return Err(format!("expected a failure, got {other:?}").into()),
    }
    let ops = ops(&sim);
    assert!(
        !ops.iter()
            .any(|op| op.contains("/storage/fast/content}") && op.starts_with("post")),
        "no home: {ops:?}"
    );
    assert!(!ops.iter().any(|op| op.contains("/status/start")), "no start: {ops:?}");
    assert_eq!(traces(&sim).await?, Vec::<String>::new());
    Ok(())
}

// --- new tests of the variant ----------------------------------------------

/// The whole create on another node: cloned on Gold's node, migrated, the
/// home on the target storage, started and verified there; nothing left on
/// Gold's node.
#[tokio::test]
async fn create_on_another_node_clones_migrates_and_verifies_there() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(Some(TARGET))).await?;
    let steps = Steps(Mutex::new(Vec::new()));
    let result = create_workspace(&pve, &cfg, &plan, &quick(), &steps).await?;
    assert_eq!(
        steps.names(),
        [
            "SSH keys",
            "Full clone",
            "Migrate to target node",
            "Verify migrated clone",
            "Persistent home disk",
            "Clone identity",
            "Cloud-Init",
            "Boot",
            "Verify workspace",
        ]
    );
    assert_eq!(result["node"], TARGET);
    assert_eq!(result["status"], "running");
    assert_eq!(result["event"], "workspace.created");
    let mut left = traces(&sim).await?;
    left.sort();
    assert_eq!(
        left,
        [
            "VM 200 on pve-example-2",
            "pve-example-2 fast:vm-200-cloudinit",
            "pve-example-2 fast:vm-200-disk-0",
            "pve-example-2 fast:vm-200-hs-home-developer",
        ]
    );
    let ops = ops(&sim);
    let clone = format!("post{{path=/nodes/{GOLD_NODE}/qemu/101/clone}}");
    let migrate = format!("post{{path=/nodes/{GOLD_NODE}/qemu/200/migrate}}");
    let home = format!("post{{path=/nodes/{TARGET}/storage/fast/content}}");
    let start = format!("post{{path=/nodes/{TARGET}/qemu/200/status/start}}");
    let at = |wanted: &str| ops.iter().position(|op| op == wanted);
    let order = [at(&clone), at(&migrate), at(&home), at(&start)];
    assert!(order.iter().all(Option::is_some), "{ops:?}");
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{ops:?}");
    Ok(())
}

/// A failure after the migration and before the start destroys the clone
/// on the target node.
#[tokio::test]
async fn a_failure_after_the_migration_removes_the_clone_on_the_target() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(Some(TARGET))).await?;
    sim.inject_fault(fault(
        &format!("post{{path=/nodes/{TARGET}/storage/fast/content}}"),
        None,
    ))?;
    match create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await {
        Err(error) => assert!(!error.to_string().contains("Cleanup report"), "{error}"),
        Ok(result) => return Err(format!("create succeeded: {result}").into()),
    }
    assert_eq!(traces(&sim).await?, Vec::<String>::new());
    Ok(())
}

/// A target without Gold's bridge, or whose storage is disabled, is
/// refused when planning.
#[tokio::test]
async fn targets_without_the_bridge_or_the_storage_are_refused() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let cfg = config()?;
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(Some("pve-example-4"))).await,
        Err(CoreError::Refused(message)) if message == "Target node 'pve-example-4' is missing Gold network bridge(s): vmbr0"
    ));
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(Some("pve-example-5"))).await,
        Err(CoreError::Refused(message)) if message == "Target storage 'local-lvm' is disabled or inactive on node"
    ));
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(Some("pve-example-9"))).await,
        Err(CoreError::Refused(message)) if message == "Target node 'pve-example-9' is not in the Proxmox cluster"
    ));
    Ok(())
}
