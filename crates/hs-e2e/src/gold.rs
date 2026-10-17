//! Build and verify the reference Gold inside this run's disposable PVE node.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use hs_model::status::GoldReadiness;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{Result, service};

const VMID: u32 = 100;

fn guard(run_id: &str) -> String {
    format!(
        "python3 -c 'import json; assert json.load(open(\"/etc/homestack-e2e.json\"))[\"run_id\"] == \"{run_id}\"'\n"
    )
}

fn cloud_config(run_id: &str, public: &str, private_host: &str, public_host: &str) -> Value {
    json!({
        "hostname": "gold-e2e", "manage_etc_hosts": true, "disable_root": false,
        "ssh_pwauth": false, "ssh_deletekeys": true,
        "ssh_keys": {"ed25519_private": private_host, "ed25519_public": public_host},
        "users": [
            {"name": "root", "lock_passwd": true, "ssh_authorized_keys": [public.trim()]},
            {"name": "user", "uid": 1000, "lock_passwd": true,
             "shell": "/bin/bash", "sudo": null, "ssh_authorized_keys": [public.trim()]}
        ],
        "package_update": false, "package_upgrade": false,
        "write_files": [
            {"path": "/etc/homestack-e2e-gold.json", "permissions": "0600", "content": json!({"run_id": run_id}).to_string()},
            {"path": "/etc/ssh/sshd_config.d/00-homestack-e2e.conf", "permissions": "0600",
             "content": "PasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin prohibit-password\n"}
        ]
    })
}

