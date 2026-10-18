//! `migrate` (B6) on the simulator, and the Python tests of phase 8
//! (`docs/migration/phase-8.md`): `MigrationProgressTests` and
//! `MigrationPowerStateTests` of `test_lifecycle.py`, and the migrate parts
//! of `test_alien_workspace.py`.
//!
//! The cluster (`data/migrate-cluster.toml`) has three nodes; the
//! workspaces start on pve-example-1.

use std::sync::Mutex;
use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::{LifecycleTiming, plan_destroy};
use hs_core::migrate::{
    MigrateRequest, MigrationTracker, migrate_workspace, migration_volume_key, migration_volume_plan, plan_migrate,
};
use hs_core::resize::{ResizeRequest, plan_resize};
use hs_core::tasks::{NoObserver, Observer};
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_model::jobs::JobEvent;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error>;
type TestResult = Result<(), Error>;

const SOURCE: &str = "pve-example-1";
const TARGET: &str = "pve-example-2";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";
const CLUSTER: &str = include_str!("data/migrate-cluster.toml");
const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
homestack-storage-2 = ["local-lvm", "fast"]
homestack-storage-3 = ["local-lvm"]
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

const GIB: u64 = 1024 * 1024 * 1024;

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

async fn cluster() -> Result<(PveSim, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    Ok((sim, pve))
}

fn request(vmid: u32, target: &str, storage: Option<&str>) -> MigrateRequest {
    MigrateRequest {
        vmid,
        target_node: target.to_owned(),
        target_storage: storage.map(str::to_owned),
    }
}

