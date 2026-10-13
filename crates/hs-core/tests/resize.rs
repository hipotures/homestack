//! `resize` (B5) on the simulator, and the Python tests of phase 7
//! (`docs/migration/phase-7.md`): `test_resize.py`, `test_resize_luks.py`
//! and `test_resize_auth.py`.
//!
//! The cluster (`data/resize-cluster.toml`) has running workspaces with a
//! plain partitioned root (200), LUKS with a key file + LVM (201), LUKS with
//! a passphrase + LVM (202) and a Btrfs root (203). The simulated guest grows
//! each layer when its tool runs, so a test can check what the guest sees.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::LifecycleTiming;
use hs_core::resize::{ResizeRequest, plan_resize, resize_workspace};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error>;
type TestResult = Result<(), Error>;

const NODE: &str = "pve-example-1";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";
const CLUSTER: &str = include_str!("data/resize-cluster.toml");
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

const GIB: u64 = 1024 * 1024 * 1024;
const PASSPHRASE: &str = "correct horse battery staple";

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

fn root(vmid: u32, size: &str) -> ResizeRequest {
    ResizeRequest {
        vmid,
        root_size: Some(size.to_owned()),
        home_size: None,
    }
}

fn home(vmid: u32, size: &str) -> ResizeRequest {
    ResizeRequest {
        vmid,
        root_size: None,
        home_size: Some(size.to_owned()),
    }
}

fn ops(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

/// The shell commands run in the guest, in order.
fn guest_commands(sim: &PveSim) -> Vec<String> {
    sim.ops()
        .into_iter()
        .filter(|op| op.name == "guest_exec")
        .filter_map(|op| op.params.get("cmd").cloned())
        .collect()
}

fn resize_calls(sim: &PveSim) -> Vec<String> {
    ops(sim).into_iter().filter(|op| op.ends_with("/resize}")).collect()
}

/// Index of the first guest command starting with `prefix`.
fn position(commands: &[String], prefix: &str) -> Option<usize> {
    commands.iter().position(|command| command.starts_with(prefix))
}

fn fault(op: &str) -> FaultSpec {
    FaultSpec {
        op: op.to_owned(),
        occurrence: 1,
        status: 500,
        message: format!("injected failure of {op}"),
        applied: false,
        hang: false,
    }
}

async fn plan(pve: &PveClient, request: &ResizeRequest) -> Result<Value, CoreError> {
    plan_resize(
        pve,
        &config().map_err(|e| CoreError::Invalid(e.to_string()))?,
        POOL,
        request,
    )
    .await
}

async fn run(pve: &PveClient, plan: &Value, passphrase: Option<&str>) -> Result<Value, CoreError> {
    resize_workspace(pve, plan, passphrase, &quick(), &NoObserver).await
}

/// `get-fsinfo`'s `total-bytes-privileged` of the filesystem at `mount`.
async fn filesystem_size(pve: &PveClient, vmid: u32, mount: &str) -> Result<u64, Error> {
    let vmid = vmid.to_string();
    let info = pve
        .get_value(&["nodes", NODE, "qemu", &vmid, "agent", "get-fsinfo"], &[])
        .await?;
    info["result"]
        .as_array()
        .and_then(|list| list.iter().find(|fs| fs["mountpoint"] == mount))
        .and_then(|fs| fs["total-bytes-privileged"].as_u64())
        .ok_or_else(|| format!("no filesystem at {mount}: {info}").into())
}

async fn disk_config(pve: &PveClient, vmid: u32, slot: &str) -> Result<String, Error> {
    let vmid = vmid.to_string();
    let config = pve.get_value(&["nodes", NODE, "qemu", &vmid, "config"], &[]).await?;
    Ok(config[slot].as_str().unwrap_or_default().to_owned())
}

fn message(result: Result<Value, CoreError>) -> String {
    match result {
        Ok(value) => panic!("expected a failure, got {value}"),
        Err(error) => error.to_string(),
    }
}

// --- the plan -------------------------------------------------------------------

#[tokio::test]
async fn plan_reports_the_inspection_and_needs_no_secret_without_a_passphrase() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(200, "32G")).await?;
    assert_eq!(plan["role"], "root");
    assert_eq!(plan["disk"], "scsi0");
    assert_eq!(plan["volume"], "local-lvm:vm-200-disk-0");
    assert_eq!(plan["current_size_gib"], json!(16.0));
    assert_eq!(plan["target_size_gib"], 32);
    assert_eq!(plan["size"], "32G");
    assert_eq!(
        plan["guest"],
        json!({
            "device": "/dev/sda1", "parent": "/dev/sda", "partition": 1, "size_bytes": 16 * GIB,
            "partition_device": "/dev/sda1", "crypt": null, "lvm": null, "filesystem": "ext4",
            "mount": "/", "btrfs_devid": null,
        })
    );
    assert_eq!(plan["secrets_required"], json!([]));
    assert!(plan.get("digest").is_none(), "the execution reads the digest again");
    Ok(())
}