fn guest_ssh(dir: &Path) -> Command {
    let mut command = Command::new("ssh");
    command
        .args([
            "-F",
            &dir.join("ssh_config").display().to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "gold-e2e",
            "bash",
            "-se",
        ])
        .stdin(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn output(mut command: Command, script: &str, timeout: Duration) -> Result<std::process::Output> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().ok_or("SSH input is unavailable")?;
    input.write_all(script.as_bytes()).await?;
    drop(input);
    Ok(tokio::time::timeout(timeout, child.wait_with_output()).await??)
}

async fn wait_ssh(dir: &Path) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let result = output(
            guest_ssh(dir),
            "cat /proc/sys/kernel/random/boot_id\n",
            Duration::from_secs(12),
        )
        .await;
        let reason = match result {
            Ok(result) if result.status.success() => return Ok(String::from_utf8(result.stdout)?.trim().to_owned()),
            Ok(result) => String::from_utf8_lossy(&result.stderr).trim().to_owned(),
            Err(error) => error.to_string(),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("reference Gold did not provide pinned root SSH within 120 seconds: {reason}").into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_stopped(dir: &Path, run_id: &str, artifacts: &Path) -> Result<()> {
    let script = format!(
        "{}for attempt in $(seq 1 50); do test \"$(qm status 100)\" = 'status: stopped' && exit 0; sleep 1; done; exit 1\n",
        guard(run_id)
    );
    service::checked_ssh(dir, &script, &artifacts.join("gold.log")).await?;
    Ok(())
}

fn require_live_ready(readiness: &GoldReadiness) -> Result<()> {
    let missing: Vec<_> = readiness
        .checks
        .iter()
        .filter(|check| check.requirement == "required" && check.status != "pass")
        .map(|check| format!("{}: {}", check.name, check.detail))
        .collect();
    let agent = readiness
        .checks
        .iter()
        .any(|check| check.name == "QEMU Guest Agent response" && check.status == "pass");
    if !readiness.ok || !readiness.guest_checked || !agent || readiness.power_state != "running" || !missing.is_empty()
    {
        return Err(format!("reference Gold is not ready: {}", missing.join("; ")).into());
    }
    Ok(())
}

async fn api(dir: &Path, report: &Value) -> Result<(PveClient, String)> {
    let run_id = report["run_id"].as_str().ok_or("missing run ID")?;
    let name = format!("hs-e2e-gold-{}", uuid::Uuid::new_v4().simple());
    // Never write the token response to an artifact.
    let script = format!(
        "{}pveum user token add root@pam {name} --privsep 0 --output-format json\n",
        guard(run_id)
    );
    let result = output(service::ssh(dir), &script, Duration::from_secs(30)).await?;
    if !result.status.success() {
        return Err("cannot create the Gold verification token".into());
    }
    let response: Value = serde_json::from_slice(&result.stdout)?;
    let secret = response["value"]
        .as_str()
        .ok_or("missing Gold verification token secret")?;
    let token = PveToken::parse(&format!("root@pam!{name}={secret}"))?;
    let port = report["api_port"].as_u64().ok_or("missing PVE API port")?;
    let endpoint = format!("https://{}:{port}", crate::domain::HOST).parse()?;
    Ok((PveClient::new(PveClientConfig::new(vec![endpoint], token))?, name))
}

async fn readiness(dir: &Path, artifacts: &Path, report: &Value, live: bool) -> Result<GoldReadiness> {
    let (client, token) = api(dir, report).await?;
    let node = report["gold_node"].as_str().ok_or("missing Gold node")?;
    let config = hs_config::legacy::read(&service::configuration(node)?)?;
    let outcome = hs_core::gold::gold_readiness(&client, "scsi0", &config.server.user, node, VMID).await;
    let cleanup = format!(
        "{}pveum user token delete root@pam {token}\n",
        guard(report["run_id"].as_str().ok_or("missing run ID")?)
    );
    service::checked_ssh(dir, &cleanup, &artifacts.join("gold.log")).await?;
    let result = outcome?;
    let file = if live {
        "gold-readiness.json"
    } else {
        "gold-stopped-readiness.json"
    };
    service::write_private(&artifacts.join(file), &serde_json::to_vec_pretty(&result)?)?;
    if live {
        require_live_ready(&result)?;
    }
    if !result.ok {
        return Err("reference Gold failed the HomeStack readiness checks; inspect gold-readiness.json".into());
    }
    Ok(result)
}

pub async fn prepare(dir: &Path, artifacts: &Path, report: &Value) -> Result<Value> {
    let run_id = uuid::Uuid::parse_str(report["run_id"].as_str().ok_or("missing run ID")?)?.to_string();
    if report["resources"]["memory_mib"]
        .as_u64()
        .is_none_or(|memory| memory < 4096)
    {
        return Err("the PVE and reference Gold lab requires --memory-mib 4096 or more".into());
    }
    let node = String::from_utf8(
        service::checked_ssh(
            dir,
            &format!("{}hostname -s\n", guard(&run_id)),
            &artifacts.join("gold.log"),
        )
        .await?,
    )?
    .trim()
    .to_owned();
    let mut context = report.clone();
    context["gold_node"] = json!(node);
    if dir.join("gold.json").exists() {
        return verify(dir, artifacts, &context).await;
    }
    service::workspace_keys(dir).await?;
    if !dir.join("gold-host-key").exists() {
        crate::vm::command(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                &dir.join("gold-host-key").display().to_string(),
            ],
            artifacts,
            "Gold host key",
        )
        .await?;
    }
    let public = std::fs::read_to_string(dir.join("client/home/.ssh/e2e-workspace.pub"))?;
    let private_host = std::fs::read_to_string(dir.join("gold-host-key"))?;
    let public_host = std::fs::read_to_string(dir.join("gold-host-key.pub"))?;
    let alias = format!("gold-e2e-{run_id}");
    service::write_private(
        &dir.join("gold-known_hosts"),
        format!("{alias} {}\n", public_host.trim()).as_bytes(),
    )?;
    let block = format!(
        "\nHost gold-e2e\n  HostName 10.77.0.100\n  User root\n  Port 22\n  HostKeyAlias {alias}\n  IdentityFile \"{0}/client/home/.ssh/e2e-workspace\"\n  UserKnownHostsFile \"{0}/gold-known_hosts\"\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  IdentitiesOnly yes\n  IdentityAgent none\n  ProxyCommand ssh -F \"{0}/ssh_config\" -W %h:%p pve-e2e\n",
        dir.display()
    );
    let mut ssh = std::fs::read_to_string(dir.join("ssh_config"))?;
    if ssh.contains("Host gold-e2e\n") {
        if !ssh.ends_with(&block) {
            return Err("Gold SSH alias belongs to another fixture".into());
        }
    } else {
        ssh.push_str(&block);
    }
    service::write_private(&dir.join("ssh_config"), ssh.as_bytes())?;
    let state = service::checked_ssh(dir, &format!("{}python3 - <<'STATE'\nimport json,pathlib,subprocess\nif pathlib.Path('/etc/pve/qemu-server/100.conf').exists():\n cfg=json.loads(subprocess.check_output(['pvesh','get','/nodes/{node}/qemu/100/config','--output-format','json']))\n if cfg.get('description') != 'homestack-e2e:{run_id}:gold': raise RuntimeError('VMID 100 is not owned by this fixture')\n print(subprocess.check_output(['qm','status','100'],text=True).strip())\nSTATE\n",guard(&run_id)), &artifacts.join("gold.log")).await?;
    if String::from_utf8(state)?.trim() == "status: running" {
        let shutdown = output(
            guest_ssh(dir),
            "systemctl poweroff --no-block\n",
            Duration::from_secs(15),
        )
        .await?;
        if !shutdown.status.success() {
            return Err("cannot stop the owned partial Gold for preparation retry".into());
        }
        wait_stopped(dir, &run_id, artifacts).await?;
    }
    let source = format!("/var/tmp/hs-e2e-gold-{run_id}.qcow2");
    eprintln!("Copying the already verified Debian image into PVE for Gold 100...");
    crate::vm::command(
        "scp",
        &[
            "-q",
            "-F",
            &dir.join("ssh_config").display().to_string(),
            &dir.join("base.qcow2").display().to_string(),
            &format!("pve-e2e:{source}"),
        ],
        artifacts,
        "Gold image transfer",
    )
    .await?;
    let data = json!({"run_id":run_id, "owner":format!("homestack-e2e:{run_id}:gold"), "source":source,
        "sha512": report["image"]["sha512"], "workspace_key":public,
        "instance_id":format!("{run_id}-gold-{}",uuid::Uuid::new_v4()),
        "mac":format!("BC:24:11:{}:{}:{}", &run_id[0..2], &run_id[2..4], &run_id[4..6]),
        "cloud_config":cloud_config(&run_id,&public,&private_host,&public_host)});
    let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&data)?);
    let create = format!(
        "python3 - <<'GOLD'\nimport base64,json\ndata=json.loads(base64.b64decode('{encoded}'))\n{}\nGOLD\n",
        include_str!("gold-create.py")
    );
    service::checked_ssh(dir, &create, &artifacts.join("gold.log")).await?;
    let first_boot = wait_ssh(dir).await?;
    eprintln!("Installing Gold guest tools, account policy and QEMU Guest Agent...");
    crate::install::run(
        guest_ssh(dir),
        include_str!("gold-guest.sh"),
        Duration::from_secs(600),
        &artifacts.join("gold-install.log"),
    )
    .await?;
    eprintln!("Cold-starting Gold to verify agent autostart and persistent networking...");
    let shutdown = output(
        guest_ssh(dir),
        "systemctl poweroff --no-block\n",
        Duration::from_secs(15),
    )
    .await?;
    if !shutdown.status.success() {
        return Err("Gold did not accept a graceful poweroff".into());
    }
    wait_stopped(dir, &run_id, artifacts).await?;
    service::checked_ssh(
        dir,
        &format!("{}qm start 100\n", guard(&run_id)),
        &artifacts.join("gold.log"),
    )
    .await?;
    let second_boot = wait_ssh(dir).await?;
    if second_boot == first_boot {
        return Err("Gold boot identity did not change after its cold start".into());
    }
    let live = readiness(dir, artifacts, &context, true).await?;
    service::write_private(
        &artifacts.join("gold-readiness.json"),
        &serde_json::to_vec_pretty(&live)?,
    )?;
    let shutdown = output(
        guest_ssh(dir),
        "cloud-init status --wait\ncloud-init clean --logs --machine-id\nsystemctl poweroff --no-block\n",
        Duration::from_secs(30),
    )
    .await?;
    if !shutdown.status.success() {
        return Err("Gold finalization failed".into());
    }
    wait_stopped(dir, &run_id, artifacts).await?;
    let cleanup = format!(
        "{}qm set 100 --delete cicustom\nrm -f /var/lib/vz/snippets/hs-e2e-gold-{run_id}.yaml /var/lib/vz/snippets/hs-e2e-gold-{run_id}-meta.yaml\n",
        guard(&run_id)
    );
    service::checked_ssh(dir, &cleanup, &artifacts.join("gold.log")).await?;
    let metadata = json!({"run_id":run_id,"vmid":VMID,"node":node,"name":"gold-e2e","address":"10.77.0.100",
        "image_sha512":report["image"]["sha512"],"agent_verified_after_cold_start":true,
        "first_boot_id":first_boot,"verified_boot_id":second_boot,"finalized":true,"power_state":"stopped"});
    service::write_private(&dir.join("gold.json"), &serde_json::to_vec_pretty(&metadata)?)?;
    verify(dir, artifacts, &context).await
}

