//! Proxmox tasks (T4) and the power steps every lifecycle operation uses:
//! `qm start` and `proxmox.shutdown_vm_on_node`, against the simulator; and
//! the optimistic lock on config changes (T5, `lifecycle._digest_args`).

use std::sync::Mutex;
use std::time::Duration;

use hs_core::power::{shutdown_vm, start_vm};
use hs_core::tasks::{Observer, TaskWait, upid_node, wait_task};
use hs_model::jobs::JobEvent;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::{ClusterSpec, PveSim};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const NODE: &str = "pve-example-1";

const CLUSTER: &str = r#"
[pve]
version = { release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }

[[pve.nodes]]
name = "pve-example-1"
pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
kversion = "Linux 7.0.14-20-pve"
uptime = 1

[[pve.vms]]
vmid = 240
node = "pve-example-1"
status = "stopped"
config = { name = "hst-power" }

[[pve.vms]]
vmid = 241
node = "pve-example-1"
status = "running"
config = { name = "hst-acpi" }

# A guest that ignores the ACPI power button.
[[pve.vms]]
vmid = 242
node = "pve-example-1"
status = "running"
acpi = false
config = { name = "hst-noacpi" }

# A clone that is already running, as `qm clone` logs it.
[[pve.tasks]]
node = "pve-example-1"
type = "qmclone"
id = "100"
user = "homestack@pve!test"
polls_until_done = 3
log = [
    "create full clone of drive scsi0 (example-storage:vm-100-disk-0)",
    "transferred 0.0 B of 16.0 GiB (0.00%)",
    "transferred 3.1 GiB of 16.0 GiB (19.38%)",
    "transferred 8.0 GiB of 16.0 GiB (50.00%)",
    "transferred 16.0 GiB of 16.0 GiB (100.00%)",
    "TASK OK",
]

[[pve.tokens]]
id = "homestack@pve!test"
secret = "00000000-0000-0000-0000-00000000000d"
acl = { "/" = ["Sys.Audit", "VM.Audit", "VM.PowerMgmt", "VM.Config.Options"] }

[[pve.tokens]]
id = "auditor@pve!server"
secret = "00000000-0000-0000-0000-00000000000c"
acl = { "/" = ["Sys.Audit", "VM.Audit"] }
"#;

const TEST_TOKEN: &str = "homestack@pve!test=00000000-0000-0000-0000-00000000000d";
const AUDITOR_TOKEN: &str = "auditor@pve!server=00000000-0000-0000-0000-00000000000c";

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
    fn events(&self) -> Vec<JobEvent> {
        self.0.lock().map(|events| events.clone()).unwrap_or_default()
    }
}

fn quick() -> TaskWait {
    TaskWait {
        poll: Duration::from_millis(5),
        timeout: Duration::from_secs(10),
    }
}

async fn setup(token: &str) -> Result<(PveSim, PveClient), Box<dyn std::error::Error>> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let mut config = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    config.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let client = PveClient::new(config)?;
    Ok((sim, client))
}

async fn status(pve: &PveClient, vmid: u32) -> String {
    hs_core::inventory::vm_status(pve, NODE, vmid).await
}

#[test]
fn upids_name_their_node() {
    assert_eq!(
        upid_node("UPID:pve2:0011D2E1:0A3B1C2D:6720A1B2:qmstart:240:homestack@pve!test:").as_deref(),
        Some("pve2")
    );
    assert_eq!(upid_node("not a upid"), None);
    assert_eq!(upid_node("UPID::1:2:3:qmstart:240:u:"), None);
}