fn ops(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

/// The calls that may change something: not `GET` and not the agent's ping
/// (the home usage of `resolve_existing_workspace` reads through it).
fn changes(sim: &PveSim) -> Vec<String> {
    sim.ops()
        .iter()
        .filter(|op| op.name != "get" && op.name != "guest_ping")
        .map(ToString::to_string)
        .collect()
}

fn fault(op: &str) -> FaultSpec {
    FaultSpec {
        op: op.to_owned(),
        occurrence: 1,
        status: 500,
        message: format!("injected failure of {op}"),
        applied: false,
        hang: false,
        task: None,
    }
}

async fn plan(pve: &PveClient, request: &MigrateRequest) -> Result<Value, CoreError> {
    plan_migrate(
        pve,
        &config().map_err(|e| CoreError::Invalid(e.to_string()))?,
        POOL,
        request,
    )
    .await
}

async fn apply(pve: &PveClient, plan: &Value, observer: &impl Observer) -> Result<Value, CoreError> {
    migrate_workspace(
        pve,
        &config().map_err(|e| CoreError::Invalid(e.to_string()))?,
        POOL,
        plan,
        &quick(),
        observer,
    )
    .await
}

/// `(node, status)` of a VM in `cluster/resources`.
async fn placed(pve: &PveClient, vmid: u32) -> Result<(String, String), Error> {
    let list = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    let vm = list
        .as_array()
        .into_iter()
        .flatten()
        .find(|vm| vm["vmid"].as_u64() == Some(u64::from(vmid)))
        .ok_or("VM not listed")?;
    Ok((
        vm["node"].as_str().unwrap_or_default().to_owned(),
        vm["status"].as_str().unwrap_or_default().to_owned(),
    ))
}

async fn vm_config(pve: &PveClient, node: &str, vmid: u32) -> Result<Value, Error> {
    let id = vmid.to_string();
    Ok(pve.get_value(&["nodes", node, "qemu", &id, "config"], &[]).await?)
}

#[derive(Default)]
struct Recorder(Mutex<Vec<JobEvent>>);

impl Observer for Recorder {
    fn event(&self, event: JobEvent) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

impl Recorder {
    fn progress(&self) -> Vec<(f64, String)> {
        self.0
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| match event {
                        JobEvent::Progress { percent, detail } => Some((*percent, detail.clone())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// --- MigrationProgressTests ---------------------------------------------------

#[test]
fn tracker_reports_current_disk_copy_progress() -> TestResult {
    let volumes = [
        json!({"slot": "ide0", "role": "cloud-init", "volume": "example-storage-b:vm-200-cloudinit", "name": "vm-200-cloudinit", "size_bytes": 4 * 1024 * 1024}),
        json!({"slot": "scsi1", "role": "home", "volume": "example-storage-b:vm-200-hs-home-user", "name": "vm-200-hs-home-user", "size_bytes": 20 * GIB}),
        json!({"slot": "scsi0", "role": "root", "volume": "example-storage-b:vm-200-hs-root-default", "name": "vm-200-hs-root-default", "size_bytes": 16 * GIB}),
    ];
    let mut tracker = MigrationTracker::new(&volumes);
    let first = tracker
        .update(
            "successfully imported 'example-storage-b:vm-200-cloudinit'\nLogical volume \"vm-200-hs-home-user\" created.\n8711372800 bytes (8.7 GB, 8.1 GiB) copied, 30 s, 290 MB/s\n",
        )
        .ok_or("progress")?;
    assert_eq!(first.role, "home");
    assert_eq!(first.copied_bytes, 8_711_372_800);
    let percent = first.percent.ok_or("percent")?;
    assert!(percent > 40.0 && percent < 41.0, "{percent}");
    assert!(first.description.contains("290 MB/s"), "{}", first.description);
    let second = tracker
        .update(
            "successfully imported 'example-storage-b:vm-200-hs-home-user'\nLogical volume \"vm-200-hs-root-default\" created.\n4294967296 bytes (4.3 GB, 4.0 GiB) copied, 15 s, 286 MB/s\n",
        )
        .ok_or("progress")?;
    assert_eq!(second.role, "root");
    let percent = second.percent.ok_or("percent")?;
    assert!(percent > 24.0 && percent < 26.0, "{percent}");
    Ok(())
}

#[test]
fn migration_volume_plan_ignores_unused_refs() -> TestResult {
    let cfg = Config::parse(&SERVER_TOML.replace("developer", "user"))?;
    let vm_cfg = [
        ("ide0", "example-storage-a:vm-200-cloudinit,media=cdrom,size=4M"),
        ("scsi0", "example-storage-a:vm-200-hs-root-default,size=16G"),
        (
            "scsi1",
            "example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200,size=20G",
        ),
        ("unused0", "example-storage-a:vm-200-disk-0"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    let plan = migration_volume_plan(&vm_cfg, &cfg);
    let roles: Vec<(&str, &str)> = plan
        .iter()
        .map(|item| (item["role"].as_str().unwrap_or(""), item["name"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        roles,
        [
            ("cloud-init", "vm-200-cloudinit"),
            ("root", "vm-200-hs-root-default"),
            ("home", "vm-200-hs-home-user")
        ]
    );
    let total: u64 = plan.iter().filter_map(|item| item["size_bytes"].as_u64()).sum();
    assert_eq!(total, 36 * GIB + 4 * 1024 * 1024);
    Ok(())
}

/// The owner's decision of 2026-10-13: the extension goes, unlike Python.
#[test]
fn volume_key_strips_the_file_extension() {
    assert_eq!(migration_volume_key("local:200/vm-200-disk-0.raw"), "vm-200-disk-0");
    assert_eq!(migration_volume_key("local:200/vm-200-disk-1.qcow2"), "vm-200-disk-1");
    assert_eq!(migration_volume_key("nfs:200/vm-200-disk-2.vmdk"), "vm-200-disk-2");
    assert_eq!(
        migration_volume_key("local-lvm:vm-200-hs-home-user"),
        "vm-200-hs-home-user"
    );
    assert_eq!(migration_volume_key("local:200/vm-200-raw"), "vm-200-raw");
}

#[test]
fn tracker_matches_a_file_volume_to_its_logical_volume() -> TestResult {
    let volumes = [
        json!({"slot": "scsi1", "role": "home", "volume": "local:200/vm-200-disk-1.raw", "name": migration_volume_key("local:200/vm-200-disk-1.raw"), "size_bytes": 20 * GIB}),
    ];
    let mut tracker = MigrationTracker::new(&volumes);
    let progress = tracker
        .update(
            "  Logical volume \"vm-200-disk-1\" created.\n10737418240 bytes (11 GB, 10 GiB) copied, 30 s, 358 MB/s\n",
        )
        .ok_or("progress")?;
    assert_eq!(progress.role, "home");
    assert_eq!(progress.total_bytes, Some(20 * GIB));
    assert_eq!(progress.percent, Some(50.0));
    assert_eq!(progress.description, "  ↳ home · 10.0 GiB / 20.0 GiB · 358 MB/s");
    Ok(())
}

#[test]
fn tracker_without_a_size_reports_the_bytes_only() -> TestResult {
    let mut tracker = MigrationTracker::new(&[]);
    assert!(tracker.update("starting migration of VM 200\n").is_none());
    let progress = tracker
        .update("  Logical volume \"vm-200-disk-9\" created.\n1048576 bytes (1.0 MB, 1.0 MiB) copied, 1 s, 1.0 MB/s\n")
        .ok_or("progress")?;
    assert_eq!(progress.role, "disk");
    assert_eq!(progress.percent, None);
    assert_eq!(progress.description, "  ↳ disk · 1.0 MiB · 1.0 MB/s");
    Ok(())
}

// --- test_alien_workspace.py ----------------------------------------------------

#[tokio::test]
async fn resize_destroy_and_migrate_plans_reject_alien_before_guest_or_disk_work() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cfg = config()?;
    let resize = ResizeRequest {
        vmid: 160,
        root_size: Some("32G".to_owned()),
        home_size: None,
    };
    let errors = [
        plan_resize(&pve, &cfg, POOL, &resize).await.err(),
        plan_destroy(&pve, &cfg, POOL, 160).await.err(),
        plan(&pve, &request(160, TARGET, None)).await.err(),
    ];
    for error in errors {
        let error = error.ok_or("expected an error")?;
        assert!(error.to_string().contains("forbidden workspace role tag"), "{error}");
    }
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn execution_rejects_alien_before_shutdown() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = json!({
        "vmid": 160, "name": "alien-workspace", "source_node": SOURCE, "target_node": TARGET,
        "target_storage": "local-lvm", "status": "running", "home_label": "HS_HOME_160", "volumes": [],
    });
    let error = apply(&pve, &plan, &NoObserver).await.err().ok_or("expected an error")?;
    assert!(error.to_string().contains("forbidden workspace role tag"), "{error}");
    assert_eq!(changes(&sim), Vec::<String>::new());
    assert_eq!(placed(&pve, 160).await?, (SOURCE.to_owned(), "running".to_owned()));
    Ok(())
}

// --- MigrationPowerStateTests -----------------------------------------------------

#[tokio::test]
async fn stopped_workspace_stays_stopped_after_migration() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(201, TARGET, None)).await?;
    assert_eq!(plan["status"], "stopped");
    assert_eq!(plan["target_storage"], "local-lvm");
    let result = apply(&pve, &plan, &NoObserver).await?;
    assert_eq!(result["status"], "stopped");
    assert_eq!(result["power_state_preserved"], true);
    assert_eq!(result["home_preserved"], true);
    assert_eq!(result["event"], "workspace.migrated");
    let calls = ops(&sim);
    assert!(
        !calls
            .iter()
            .any(|op| op.contains("/status/start") || op.contains("/status/shutdown")),
        "{calls:?}"
    );
    assert_eq!(placed(&pve, 201).await?, (TARGET.to_owned(), "stopped".to_owned()));
    Ok(())
}

#[tokio::test]
async fn running_workspace_is_started_after_migration() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, None)).await?;
    assert_eq!(plan["status"], "running");
    let recorder = Recorder::default();
    let result = apply(&pve, &plan, &recorder).await?;
    assert_eq!(result["status"], "running");
    assert_eq!(result["power_state_preserved"], true);
    let calls = changes(&sim);
    let position = |needle: &str| calls.iter().position(|op| op.contains(needle));
    let shutdown = position(&format!("/nodes/{SOURCE}/qemu/200/status/shutdown")).ok_or("no shutdown")?;
    let migrate = position(&format!("/nodes/{SOURCE}/qemu/200/migrate")).ok_or("no migrate")?;
    let start = position(&format!("/nodes/{TARGET}/qemu/200/status/start")).ok_or("no start on target")?;
    assert!(shutdown < migrate && migrate < start, "{calls:?}");
    assert_eq!(placed(&pve, 200).await?, (TARGET.to_owned(), "running".to_owned()));
    let config = vm_config(&pve, TARGET, 200).await?;
    assert_eq!(
        config["scsi1"],
        "local-lvm:vm-200-hs-home-developer,serial=HS_HOME_200,size=20G"
    );
    let progress = recorder.progress();
    assert!(
        progress
            .iter()
            .any(|(percent, detail)| *percent == 100.0 && detail.starts_with("home · 20.0 GiB / 20.0 GiB")),
        "{progress:?}"
    );
    Ok(())
}

// --- plan -------------------------------------------------------------------------

#[tokio::test]
async fn plan_lists_the_volumes_and_the_power_state() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, Some("fast"))).await?;
    assert_eq!(plan["command"], "migrate");
    assert_eq!(plan["name"], "alpha");
    assert_eq!(plan["source_node"], SOURCE);
    assert_eq!(plan["target_node"], TARGET);
    assert_eq!(plan["target_storage"], "fast");
    assert_eq!(plan["home_disk"], "scsi1");
    assert_eq!(plan["home_label"], "HS_HOME_200");
    assert_eq!(plan["home_volume"], "local-lvm:vm-200-hs-home-developer");
    let roles: Vec<&str> = plan["volumes"]
        .as_array()
        .ok_or("no volumes")?
        .iter()
        .filter_map(|v| v["role"].as_str())
        .collect();
    assert_eq!(roles, ["cloud-init", "root", "home"]);
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn plan_refusals() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cases: [(MigrateRequest, &str); 5] = [
        (request(200, SOURCE, None), "VM 200 is already on node pve-example-1"),
        (
            request(202, TARGET, None),
            "VM 202 has detached/unused disk references (unused0=local-lvm:vm-202-disk-1); Proxmox migration scans them too. Clean or resolve them before migration.",
        ),
        (
            request(200, TARGET, Some("local")),
            "Storage 'local' is not assigned to node 'pve-example-2'; allowed HomeStack storages: local-lvm, fast",
        ),
        (
            request(200, "pve-example-3", None),
            "Target node 'pve-example-3' is not available in the Proxmox cluster",
        ),
        (
            request(200, "pve-example-4", None),
            "No HomeStack storages are assigned to node 'pve-example-4' via homestack-storage-4",
        ),
    ];
    for (request, message) in cases {
        let error = plan(&pve, &request).await.err().ok_or("expected an error")?;
        assert_eq!(error.to_string(), message);
    }
    let error = plan(&pve, &request(150, TARGET, None))
        .await
        .err()
        .ok_or("expected an error")?;
    assert!(matches!(error, CoreError::NotFound(_)), "{error}");
    let error = plan(&pve, &request(101, TARGET, None))
        .await
        .err()
        .ok_or("expected an error")?;
    assert!(matches!(error, CoreError::OutsidePool(_)), "{error}");
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

// --- execution --------------------------------------------------------------------

#[tokio::test]
async fn the_requested_storage_receives_every_disk() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, Some("fast"))).await?;
    apply(&pve, &plan, &NoObserver).await?;
    let config = vm_config(&pve, TARGET, 200).await?;
    assert_eq!(config["scsi0"], "fast:vm-200-disk-0,iothread=1,size=16G");
    assert_eq!(
        config["scsi1"],
        "fast:vm-200-hs-home-developer,serial=HS_HOME_200,size=20G"
    );
    assert_eq!(config["ide2"], "fast:vm-200-cloudinit,media=cdrom");
    Ok(())
}