pub async fn verify(dir: &Path, artifacts: &Path, report: &Value) -> Result<Value> {
    let metadata: Value = serde_json::from_slice(&std::fs::read(dir.join("gold.json"))?)?;
    if metadata["run_id"] != report["run_id"]
        || metadata["finalized"] != true
        || metadata["agent_verified_after_cold_start"] != true
    {
        return Err("Gold fixture is not finalized and agent-verified for this run".into());
    }
    let run_id = report["run_id"].as_str().ok_or("missing run ID")?;
    let script = format!(
        "{}python3 - <<'CHECK'\nimport json,subprocess\ncfg=json.loads(subprocess.check_output(['pvesh','get','/nodes/' + subprocess.check_output(['hostname','-s'],text=True).strip() + '/qemu/100/config','--output-format','json']))\nassert cfg.get('description') == 'homestack-e2e:{run_id}:gold', 'Gold ownership changed'\nCHECK\n",
        guard(run_id)
    );
    service::checked_ssh(dir, &script, &artifacts.join("gold.log")).await?;
    let mut context = report.clone();
    context["gold_node"] = metadata["node"].clone();
    let result = readiness(dir, artifacts, &context, false).await?;
    if result.power_state != "stopped" {
        return Err("reference Gold must remain stopped after finalization".into());
    }
    Ok(metadata)
}

