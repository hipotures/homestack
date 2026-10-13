//! `addkey` (C4) on the simulator, and the server's part of
//! `python/tests/test_addkey.py` (`docs/migration/phase-9.md`): `KeyTests`,
//! `VendorTests` (the key step) and `FlowTests`.
//!
//! The cluster is `data/addkey-cluster.toml`. The key step runs in `bash`
//! here, as Python's tests ran it.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::addkey::{
    AddkeyRequest, ENSURE_DONE, ENSURE_LOOP, ENSURE_MARKER, KEYS_HEREDOC, add_key, ensure_keys_script, merge_sshkeys,
    plan_addkey, read_public_key,
};
use hs_core::cloudinit::{HomeFormat, vendor_snippet};
use hs_core::guest::read_text;
use hs_core::lifecycle::LifecycleTiming;
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
const CLUSTER: &str = include_str!("data/addkey-cluster.toml");
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

const YUBIKEY: &str = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1 yubikey1";
const TREZOR: &str = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBHRyZXpvcg== trezor-vm";

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

async fn cluster() -> Result<(PveSim, PveClient, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    let admin = client(&sim, ADMIN)?;
    Ok((sim, pve, admin))
}

fn request(vmid: u32, key: Option<&str>, restart: bool) -> AddkeyRequest {
    AddkeyRequest {
        vmid,
        key: key.map(|key| format!("{key}\n")),
        restart,
    }
}

async fn plan(pve: &PveClient, request: &AddkeyRequest) -> Result<Value, CoreError> {
    plan_addkey(
        pve,
        &config().map_err(|e| CoreError::Invalid(e.to_string()))?,
        POOL,
        request,
    )
    .await
}

async fn apply(pve: &PveClient, plan: &Value) -> Result<Value, CoreError> {
    let cfg = config().map_err(|e| CoreError::Invalid(e.to_string()))?;
    add_key(pve, &cfg, POOL, plan, &quick(), &NoObserver).await
}

/// The calls that may change something: not `GET` and not the agent's ping.
fn changes(sim: &PveSim) -> Vec<String> {
    sim.ops()
        .iter()
        .filter(|op| op.name != "get" && op.name != "guest_ping")
        .map(ToString::to_string)
        .collect()
}

async fn file(admin: &PveClient, vmid: u32, path: &str) -> Result<String, Error> {
    Ok(read_text(admin, NODE, vmid, path).await?.unwrap_or_default())
}

async fn vm_config(admin: &PveClient, vmid: u32) -> Result<Value, Error> {
    Ok(admin
        .get_value(&["nodes", NODE, "qemu", &vmid.to_string(), "config"], &[])
        .await?)
}

async fn status(admin: &PveClient, vmid: u32) -> Result<String, Error> {
    let current = admin
        .get_value(&["nodes", NODE, "qemu", &vmid.to_string(), "status", "current"], &[])
        .await?;
    Ok(current["status"].as_str().unwrap_or_default().to_owned())
}