#[tokio::test]
async fn a_changed_workspace_is_refused_before_the_shutdown() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(201, TARGET, None)).await?;
    // Someone else moves it first.
    let admin = client(&sim, ADMIN)?;
    let upid = admin
        .post_value(
            &["nodes", SOURCE, "qemu", "201", "migrate"],
            &[("target", "pve-example-2"), ("targetstorage", "local-lvm")],
        )
        .await?;
    assert!(upid.as_str().is_some());
    sim.clear_ops();
    let error = apply(&pve, &plan, &NoObserver).await.err().ok_or("expected an error")?;
    assert_eq!(
        error.to_string(),
        "VM 201 is on pve-example-2, not on pve-example-1 as planned; generate a new plan"
    );
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn a_failed_migration_starts_the_vm_again_on_the_source() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, None)).await?;
    sim.inject_fault(fault(&format!("post{{path=/nodes/{SOURCE}/qemu/200/migrate}}")))?;
    let error = apply(&pve, &plan, &NoObserver)
        .await
        .err()
        .ok_or("expected an error")?
        .to_string();
    assert!(error.contains("injected failure"), "{error}");
    assert!(
        error.ends_with("\nRecovery: VM 200 was restarted automatically on pve-example-1."),
        "{error}"
    );
    assert_eq!(placed(&pve, 200).await?, (SOURCE.to_owned(), "running".to_owned()));
    let calls = changes(&sim);
    assert!(
        calls
            .iter()
            .any(|op| op.contains(&format!("/nodes/{SOURCE}/qemu/200/status/start"))),
        "{calls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_restart_on_the_source_is_reported() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, None)).await?;
    sim.inject_fault(fault(&format!("post{{path=/nodes/{SOURCE}/qemu/200/migrate}}")))?;
    sim.inject_fault(fault(&format!("post{{path=/nodes/{SOURCE}/qemu/200/status/start}}")))?;
    let error = apply(&pve, &plan, &NoObserver)
        .await
        .err()
        .ok_or("expected an error")?
        .to_string();
    assert!(error.contains("injected failure"), "{error}");
    assert!(
        error.contains("\nRecovery: VM 200 is still on pve-example-1, but automatic restart failed: "),
        "{error}"
    );
    assert_eq!(placed(&pve, 200).await?, (SOURCE.to_owned(), "stopped".to_owned()));
    Ok(())
}

