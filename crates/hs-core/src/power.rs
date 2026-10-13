//! Starting and stopping VMs, the way every lifecycle operation does it.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::error::CoreError;
use crate::inventory::{cluster_vm_resources, py_repr, vm_status};
use crate::pve::Pve;
use crate::tasks::{Observer, TaskWait, is_lock_timeout, retry_while_locked, wait_task};

/// Seconds `qm start` is given (`lifecycle`: `node_run(..., timeout=300)`).
pub const START_TIMEOUT_SECS: u64 = 300;

/// `shutdown_vm_on_node`'s default `--timeout`.
pub const SHUTDOWN_TIMEOUT_SECS: u32 = 90;

fn upid(answer: &Value) -> Result<String, CoreError> {
    answer
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Unusable(format!("Proxmox did not return a task id: {answer}")))
}

/// `qm start VMID`: start the VM and wait for its task (`wait.timeout`,
/// [`START_TIMEOUT_SECS`] in the operations). Python fails with the output
/// of `qm start`; the server names the task and its exit status. A start
/// refused for a taken lock is tried again ([`retry_while_locked`]).
pub async fn start_vm(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    wait: &TaskWait,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    retry_while_locked(wait.poll, || async {
        let answer = pve
            .post(&format!("/nodes/{node}/qemu/{vmid}/status/start"), &[])
            .await?;
        let result = wait_task(pve, &upid(&answer)?, wait, observer).await?;
        if result.ok() {
            Ok(())
        } else {
            Err(CoreError::Failed(format!(
                "Proxmox task qmstart for VM {vmid} on {node} failed: {}",
                result.exitstatus
            )))
        }
    })
    .await
}

/// `proxmox.shutdown_vm_on_node`: a stopped VM is left alone, any status
/// but `running` is refused, the guest gets `timeout_secs` to power off
/// (`qm shutdown --timeout`; the task is followed for that plus 30 seconds,
/// polling every `wait.poll`), and anything but `stopped` afterwards is an
/// error. HomeStack never forces a stop. A shutdown refused for a taken
/// lock is tried again ([`retry_while_locked`]).
pub async fn shutdown_vm(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    timeout_secs: u32,
    wait: &TaskWait,
    observer: &impl Observer,
) -> Result<(), CoreError> {
    let status = vm_status(pve, node, vmid).await;
    if status == "stopped" {
        return Ok(());
    }
    if status != "running" {
        return Err(CoreError::Failed(format!(
            "VM {vmid} has unexpected status {}; refusing shutdown",
            py_repr(Some(&status))
        )));
    }
    let timeout = timeout_secs.to_string();
    // Python allows `qm shutdown` its timeout plus 30 seconds.
    let task_wait = TaskWait {
        poll: wait.poll,
        timeout: std::time::Duration::from_secs(u64::from(timeout_secs) + 30),
    };
    let task_ok = retry_while_locked(wait.poll, || async {
        let answer = pve
            .post(
                &format!("/nodes/{node}/qemu/{vmid}/status/shutdown"),
                &[("timeout", &timeout)],
            )
            .await?;
        match wait_task(pve, &upid(&answer)?, &task_wait, observer).await {
            Ok(result) if is_lock_timeout(&result.exitstatus) => Err(CoreError::Failed(format!(
                "Proxmox task qmshutdown for VM {vmid} on {node} failed: {}",
                result.exitstatus
            ))),
            Ok(result) => Ok(result.ok()),
            // A task that outlives its time counts as a failed shutdown; the
            // status read below says how the VM was left.
            Err(CoreError::Failed(_)) => Ok(false),
            Err(error) => Err(error),
        }
    })
    .await?;
    let final_status = vm_status(pve, node, vmid).await;
    if !task_ok || final_status != "stopped" {
        return Err(CoreError::Failed(format!(
            "VM {vmid} did not shut down cleanly on {node}. Current status: {final_status}. \
             HomeStack will not force-stop it automatically."
        )));
    }
    wait_listed_stopped(pve, vmid).await
}

/// How long `cluster/resources` may still list a stopped VM as running.
const LISTED_STOPPED_TIMEOUT: Duration = Duration::from_secs(30);

/// Wait until `cluster/resources`, which the operations read the power state
/// from, also lists the VM as stopped: on PVE 9.2 it still said `running` a
/// few seconds after the shutdown (phase 9, live), and an operation planned
/// right then took the VM for a running one.
async fn wait_listed_stopped(pve: &impl Pve, vmid: u32) -> Result<(), CoreError> {
    let deadline = Instant::now() + LISTED_STOPPED_TIMEOUT;
    loop {
        let listed = cluster_vm_resources(pve)
            .await?
            .into_iter()
            .find(|resource| resource.vmid == vmid)
            .and_then(|resource| resource.status);
        if listed.as_deref() != Some("running") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Failed(format!(
                "VM {vmid} is stopped, but the cluster still lists it as running after {}s",
                LISTED_STOPPED_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(LISTED_STOPPED_POLL).await;
    }
}

/// Pause between two reads of `cluster/resources`.
const LISTED_STOPPED_POLL: Duration = Duration::from_secs(1);