/// The key lines of a VM's `sshkeys`.
async fn sshkeys(admin: &PveClient, vmid: u32) -> Result<Vec<String>, Error> {
    let cfg = vm_config(admin, vmid).await?;
    let encoded = cfg["sshkeys"].as_str().unwrap_or_default();
    Ok(hs_core::keys::unquote(encoded)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
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

// --- KeyTests -------------------------------------------------------------------

/// `KeyTests::test_public_key_must_be_one_named_key`; the client reads the
/// file, so "Cannot read" is not the server's.
#[test]
fn public_key_must_be_one_named_key() -> TestResult {
    let record = read_public_key(&format!("{TREZOR}\n"))?;
    assert_eq!(
        (record.comment.as_str(), record.key_type.as_str()),
        ("trezor-vm", "sk-ecdsa-sha2-nistp256@openssh.com")
    );
    assert_eq!(record.line, TREZOR);
    let two = read_public_key(&format!("{TREZOR}\n{YUBIKEY}\n"))
        .err()
        .ok_or("two keys accepted")?;
    assert!(two.to_string().contains("exactly one"), "{two}");
    let unnamed = TREZOR.split(' ').take(2).collect::<Vec<_>>().join(" ");
    let unnamed = read_public_key(&unnamed)
        .err()
        .ok_or("a key without a comment accepted")?;
    assert!(unnamed.to_string().contains("no comment"), "{unnamed}");
    Ok(())
}

/// `KeyTests::test_sshkeys_gain_the_key_once_and_keep_the_others`
#[test]
fn sshkeys_gain_the_key_once_and_keep_the_others() -> TestResult {
    let key = read_public_key(TREZOR)?;
    let current = hs_core::cloudinit::quote_sshkeys(&format!("{YUBIKEY}\n"));
    let (keys, present) = merge_sshkeys(&current, &key);
    assert_eq!(
        (keys.clone(), present),
        (vec![YUBIKEY.to_owned(), TREZOR.to_owned()], false)
    );
    let again = hs_core::cloudinit::quote_sshkeys(&(keys.join("\n") + "\n"));
    assert_eq!(merge_sshkeys(&again, &key), (keys, true));
    assert_eq!(merge_sshkeys("", &key), (vec![TREZOR.to_owned()], false));
    Ok(())
}

// --- VendorTests: the key step -----------------------------------------------

fn bash(script: &str) -> Result<std::process::Output, Error> {
    Ok(std::process::Command::new("bash").arg("-c").arg(script).output()?)
}

/// `VendorTests::test_ensure_step_is_valid_shell_and_adds_without_removing`
#[test]
fn ensure_step_is_valid_shell_and_adds_without_removing() -> TestResult {
    let script = ensure_keys_script(&[YUBIKEY.to_owned(), TREZOR.to_owned()], "user");
    let check = std::process::Command::new("bash")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output()?;
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let posix = std::process::Command::new("sh")
        .arg("-n")
        .arg("-c")
        .arg(&script)
        .output()?;
    assert!(posix.status.success(), "{}", String::from_utf8_lossy(&posix.stderr));
    assert!(script.starts_with(&format!("{ENSURE_MARKER}\n")));
    assert!(script.contains("for dir in /root /home/user; do"));
    assert!(script.ends_with(&format!("echo '{ENSURE_DONE}'\n")));
    assert_eq!(script.matches(&format!("<<'{KEYS_HEREDOC}'")).count(), 1);
    assert!(!script.contains("rm "));
    assert!(!script.contains(" > \"$f\""));
    Ok(())
}

/// `VendorTests::test_ensure_step_appends_only_missing_keys`
#[test]
fn ensure_step_appends_only_missing_keys() -> TestResult {
    let script = ensure_keys_script(&[YUBIKEY.to_owned(), TREZOR.to_owned()], "user");
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    std::fs::create_dir_all(home.join(".ssh"))?;
    let target = home.join(".ssh/authorized_keys");
    std::fs::write(&target, format!("other-key AAAA other\n{YUBIKEY}"))?; // no trailing newline
    let patched = script.replace(
        "for dir in /root /home/user; do",
        &format!("for dir in {}; do", home.display()),
    );
    for _ in 0..2 {
        let run = bash(&patched)?;
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    }
    let text = std::fs::read_to_string(&target)?;
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        ["other-key AAAA other", YUBIKEY, TREZOR]
    );
    Ok(())
}

/// `VendorTests::test_ensure_step_creates_a_missing_file_and_directory_with_private_modes`
#[test]
fn ensure_step_creates_a_missing_file_and_directory_with_private_modes() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let script = ensure_keys_script(&[YUBIKEY.to_owned(), TREZOR.to_owned()], "user");
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    std::fs::create_dir(&home)?;
    let absent = directory.path().join("absent");
    let patched = script.replace(
        "for dir in /root /home/user; do",
        &format!("for dir in {} {}; do", home.display(), absent.display()),
    );
    for _ in 0..2 {
        let run = bash(&patched)?;
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    }
    let target = home.join(".ssh/authorized_keys");
    assert_eq!(
        std::fs::read_to_string(&target)?.lines().collect::<Vec<_>>(),
        [YUBIKEY, TREZOR]
    );
    assert_eq!(std::fs::metadata(&target)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(home.join(".ssh"))?.permissions().mode() & 0o777,
        0o700
    );
    assert!(!absent.exists());
    Ok(())
}