#[tokio::test]
async fn a_task_is_followed_from_upid_to_end_with_its_log_and_progress() -> TestResult {
    let (sim, pve) = setup(TEST_TOKEN).await?;
    let upid = sim.task_upids().first().cloned().ok_or("no task")?;
    let recorder = Recorder::default();
    let result = wait_task(&pve, &upid, &quick(), &recorder).await?;
    assert_eq!(result.exitstatus, "OK");
    assert!(result.ok());
    let events = recorder.events();
    let log: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            JobEvent::TaskLog { lines, .. } => Some(lines.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(log.len(), 6, "every log line exactly once: {log:?}");
    assert_eq!(log.last().map(String::as_str), Some("TASK OK"));
    let percents: Vec<f64> = events
        .iter()
        .filter_map(|e| match e {
            JobEvent::Progress { percent, .. } => Some(*percent),
            _ => None,
        })
        .collect();
    assert_eq!(percents.last(), Some(&100.0));
    assert!(percents.windows(2).all(|w| w[0] < w[1]), "{percents:?}");
    assert!(matches!(events.last(), Some(JobEvent::TaskFinished { exitstatus, .. }) if exitstatus == "OK"));
    Ok(())
}

#[tokio::test]
async fn start_runs_a_stopped_vm_and_reports_the_task() -> TestResult {
    let (_sim, pve) = setup(TEST_TOKEN).await?;
    let recorder = Recorder::default();
    start_vm(&pve, NODE, 240, &quick(), &recorder).await?;
    assert_eq!(status(&pve, 240).await, "running");
    let events = recorder.events();
    assert!(
        matches!(events.first(), Some(JobEvent::TaskStarted { kind, vmid: Some(240), .. }) if kind == "qmstart"),
        "{events:?}"
    );

    let error = start_vm(&pve, NODE, 240, &quick(), &recorder)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("Proxmox task qmstart for VM 240 on pve-example-1 failed: VM 240 already running")
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_follows_shutdown_vm_on_node() -> TestResult {
    let (sim, pve) = setup(TEST_TOKEN).await?;
    let recorder = Recorder::default();

    sim.clear_ops();
    shutdown_vm(&pve, NODE, 240, 90, &quick(), &recorder).await?;
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(ops.len(), 1, "a stopped VM is only looked at: {ops:?}");

    shutdown_vm(&pve, NODE, 241, 90, &quick(), &recorder).await?;
    assert_eq!(status(&pve, 241).await, "stopped");

    let error = shutdown_vm(&pve, NODE, 242, 90, &quick(), &recorder)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some(
            "VM 242 did not shut down cleanly on pve-example-1. Current status: running. \
             HomeStack will not force-stop it automatically."
        )
    );
    assert_eq!(status(&pve, 242).await, "running");
    Ok(())
}

#[tokio::test]
async fn power_needs_vm_power_mgmt() -> TestResult {
    let (_sim, pve) = setup(AUDITOR_TOKEN).await?;
    let error = start_vm(&pve, NODE, 240, &quick(), &hs_core::tasks::NoObserver)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("VM.PowerMgmt"), "{error}");
    Ok(())
}

/// T5: a change made with the digest of an older config is refused.
#[tokio::test]
async fn config_changes_carry_the_digest_they_were_planned_with() -> TestResult {
    let (_sim, pve) = setup(TEST_TOKEN).await?;
    let before = hs_core::inventory::vm_config(&pve, NODE, 240).await?;
    let digest = before.get("digest").cloned().ok_or("no digest")?;
    hs_core::inventory::set_vm_config(&pve, NODE, 240, &[("description", "first")], Some(&digest)).await?;
    let after = hs_core::inventory::vm_config(&pve, NODE, 240).await?;
    assert_eq!(after.get("description").map(String::as_str), Some("first"));
    assert_ne!(after.get("digest"), Some(&digest), "the digest follows the content");

    let error = hs_core::inventory::set_vm_config(&pve, NODE, 240, &[("description", "second")], Some(&digest))
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("Proxmox error 500: detected modified configuration - file changed by other user? Try again.")
    );
    hs_core::inventory::set_vm_config(&pve, NODE, 240, &[("description", "third")], None).await?;
    Ok(())
}