#[tokio::test]
async fn a_failure_on_the_target_leaves_the_vm_there() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &request(200, TARGET, None)).await?;
    sim.inject_fault(fault(&format!("post{{path=/nodes/{TARGET}/qemu/200/status/start}}")))?;
    let error = apply(&pve, &plan, &NoObserver)
        .await
        .err()
        .ok_or("expected an error")?
        .to_string();
    assert!(error.contains("injected failure"), "{error}");
    assert!(!error.contains("Recovery"), "{error}");
    assert_eq!(placed(&pve, 200).await?, (TARGET.to_owned(), "stopped".to_owned()));
    let calls = changes(&sim);
    assert!(
        !calls
            .iter()
            .any(|op| op.contains(&format!("/nodes/{SOURCE}/qemu/200/status/start"))),
        "{calls:?}"
    );
    Ok(())
}

// Phase 17: capacity refusals must happen before shutting the source down.

#[tokio::test]
async fn insufficient_capacity_is_refused_during_planning_without_changes() -> TestResult {
    let (sim, pve) = cluster().await?;
    let before = vm_config(&pve, SOURCE, 203).await?;
    sim.clear_ops();
    let error = plan(&pve, &request(203, TARGET, None))
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(matches!(error, CoreError::Refused(_)), "{error}");
    assert!(error.to_string().contains("Not enough free space"), "{error}");
    assert!(changes(&sim).is_empty(), "{:?}", changes(&sim));
    assert_eq!(placed(&pve, 203).await?, (SOURCE.to_owned(), "running".to_owned()));
    assert_eq!(vm_config(&pve, SOURCE, 203).await?, before);
    Ok(())
}