/// The vendor snippets run the same key step as the agent, over root and
/// the home, with the keys of user-data; it comes after the home is mounted.
#[test]
fn vendor_snippets_carry_the_same_key_step() {
    for format in [HomeFormat::Create, HomeFormat::Keep] {
        let vendor = vendor_snippet(format);
        let indented: String = ENSURE_LOOP.lines().map(|line| format!("      {line}\n")).collect();
        assert!(vendor.contains(&indented), "{format:?} lacks the key loop");
        assert!(vendor.contains("      for dir in /root \"$home_path\"; do\n"));
        assert!(vendor.contains(&format!("      echo '{ENSURE_DONE}'\n")));
        let mounted = vendor.find("mountpoint -q \"$home_path\" || mount \"$home_path\"");
        let step = vendor.find("for dir in /root");
        assert!(
            mounted < step && mounted.is_some(),
            "{format:?}: the key step runs before the mount"
        );
    }
}

// --- FlowTests --------------------------------------------------------------------

/// `FlowTests::test_plan_names_the_key_and_every_step`
#[tokio::test]
async fn plan_names_the_key_and_every_step() -> TestResult {
    let (sim, pve, _admin) = cluster().await?;
    let planned = plan(&pve, &request(200, Some(TREZOR), false)).await?;
    assert_eq!(
        (&planned["name"], &planned["ip"], &planned["key"]["comment"]),
        (&json!("alpha"), &json!("192.0.2.200"), &json!("trezor-vm"))
    );
    assert_eq!(planned["keys"], json!([YUBIKEY, TREZOR]));
    assert_eq!(planned["key_already_in_sshkeys"], json!(false));
    assert_eq!(planned["keep_snippet"], json!(true));
    let steps: Vec<&str> = planned["steps"]
        .as_array()
        .ok_or("no steps")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(steps.len(), 6, "{steps:?}");
    assert!(steps.iter().any(|step| step.contains("through the guest agent")));
    assert!(steps.iter().any(|step| step.contains("NOT restarted")));
    assert!(steps.iter().any(|step| step.contains("new SSH host keys")));
    let stopped = plan(&pve, &request(201, Some(TREZOR), false)).await?;
    assert!(stopped["steps"].to_string().contains("stays stopped"));
    let create = plan(&pve, &request(202, Some(TREZOR), false)).await?;
    assert!(
        create["steps"]
            .to_string()
            .contains("Switch to the keep vendor snippet")
    );
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

/// `FlowTests::test_vm_without_homestack_snippets_is_refused`: Python's four
/// per-VM snippets are not the server's.
#[tokio::test]
async fn vm_without_homestack_snippets_is_refused() -> TestResult {
    let (sim, pve, _admin) = cluster().await?;
    let error = plan(&pve, &request(203, Some(TREZOR), false))
        .await
        .err()
        .ok_or("planned")?;
    assert!(
        error.to_string().contains("does not use the HomeStack vendor snippet"),
        "{error}"
    );
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

/// `FlowTests::test_running_vm_gets_the_key_restarts_and_is_verified`: the
/// keys go in through the agent first, then the restart and the check.
#[tokio::test]
async fn running_vm_gets_the_key_restarts_and_is_verified() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(200, Some(TREZOR), true)).await?;
    sim.clear_ops();
    let result = apply(&pve, &planned).await?;
    assert_eq!(
        result,
        json!({
            "ok": true, "command": "addkey", "vmid": 200, "name": "alpha", "ip": "192.0.2.200",
            "key": "trezor-vm", "restarted": true, "pending_restart": false, "verified": true,
            "new_instance_at_next_start": false, "event": "workspace.key_added",
        })
    );
    let calls = changes(&sim);
    let position = |needle: &str| calls.iter().position(|op| op.contains(needle));
    let order = [
        position(&format!("post{{path=/nodes/{NODE}/qemu/200/config}}")).ok_or("no config change")?,
        position("guest_exec{cmd=# HomeStack ensure-keys").ok_or("no key step")?,
        position("/qemu/200/status/shutdown").ok_or("no shutdown")?,
        position("/qemu/200/status/start").ok_or("no start")?,
    ];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{calls:?}");
    assert_eq!(sshkeys(&admin, 200).await?, [YUBIKEY, TREZOR]);
    assert_eq!(
        file(&admin, 200, "/home/developer/.ssh/authorized_keys").await?,
        format!("ssh-ed25519 AAAAother other\n{YUBIKEY}\n{TREZOR}\n")
    );
    assert_eq!(
        file(&admin, 200, "/root/.ssh/authorized_keys").await?,
        format!("{YUBIKEY}\n{TREZOR}\n")
    );
    assert_eq!(status(&admin, 200).await?, "running");
    Ok(())
}