#[tokio::test]
async fn plan_refusals_come_before_any_guest_command() -> TestResult {
    let (sim, pve) = cluster().await?;
    let cases = [
        (
            ResizeRequest {
                vmid: 200,
                root_size: Some("32G".to_owned()),
                home_size: Some("32G".to_owned()),
            },
            "Specify exactly one of root_size or home_size",
        ),
        (root(200, "32.5G"), "Home size must be an integer number of GiB or TiB"),
        (
            root(205, "32G"),
            "Resize requires a running VM with a working QEMU Guest Agent",
        ),
        (
            root(204, "32G"),
            "Resize requires exactly the configured root and home data disks",
        ),
        (root(101, "32G"), "VM 101 is not in pool homestack"),
    ];
    for (request, expected) in cases {
        let error = plan(&pve, &request)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(expected), "{request:?}: {error}");
    }
    assert!(guest_commands(&sim).is_empty(), "{:?}", guest_commands(&sim));
    Ok(())
}

// --- test_resize.py ---------------------------------------------------------------

/// `ResizeInspectionErrorTests.test_guest_failure_reports_reason_without_script`
#[tokio::test]
async fn guest_failure_reports_reason_without_script() -> TestResult {
    let (_sim, pve) = cluster().await?;
    let error = plan(&pve, &root(206, "32G"))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert_eq!(
        error,
        "Cannot inspect root disk in VM 206: Resize requires an exact ext4 or btrfs mount: /"
    );
    Ok(())
}

/// `test_lock_allows_resize_and_routes_to_current_node`
#[tokio::test]
async fn lock_allows_resize_and_routes_to_current_node() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(200, "32G")).await?;
    sim.clear_ops();
    let result = run(&pve, &plan, None).await?;
    assert_eq!(result["ok"], true);
    assert_eq!(result["status"], "resized");
    assert_eq!(result["event"], "workspace.resized");
    assert_eq!(
        resize_calls(&sim),
        [format!("put{{path=/nodes/{NODE}/qemu/200/resize}}")]
    );
    let commands = guest_commands(&sim);
    assert!(commands.contains(&"growpart /dev/sda 1".to_owned()), "{commands:?}");
    assert!(commands.contains(&"resize2fs /dev/sda1".to_owned()), "{commands:?}");
    assert!(disk_config(&pve, 200, "scsi0").await?.ends_with("size=32G"));
    assert!(filesystem_size(&pve, 200, "/").await? > 31 * GIB);
    Ok(())
}

/// `test_shrink_rejected_before_mutations_and_guest_inspection`
#[tokio::test]
async fn shrink_rejected_before_mutations_and_guest_inspection() -> TestResult {
    let (sim, pve) = cluster().await?;
    let error = plan(&pve, &root(200, "8G"))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert_eq!(error, "Shrinking is not supported: scsi0 is 16G, requested 8G");
    assert!(guest_commands(&sim).is_empty());
    assert!(resize_calls(&sim).is_empty());
    Ok(())
}

