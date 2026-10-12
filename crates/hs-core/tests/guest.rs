//! Ports of the guest-agent and progress tests: `python/tests/test_proxmox.py`
//! (`QgaWaitTests`) and `test_lifecycle.py` (`ProgressTests`).
//!
//! Time is paused, so the waits of the ported functions take no real time,
//! as the Python tests patch `time.sleep`.

mod common;

use std::time::Duration;

use common::{FakePve, exec_key, key};
use hs_core::guest::{GUEST_EXEC_TIMEOUT, QGA_PING_TIMEOUT, guest_exec, guest_out, qga_ping, wait_for_qga};
use hs_core::tasks::TransferTracker;
use serde_json::json;

const NODE: &str = "example-node-1";

/// `QgaWaitTests::test_qga_ping_command_is_bounded`: Python bounds the ping
/// with `timeout -k 2s 5s`; the server bounds the API call to 5 seconds.
#[tokio::test(start_paused = true)]
async fn qga_ping_is_bounded() {
    let pve = FakePve::new().hanging("POST /nodes/example-node-1/qemu/200/agent/ping");
    let start = tokio::time::Instant::now();
    let result = qga_ping(&pve, NODE, 200).await;
    assert!(result.is_err(), "a ping that never answers fails");
    assert_eq!(start.elapsed(), QGA_PING_TIMEOUT);
    assert_eq!(QGA_PING_TIMEOUT, Duration::from_secs(5));
}

/// `QgaWaitTests::test_wait_for_qga_retries_transient_transport_error`
#[tokio::test(start_paused = true)]
async fn wait_for_qga_retries_transient_transport_error() {
    let pve = FakePve::new()
        .unreachable("POST /nodes/example-node-1/qemu/200/agent/ping", "transient timeout")
        .with("POST /nodes/example-node-1/qemu/200/agent/ping", json!({"result": {}}));
    let result = wait_for_qga(&pve, NODE, 200, Duration::from_secs(30)).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(pve.calls().len(), 2);
}

/// Not in Python: the timeout message names the last failure.
#[tokio::test(start_paused = true)]
async fn wait_for_qga_gives_up_after_the_timeout() {
    let pve = FakePve::new().failing(
        "POST /nodes/example-node-1/qemu/200/agent/ping",
        500,
        "QEMU guest agent is not running",
    );
    let error = wait_for_qga(&pve, NODE, 200, Duration::from_secs(10))
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some(
            "VM 200 on example-node-1 did not expose a working QEMU Guest Agent within 10s: \
             Proxmox error 500: QEMU guest agent is not running"
        )
    );
    assert_eq!(pve.calls().len(), 5, "pings at 0, 2, 4, 6 and 8 seconds");
}

/// `QgaWaitTests::test_guest_exec_polls_pid_until_explicit_completion`
#[tokio::test(start_paused = true)]
async fn guest_exec_polls_pid_until_explicit_completion() {
    let pve = FakePve::new().with_exec(
        NODE,
        200,
        "echo ready",
        42,
        json!({"exited": 1, "exitcode": 0, "out-data": "ready\n"}),
    );
    let out = guest_out(&pve, NODE, 200, "echo ready", true, GUEST_EXEC_TIMEOUT).await;
    assert_eq!(out.ok().as_deref(), Some("ready"));
    let calls = pve.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], exec_key(NODE, 200, "echo ready"));
    assert_eq!(
        calls[1],
        key(
            "GET",
            "/nodes/example-node-1/qemu/200/agent/exec-status",
            &[("pid", "42")]
        )
    );
}

/// Not in Python's tests: an unfinished command is polled again.
#[tokio::test(start_paused = true)]
async fn guest_exec_waits_while_the_command_runs() {
    let pve = FakePve::new()
        .with_exec(NODE, 200, "sleep 1", 7, json!({"exited": 0}))
        .with(
            &key(
                "GET",
                "/nodes/example-node-1/qemu/200/agent/exec-status",
                &[("pid", "7")],
            ),
            json!({"exited": 1, "exitcode": 3, "err-data": "boom\n"}),
        );
    let result = guest_exec(&pve, NODE, 200, "sleep 1", false, GUEST_EXEC_TIMEOUT).await;
    let result = result.ok();
    assert_eq!(
        result.as_ref().map(|r| (r.exitcode, r.err.as_str())),
        Some((3, "boom\n"))
    );
    assert_eq!(pve.calls().len(), 3);

    let error = guest_exec(&pve, NODE, 200, "sleep 1", true, GUEST_EXEC_TIMEOUT)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("Guest command failed in VM 200 on example-node-1 (exit=3): sleep 1\nboom")
    );
}

/// Not in Python's tests: the deadline of `_complete_guest_exec`.
#[tokio::test(start_paused = true)]
async fn guest_exec_gives_up_at_the_deadline() {
    let pve = FakePve::new().with_exec(NODE, 200, "sleep 99", 8, json!({"exited": 0}));
    let error = guest_exec(&pve, NODE, 200, "sleep 99", true, Duration::from_secs(2))
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("Guest command did not finish in VM 200 on example-node-1 within 2s: sleep 99")
    );
}

/// `QgaWaitTests::test_guest_exec_rejects_incomplete_non_pid_response`
#[tokio::test(start_paused = true)]
async fn guest_exec_rejects_incomplete_non_pid_response() {
    let pve = FakePve::new().with(&exec_key(NODE, 200, "true"), json!({"out-data": "not finished"}));
    let error = guest_exec(&pve, NODE, 200, "true", true, GUEST_EXEC_TIMEOUT)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("incomplete response"), "{error}");
}

/// Not in Python's tests: a refused `exec` with and without `check`.
#[tokio::test(start_paused = true)]
async fn refused_exec_follows_check() {
    let pve = FakePve::new().failing(&exec_key(NODE, 200, "true"), 500, "QEMU guest agent is not running");
    let error = guest_exec(&pve, NODE, 200, "true", true, GUEST_EXEC_TIMEOUT)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some(
            "QEMU Guest Agent exec failed for VM 200 on example-node-1: \
             Proxmox error 500: QEMU guest agent is not running"
        )
    );
    let result = guest_exec(&pve, NODE, 200, "true", false, GUEST_EXEC_TIMEOUT)
        .await
        .ok();
    assert_eq!(
        result.map(|r| (r.exited, r.exitcode, r.out)),
        Some((true, 1, "Proxmox error 500: QEMU guest agent is not running".to_owned()))
    );
}

/// `ProgressTests::test_transfer_progress_uses_latest_proxmox_mirror_percentage`
#[test]
fn transfer_progress_uses_latest_proxmox_mirror_percentage() {
    let mut tracker = TransferTracker::default();
    let update = tracker.update(
        "mirror-scsi0: transferred 3.1 GiB of 16.0 GiB (31.88%) in 13s\n\
         mirror-scsi0: transferred 5.8 GiB of 16.0 GiB (36.14%) in 18s\n",
    );
    let update = update.map(|u| (u.percent, u.detail));
    assert_eq!(update, Some((36.14, "5.8 / 16.0 GiB".to_owned())));
    assert_eq!(
        tracker.update("mirror-scsi0: transferred 5.8 GiB of 16.0 GiB (36.14%) in 19s\n"),
        None,
        "an unchanged percentage is not reported again"
    );
    let update = tracker.update("transferred 900.0 MiB of 16.0 GiB (130.00%)");
    assert_eq!(
        update.map(|u| (u.percent, u.detail)),
        Some((100.0, "900.0 MiB / 16.0 GiB".to_owned()))
    );
    assert_eq!(tracker.update("no progress here"), None);
}