/// `FlowTests::test_running_vm_is_not_restarted_unless_asked`: the keys are
/// in both files at once, so no restart is pending (phase 9, difference 2).
#[tokio::test]
async fn running_vm_is_not_restarted_unless_asked() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(200, Some(TREZOR), false)).await?;
    sim.clear_ops();
    let result = apply(&pve, &planned).await?;
    assert_eq!(
        (&result["restarted"], &result["pending_restart"], &result["verified"]),
        (&json!(false), &json!(false), &json!(true))
    );
    assert_eq!(result["new_instance_at_next_start"], json!(true));
    let calls = changes(&sim);
    assert!(
        calls
            .iter()
            .any(|op| op.contains("guest_exec{cmd=# HomeStack ensure-keys"))
    );
    assert!(!calls.iter().any(|op| op.contains("/status/")), "{calls:?}");
    assert!(
        file(&admin, 200, "/home/developer/.ssh/authorized_keys")
            .await?
            .contains(TREZOR)
    );
    assert!(file(&admin, 200, "/root/.ssh/authorized_keys").await?.contains(TREZOR));
    Ok(())
}

/// `FlowTests::test_restart_question_comes_last_and_a_no_leaves_the_vm_running`:
/// the question is the client's; the restart is a second request, without
/// a key, which applies the keys already in `sshkeys`.
#[tokio::test]
async fn restart_is_a_second_request_after_the_key() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(200, Some(TREZOR), false)).await?;
    apply(&pve, &planned).await?;
    assert_eq!(status(&admin, 200).await?, "running");
    let again = plan(&pve, &request(200, None, true)).await?;
    assert_eq!(again["label"], json!("yubikey1, trezor-vm"));
    assert_eq!(again["key_already_in_sshkeys"], json!(true));
    sim.clear_ops();
    let result = apply(&pve, &again).await?;
    assert_eq!(
        (&result["restarted"], &result["verified"]),
        (&json!(true), &json!(true))
    );
    let calls = changes(&sim);
    assert!(!calls.iter().any(|op| op.contains("/qemu/200/config")), "{calls:?}");
    assert!(calls.iter().any(|op| op.contains("/qemu/200/status/shutdown")));
    Ok(())
}

/// `FlowTests::test_stopped_vm_is_not_started_and_known_hosts_stay`; the
/// next start (by someone else) runs the key step of the snippet.
#[tokio::test]
async fn stopped_vm_is_not_started_and_gets_the_key_at_its_next_start() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(201, Some(TREZOR), true)).await?;
    sim.clear_ops();
    let result = apply(&pve, &planned).await?;
    assert_eq!(
        (&result["restarted"], &result["verified"], &result["pending_restart"]),
        (&json!(false), &json!(false), &json!(false))
    );
    let calls = changes(&sim);
    assert!(
        !calls
            .iter()
            .any(|op| op.contains("/status/") || op.starts_with("guest_exec")),
        "{calls:?}"
    );
    assert_eq!(status(&admin, 201).await?, "stopped");
    assert!(
        !file(&admin, 201, "/home/developer/.ssh/authorized_keys")
            .await
            .unwrap_or_default()
            .contains(TREZOR)
    );

    admin
        .post_value(&["nodes", NODE, "qemu", "201", "status", "start"], &[])
        .await?;
    assert_eq!(
        file(&admin, 201, "/home/developer/.ssh/authorized_keys").await?,
        format!("{YUBIKEY}\n{TREZOR}\n")
    );
    assert!(file(&admin, 201, "/root/.ssh/authorized_keys").await?.contains(TREZOR));
    Ok(())
}