/// `test_equal_size_retries_filesystem_without_disk_resize`
#[tokio::test]
async fn equal_size_retries_filesystem_without_disk_resize() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(200, "16G")).await?;
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    assert!(resize_calls(&sim).is_empty());
    let commands = guest_commands(&sim);
    assert!(
        commands.contains(&"growpart /dev/sda 1".to_owned()),
        "NOCHANGE is success: {commands:?}"
    );
    assert!(commands.contains(&"resize2fs /dev/sda1".to_owned()), "{commands:?}");
    Ok(())
}

/// `test_home_selects_home_disk_and_skips_partition_growth`
#[tokio::test]
async fn home_selects_home_disk_and_skips_partition_growth() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &home(200, "1T")).await?;
    assert_eq!(plan["disk"], "scsi1");
    assert_eq!(plan["guest"]["device"], "/dev/sdb");
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    assert_eq!(resize_calls(&sim).len(), 1);
    assert!(disk_config(&pve, 200, "scsi1").await?.ends_with("size=1T"));
    let commands = guest_commands(&sim);
    assert!(!commands.iter().any(|c| c.starts_with("growpart")), "{commands:?}");
    assert!(commands.contains(&"resize2fs /dev/sdb".to_owned()), "{commands:?}");
    assert_eq!(filesystem_size(&pve, 200, "/home/developer").await?, 1024 * GIB);
    Ok(())
}

/// `test_btrfs_grows_the_inspected_device_id`
#[tokio::test]
async fn btrfs_grows_the_inspected_device_id() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(203, "32G")).await?;
    assert_eq!(plan["guest"]["btrfs_devid"], 1);
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    let commands = guest_commands(&sim);
    assert!(
        commands.contains(&"btrfs filesystem resize 1:max /".to_owned()),
        "{commands:?}"
    );
    assert!(!commands.iter().any(|c| c.starts_with("resize2fs")), "{commands:?}");
    assert!(filesystem_size(&pve, 203, "/").await? > 31 * GIB);
    Ok(())
}

/// `test_revalidation_rejects_volume_or_size_drift`
#[tokio::test]
async fn revalidation_rejects_volume_or_size_drift() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &root(200, "32G")).await?;
    let mut other_volume = planned.clone();
    other_volume["volume"] = json!("local-lvm:vm-200-disk-9");
    sim.clear_ops();
    let error = message(run(&pve, &other_volume, None).await);
    assert!(error.contains("changed after resize confirmation"), "{error}");
    assert!(resize_calls(&sim).is_empty());

    let admin = client(&sim, ADMIN)?;
    admin
        .put_value(
            &["nodes", NODE, "qemu", "200", "resize"],
            &[("disk", "scsi0"), ("size", "64G")],
        )
        .await?;
    sim.clear_ops();
    let error = message(run(&pve, &planned, None).await);
    assert!(error.contains("changed after resize confirmation"), "{error}");
    assert!(resize_calls(&sim).is_empty());
    Ok(())
}

/// `test_filesystem_failure_explains_same_size_retry`, then the retry.
#[tokio::test]
async fn filesystem_failure_explains_same_size_retry() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &root(200, "32G")).await?;
    sim.inject_fault(fault("guest_exec{cmd=resize2fs /dev/sda1,vmid=200}"))?;
    sim.clear_ops();
    let error = message(run(&pve, &planned, None).await);
    assert!(error.starts_with("Disk growth may already be applied; filesystem growth is incomplete. Re-run resize 200 --root-size 32G to finish."), "{error}");
    assert_eq!(resize_calls(&sim).len(), 1);
    assert!(filesystem_size(&pve, 200, "/").await? < 17 * GIB);

    let retry = plan(&pve, &root(200, "32G")).await?;
    assert_eq!(retry["current_size_gib"], json!(32.0));
    sim.clear_ops();
    run(&pve, &retry, None).await?;
    assert!(resize_calls(&sim).is_empty(), "the disk has grown already");
    assert!(filesystem_size(&pve, 200, "/").await? > 31 * GIB);
    Ok(())
}