#[tokio::test]
async fn capacity_counts_all_disks_and_accepts_the_exact_total() -> TestResult {
    let (sim, pve) = cluster().await?;
    let required = 36 * GIB + 4 * 1024 * 1024;
    sim.set_storage_available(TARGET, "fast", required - 1)?;
    let error = plan(&pve, &request(200, TARGET, Some("fast")))
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(
        error.to_string().contains(&format!("requires {required} bytes")),
        "{error}"
    );
    assert!(changes(&sim).is_empty());
    sim.set_storage_available(TARGET, "fast", required)?;
    let planned = plan(&pve, &request(200, TARGET, Some("fast"))).await?;
    assert_eq!(
        planned["capacity"],
        json!({"required_bytes":required,"available_bytes":required})
    );
    apply(&pve, &planned, &NoObserver).await?;
    assert_eq!(placed(&pve, 200).await?, (TARGET.to_owned(), "running".to_owned()));
    Ok(())
}

#[tokio::test]
async fn capacity_is_checked_again_after_confirmation_before_shutdown() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &request(200, TARGET, Some("fast"))).await?;
    sim.set_storage_available(TARGET, "fast", 0)?;
    sim.clear_ops();
    let error = apply(&pve, &planned, &NoObserver)
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(error.to_string().contains("available 0 bytes"), "{error}");
    assert!(changes(&sim).is_empty(), "{:?}", changes(&sim));
    assert_eq!(placed(&pve, 200).await?, (SOURCE.to_owned(), "running".to_owned()));
    Ok(())
}