/// `FlowTests::test_failure_before_the_restart_puts_the_snippets_back`:
/// nothing to put back; a failed config change stops before the guest and
/// the restart, and the VM keeps its keys and snippet.
#[tokio::test]
async fn failure_before_the_restart_changes_nothing_else() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(202, Some(TREZOR), true)).await?;
    sim.inject_fault(fault(&format!("post{{path=/nodes/{NODE}/qemu/202/config}}")))?;
    sim.clear_ops();
    let error = apply(&pve, &planned).await.err().ok_or("applied")?;
    assert!(error.to_string().contains("injected failure"), "{error}");
    let calls = changes(&sim);
    assert!(
        !calls
            .iter()
            .any(|op| op.contains("/status/") || op.starts_with("guest_exec")),
        "{calls:?}"
    );
    assert_eq!(sshkeys(&admin, 202).await?, [YUBIKEY]);
    assert_eq!(
        vm_config(&admin, 202).await?["cicustom"],
        json!("vendor=local:snippets/homestack-vendor-create.yaml")
    );
    assert_eq!(status(&admin, 202).await?, "running");
    Ok(())
}

/// `FlowTests::test_cloud_init_error_and_missing_key_are_reported`
#[tokio::test]
async fn cloud_init_error_and_missing_key_are_reported() -> TestResult {
    let (_sim, pve, _admin) = cluster().await?;
    let planned = plan(&pve, &request(207, Some(TREZOR), true)).await?;
    let error = apply(&pve, &planned).await.err().ok_or("applied")?;
    assert!(error.to_string().contains("did not report 'done'"), "{error}");

    let planned = plan(&pve, &request(206, Some(TREZOR), false)).await?;
    let error = apply(&pve, &planned).await.err().ok_or("applied")?.to_string();
    assert!(
        error.starts_with("Not all keys are in root and developer authorized_keys of VM 206: "),
        "{error}"
    );
    assert!(
        error.contains("trezor-vm in /home/developer/.ssh/authorized_keys"),
        "{error}"
    );
    assert!(!error.contains("in /root/"), "{error}");
    Ok(())
}

/// `FlowTests::test_without_a_key_the_existing_sshkeys_are_applied`
#[tokio::test]
async fn without_a_key_the_existing_sshkeys_are_applied() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(208, None, false)).await?;
    assert_eq!(planned["key"], Value::Null);
    assert_eq!(
        (&planned["label"], &planned["keys"]),
        (&json!("yubikey1, trezor-vm"), &json!([YUBIKEY, TREZOR]))
    );
    assert!(
        planned["steps"][0]
            .as_str()
            .unwrap_or_default()
            .starts_with("Keep the Proxmox sshkeys as they are")
    );
    sim.clear_ops();
    let result = apply(&pve, &planned).await?;
    assert_eq!(result["key"], json!("yubikey1, trezor-vm"));
    let calls = changes(&sim);
    assert!(!calls.iter().any(|op| op.contains("/config")), "{calls:?}");
    assert!(calls.iter().any(|op| op.starts_with("guest_exec")));
    assert_eq!(
        file(&admin, 208, "/home/developer/.ssh/authorized_keys").await?,
        format!("{YUBIKEY}\n{TREZOR}\n")
    );
    Ok(())
}

/// `FlowTests::test_without_a_key_and_without_sshkeys_is_refused`
#[tokio::test]
async fn without_a_key_and_without_sshkeys_is_refused() -> TestResult {
    let (_sim, pve, _admin) = cluster().await?;
    let error = plan(&pve, &request(204, None, false)).await.err().ok_or("planned")?;
    assert!(error.to_string().contains("no keys in its Proxmox sshkeys"), "{error}");
    Ok(())
}