/// The application's SSH action owns startup and restores a stopped Gold on logout.
pub async fn shell(artifacts: &Path) -> Result<()> {
    let report: Value = serde_json::from_slice(&std::fs::read(artifacts.join("report.json"))?)?;
    if report["status"] != "paused" {
        return Err("the E2E controller must remain paused during Gold inspection".into());
    }
    let run_id = uuid::Uuid::parse_str(report["run_id"].as_str().ok_or("missing run ID")?)?.to_string();
    let dir = Path::new(report["temporary_directory"].as_str().ok_or("missing lab directory")?);
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("gold-shell.lock"))?;
    lock.try_lock()
        .map_err(|_| "Gold is already being inspected by another SSH session")?;
    drop(std::os::unix::net::UnixStream::connect(dir.join("inspection.sock"))?);
    let metadata: Value = serde_json::from_slice(&std::fs::read(dir.join("gold.json"))?)?;
    if metadata["run_id"] != run_id || metadata["vmid"] != VMID || metadata["finalized"] != true {
        return Err("Gold inspection requires this run's finalized reference Gold".into());
    }
    eprintln!("Opening the lab Gold as root; a stopped Gold will start and return to stopped on logout...");
    let cloud = cloud_config(
        &run_id,
        &std::fs::read_to_string(dir.join("client/home/.ssh/e2e-workspace.pub"))?,
        &std::fs::read_to_string(dir.join("gold-host-key"))?,
        &std::fs::read_to_string(dir.join("gold-host-key.pub"))?,
    );
    let data = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&json!({"run_id": run_id, "cloud_config": cloud}))?);
    let script = format!(
        "python3 - <<'INSPECT'\nimport base64,json\ndata=json.loads(base64.b64decode('{data}'))\n{}\nINSPECT\n",
        include_str!("gold-shell.py")
    );
    let result = service::checked_ssh(dir, &script, &artifacts.join("gold-shell.log")).await?;
    let result: Value = serde_json::from_slice(&result)?;
    let key = result["key"].as_str().ok_or("Gold did not return its host key")?;
    service::write_private(
        &dir.join("gold-known_hosts"),
        format!("gold-e2e-{run_id} {key}\n").as_bytes(),
    )?;
    let status = Command::new("ssh")
        .args(["-F", &dir.join("ssh_config").display().to_string(), "gold-e2e"])
        .kill_on_drop(true)
        .status()
        .await?;
    if result["started"] == true {
        eprintln!("Finalizing Gold and restoring its stopped state for cloning...");
        let shutdown = output(
            guest_ssh(dir),
            "cloud-init status --wait\ncloud-init clean --logs --machine-id\nsystemctl poweroff --no-block\n",
            Duration::from_secs(30),
        )
        .await?;
        if !shutdown.status.success() {
            return Err("Gold finalization failed after SSH; the VM remains available for inspection".into());
        }
        wait_stopped(dir, &run_id, artifacts).await?;
        service::checked_ssh(
            dir,
            &format!(
                "{}qm set 100 --delete cicustom\nrm -f /var/lib/vz/snippets/hs-e2e-gold-{run_id}-inspect.yaml\n",
                guard(&run_id)
            ),
            &artifacts.join("gold-shell.log"),
        )
        .await?;
    }
    if !status.success() {
        return Err(format!("Gold SSH ended with {status}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_refuses_unrelated_runs_vms_and_containers() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for script in ["gold-create.py", "gold-shell.py"] {
            let result = std::process::Command::new("python3")
                .arg(root.join("gold-test.py"))
                .arg(root.join(script))
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .output()?;
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        Ok(())
    }

    #[test]
    fn cloud_seed_has_only_lab_keys_and_the_required_nonprivileged_account() {
        let seed = cloud_config("run", "ssh-ed25519 workspace", "private host", "public host");
        assert_eq!(seed["users"][1]["name"], "user");
        assert_eq!(seed["users"][1]["uid"], 1000);
        assert_eq!(seed["users"][1]["sudo"], Value::Null);
        assert!(seed["users"][1].get("primary_group").is_none());
        assert_eq!(seed["ssh_pwauth"], false);
        assert_eq!(seed["users"][0]["ssh_authorized_keys"][0], "ssh-ed25519 workspace");
    }

    #[test]
    fn a_missing_or_skipped_agent_never_counts_as_ready() -> Result<()> {
        let mut report = GoldReadiness {
            vmid: 100,
            node: "pve1".into(),
            power_state: "running".into(),
            ok: true,
            guest_checked: true,
            checks: vec![hs_model::status::GoldCheck {
                scope: "guest".into(),
                name: "QEMU Guest Agent response".into(),
                status: "pass".into(),
                detail: "responding".into(),
                requirement: "required".into(),
            }],
        };
        require_live_ready(&report)?;
        for status in ["fail", "skip"] {
            report.checks[0].status = status.into();
            assert!(require_live_ready(&report).is_err());
        }
        Ok(())
    }
}