#[tokio::test]
async fn an_unreadable_capacity_is_not_permission_to_shutdown() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &request(200, TARGET, None)).await?;
    sim.inject_fault(fault(&format!("get{{path=/nodes/{TARGET}/storage/local-lvm/status}}")))?;
    sim.clear_ops();
    assert!(apply(&pve, &planned, &NoObserver).await.is_err());
    assert!(changes(&sim).is_empty());
    assert_eq!(placed(&pve, 200).await?, (SOURCE.to_owned(), "running".to_owned()));
    Ok(())
}

#[tokio::test]
async fn missing_disk_sizes_are_refused_and_shared_disks_are_not_counted() -> TestResult {
    let mut spec = ClusterSpec::from_scenario(CLUSTER)?;
    spec.volumes.retain(|v| v.volid != "local-lvm:vm-200-cloudinit");
    let sim = PveSim::start(spec).await?;
    let pve = client(&sim, TOKEN)?;
    let error = plan(&pve, &request(200, TARGET, None))
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(
        error.to_string().contains("Cannot determine migration disk size"),
        "{error}"
    );
    assert!(changes(&sim).is_empty());

    let mut spec = ClusterSpec::from_scenario(CLUSTER)?;
    let vm = spec.vms.iter_mut().find(|vm| vm.vmid == 200).ok_or("missing VM")?;
    let root = vm
        .config
        .get("scsi0")
        .and_then(|value| value.as_str())
        .ok_or("missing root")?;
    vm.config.insert("scsi0".to_owned(), format!("{root},shared=1").into());
    let sim = PveSim::start(spec).await?;
    let pve = client(&sim, TOKEN)?;
    let planned = plan(&pve, &request(200, TARGET, None)).await?;
    assert_eq!(planned["capacity"]["required_bytes"], 20 * GIB + 4 * 1024 * 1024);
    assert!(changes(&sim).is_empty());
    Ok(())
}