/// `FlowTests::test_key_already_in_sshkeys_skips_qm_set`
#[tokio::test]
async fn key_already_in_sshkeys_skips_the_config_change() -> TestResult {
    let (sim, pve, _admin) = cluster().await?;
    let planned = plan(&pve, &request(208, Some(TREZOR), false)).await?;
    assert_eq!(planned["key_already_in_sshkeys"], json!(true));
    sim.clear_ops();
    apply(&pve, &planned).await?;
    assert!(!changes(&sim).iter().any(|op| op.contains("/config")));
    Ok(())
}

// --- new --------------------------------------------------------------------------

/// A VM still on the create snippet is switched to the keep one, so the
/// restart does not rewrite the home's `authorized_keys`: the user's own
/// key stays (Python's `allow_format=0`).
#[tokio::test]
async fn create_snippet_is_switched_to_keep_and_own_keys_stay() -> TestResult {
    let (_sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(202, Some(TREZOR), true)).await?;
    apply(&pve, &planned).await?;
    assert_eq!(
        vm_config(&admin, 202).await?["cicustom"],
        json!("vendor=local:snippets/homestack-vendor-keep.yaml")
    );
    assert_eq!(
        file(&admin, 202, "/home/developer/.ssh/authorized_keys").await?,
        format!("{YUBIKEY}\nssh-ed25519 AAAAown own-laptop\n{TREZOR}\n")
    );
    Ok(())
}

/// The create snippet would rewrite it: what the switch prevents.
#[tokio::test]
async fn create_snippet_rewrites_the_home_keys_on_a_new_instance() -> TestResult {
    let (_sim, _pve, admin) = cluster().await?;
    let keys = hs_core::cloudinit::quote_sshkeys(&format!("{YUBIKEY}\n{TREZOR}\n"));
    let upid = admin
        .post_value(&["nodes", NODE, "qemu", "202", "config"], &[("sshkeys", keys.as_str())])
        .await?;
    assert!(upid.as_str().is_some());
    for action in ["shutdown", "start"] {
        admin
            .post_value(&["nodes", NODE, "qemu", "202", "status", action], &[])
            .await?;
    }
    assert_eq!(
        file(&admin, 202, "/home/developer/.ssh/authorized_keys").await?,
        format!("{YUBIKEY}\n{TREZOR}\n")
    );
    Ok(())
}

/// A running VM whose agent does not answer keeps the keys in `sshkeys`
/// for its next start, and the result says a restart is pending.
#[tokio::test]
async fn silent_agent_leaves_the_restart_pending() -> TestResult {
    let (_sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(205, Some(TREZOR), false)).await?;
    let result = apply(&pve, &planned).await?;
    assert_eq!(
        (&result["pending_restart"], &result["verified"]),
        (&json!(true), &json!(false))
    );
    assert!(
        result["note"]
            .as_str()
            .unwrap_or_default()
            .contains("added at its next start")
    );
    assert_eq!(sshkeys(&admin, 205).await?, [YUBIKEY, TREZOR]);
    Ok(())
}

/// The pool and the role tags come first, as for the other operations.
#[tokio::test]
async fn alien_and_outside_workspaces_are_refused() -> TestResult {
    let (sim, pve, _admin) = cluster().await?;
    for vmid in [160, 150, 101] {
        let error = plan(&pve, &request(vmid, Some(TREZOR), false))
            .await
            .err()
            .ok_or("planned")?;
        assert!(!error.to_string().is_empty());
    }
    let error = plan(&pve, &request(200, Some("not a key"), false))
        .await
        .err()
        .ok_or("planned")?;
    assert!(error.to_string().contains("Unexpected line"), "{error}");
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}

/// A plan of a running VM is refused once the VM has stopped.
#[tokio::test]
async fn execution_refuses_a_changed_power_state() -> TestResult {
    let (sim, pve, admin) = cluster().await?;
    let planned = plan(&pve, &request(200, Some(TREZOR), false)).await?;
    admin
        .post_value(&["nodes", NODE, "qemu", "200", "status", "stop"], &[])
        .await?;
    sim.clear_ops();
    let error = apply(&pve, &planned).await.err().ok_or("applied")?;
    assert!(error.to_string().contains("generate a new plan"), "{error}");
    assert_eq!(changes(&sim), Vec::<String>::new());
    Ok(())
}