/// `test_guest_preflight_failure_prevents_host_growth`
#[tokio::test]
async fn guest_preflight_failure_prevents_host_growth() -> TestResult {
    let (sim, pve) = cluster().await?;
    sim.inject_fault(fault(
        "guest_exec{cmd=lsblk --json --bytes --paths --list -o NAME%2CTYPE%2CPKNAME%2CSERIAL%2CSIZE%2CMAJ:MIN,vmid=200}",
    ))?;
    assert!(plan(&pve, &root(200, "32G")).await.is_err());
    assert!(resize_calls(&sim).is_empty());
    Ok(())
}

#[tokio::test]
async fn a_refused_disk_resize_leaves_the_guest_alone() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &root(200, "32G")).await?;
    // Changed between the config read and the resize.
    sim.inject_fault(FaultSpec {
        status: 500,
        message: "checksum mismatch (file change by other user?)".to_owned(),
        ..fault(&format!("put{{path=/nodes/{NODE}/qemu/200/resize}}"))
    })?;
    let error = message(run(&pve, &planned, None).await);
    assert!(error.contains("checksum mismatch"), "{error}");
    assert!(disk_config(&pve, 200, "scsi0").await?.ends_with("size=16G"));
    assert!(!guest_commands(&sim).iter().any(|c| c.starts_with("growpart")));
    Ok(())
}

// --- test_resize_luks.py and test_resize_auth.py ------------------------------------

/// `test_keyfile_is_validated_in_guest_before_host_disk_growth`,
/// `test_keyfile_stays_in_guest_and_is_checked_before_resize`
#[tokio::test]
async fn keyfile_is_validated_in_guest_before_host_disk_growth() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(201, "32G")).await?;
    assert_eq!(
        plan["guest"]["crypt"],
        json!({"name": "cryptroot", "device": "/dev/sda1", "key_file": "/etc/cryptsetup-keys.d/cryptroot.key", "auth": "keyfile"})
    );
    assert_eq!(plan["guest"]["lvm"]["lv_path"], "/dev/vg/root");
    assert_eq!(plan["secrets_required"], json!([]));
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    let all = ops(&sim);
    let test = all
        .iter()
        .position(|op| op.contains("--test-passphrase"))
        .ok_or("no key test")?;
    let put = all.iter().position(|op| op.ends_with("/resize}")).ok_or("no resize")?;
    assert!(test < put, "the key is tested before the disk grows: {all:?}");
    let commands = guest_commands(&sim);
    assert_eq!(
        commands[position(&commands, "cryptsetup").ok_or("no cryptsetup")?],
        "cryptsetup --batch-mode --key-file /etc/cryptsetup-keys.d/cryptroot.key open --type luks --test-passphrase /dev/sda1"
    );
    let order = [
        "blockdev --getsize64 /dev/sda",
        "growpart /dev/sda 1",
        "cryptsetup --batch-mode --key-file /etc/cryptsetup-keys.d/cryptroot.key resize cryptroot",
        "pvresize /dev/mapper/cryptroot",
        "vgs --noheadings -o vg_free_count vg",
        "lvextend --extents +100%FREE /dev/vg/root",
        "resize2fs /dev/mapper/vg-root",
    ];
    let found: Vec<Option<usize>> = order.iter().map(|command| position(&commands, command)).collect();
    assert!(found.iter().all(Option::is_some), "{commands:?}");
    assert!(found.windows(2).all(|pair| pair[0] < pair[1]), "{commands:?}");
    assert!(filesystem_size(&pve, 201, "/").await? > 31 * GIB);
    Ok(())
}

/// `test_wrong_keyfile_fails_before_any_host_or_guest_growth_mutation`,
/// `test_wrong_keyfile_aborts_before_yield`
#[tokio::test]
async fn wrong_keyfile_fails_before_any_host_or_guest_growth_mutation() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(201, "32G")).await?;
    let admin = client(&sim, ADMIN)?;
    admin
        .post_value(
            &["nodes", NODE, "qemu", "201", "agent", "file-write"],
            &[("file", "/etc/cryptsetup-keys.d/cryptroot.key"), ("content", "wrong")],
        )
        .await?;
    sim.clear_ops();
    let error = message(run(&pve, &plan, None).await);
    assert_eq!(
        error,
        "LUKS authentication or resize failed; verify the configured key file in the VM"
    );
    assert!(resize_calls(&sim).is_empty());
    let commands = guest_commands(&sim);
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(commands[0].contains("--test-passphrase"));
    Ok(())
}

/// `test_encrypted_equal_size_retry_revalidates_and_grows_guest_layers`
#[tokio::test]
async fn encrypted_equal_size_retry_revalidates_and_grows_guest_layers() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(201, "16G")).await?;
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    assert!(resize_calls(&sim).is_empty());
    let commands = guest_commands(&sim);
    assert!(commands[0].contains("--test-passphrase"), "{commands:?}");
    assert!(
        commands.contains(
            &"cryptsetup --batch-mode --key-file /etc/cryptsetup-keys.d/cryptroot.key resize cryptroot".to_owned()
        ),
        "{commands:?}"
    );
    Ok(())
}

/// `test_passphrase_is_sent_as_workspace_ssh_input_after_guest_identity_check`,
/// `test_password_is_only_sent_on_stdin_after_identity_check`: with
/// decision 6 the passphrase goes to the guest agent as `input-data`.
#[tokio::test]
async fn passphrase_is_sent_only_as_standard_input() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(202, "32G")).await?;
    assert_eq!(plan["guest"]["crypt"]["auth"], "passphrase");
    assert_eq!(plan["guest"]["crypt"]["key_file"], Value::Null);
    assert_eq!(plan["secrets_required"], json!(["luks_passphrase"]));
    sim.clear_ops();
    let result = run(&pve, &plan, Some(PASSPHRASE)).await?;
    assert!(!result.to_string().contains(PASSPHRASE));
    let all = ops(&sim);
    assert!(
        !all.iter().any(|op| op.contains("horse")),
        "the passphrase is in a recorded call: {all:?}"
    );
    let commands = guest_commands(&sim);
    let crypt: Vec<&String> = commands.iter().filter(|c| c.starts_with("cryptsetup")).collect();
    assert_eq!(
        crypt,
        [
            "cryptsetup --batch-mode --key-file - open --type luks --test-passphrase /dev/sda1",
            "cryptsetup --batch-mode --key-file - resize cryptroot",
        ]
    );
    let test = all
        .iter()
        .position(|op| op.contains("--test-passphrase"))
        .ok_or("no test")?;
    let put = all.iter().position(|op| op.ends_with("/resize}")).ok_or("no resize")?;
    assert!(test < put);
    assert!(filesystem_size(&pve, 202, "/").await? > 31 * GIB);
    Ok(())
}

/// `test_wrong_passphrase_fails_before_host_disk_resize`,
/// `test_wrong_password_aborts_before_resize`
#[tokio::test]
async fn wrong_passphrase_fails_before_host_disk_resize() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(202, "32G")).await?;
    sim.clear_ops();
    let error = message(run(&pve, &plan, Some("wrong passphrase")).await);
    assert_eq!(error, "LUKS passphrase verification failed; no disk changes made");
    assert!(resize_calls(&sim).is_empty());
    assert_eq!(guest_commands(&sim).len(), 1);
    Ok(())
}

/// `test_json_or_non_tty_passphrase_refuses_before_prompt_or_mutation`,
/// `test_noninteractive_password_refuses_before_prompt_or_connection`: with
/// decision 6, a plan applied without the passphrase.
#[tokio::test]
async fn missing_or_empty_passphrase_refuses_before_any_change() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &root(202, "32G")).await?;
    for (passphrase, expected) in [
        (
            None,
            "LUKS requires a passphrase. Apply the plan with secrets.luks_passphrase; no disk changes made",
        ),
        (Some(""), "Empty LUKS passphrase; no disk changes made"),
    ] {
        sim.clear_ops();
        let error = message(run(&pve, &plan, passphrase).await);
        assert_eq!(error, expected);
        assert!(guest_commands(&sim).is_empty());
        assert!(resize_calls(&sim).is_empty());
    }
    Ok(())
}
