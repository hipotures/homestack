//! Prepare the real install form without applying a HomeStack installation.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{Options, Result, service_fixture::Fixture};

pub fn ssh(dir: &Path) -> Command {
    let mut command = Command::new("ssh");
    command
        .args([
            "-F",
            &dir.join("ssh_config").display().to_string(),
            "-o",
            "BatchMode=yes",
            "pve-e2e",
            "bash",
            "-se",
        ])
        .stdin(Stdio::piped())
        .kill_on_drop(true);
    command
}

pub async fn checked_ssh(dir: &Path, script: &str, log: &Path) -> Result<Vec<u8>> {
    let mut child = ssh(dir).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().ok_or("SSH stdin unavailable")?;
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await??;
    use std::io::Write;
    let mut file = std::fs::File::options()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(log)?;
    file.write_all(&output.stdout)?;
    file.write_all(&output.stderr)?;
    if !output.status.success() {
        return Err(format!("E2E service preparation failed; see {}", log.display()).into());
    }
    Ok(output.stdout)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or("missing private file directory")?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    if path.is_symlink() {
        return Err("refusing to replace a symlink in the E2E profile".into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.persist(path)?;
    Ok(())
}

pub async fn prepare(options: &Options, dir: &Path, artifacts: &Path, report: &Value) -> Result<Value> {
    let run_id = uuid::Uuid::parse_str(report["run_id"].as_str().ok_or("run has no ID")?)?;
    let log = artifacts.join("service-preparation.log");
    let guard = format!(
        "python3 -c 'import json; assert json.load(open(\"/etc/homestack-e2e.json\"))[\"run_id\"] == \"{run_id}\"'\nhostname -s\n"
    );
    let node = String::from_utf8(checked_ssh(dir, &guard, &log).await?)?
        .trim()
        .to_owned();
    let layout = hs_model::config::active_layout_name(&hs_model::NodeName::parse(&node)?)?;
    let config = include_str!("service-config.toml")
        .replace("@NODE@", &node)
        .replace("@LAYOUT@", &layout);
    let _ = hs_config::legacy::read(&config)?;
    let mut fixture = Fixture::open(&options.fixtures, &options.spaceship_file, &options.tsig_file)?;
    eprintln!("Preparing HomeStack API DNS and a public wildcard certificate...");
    let outcome = fixture.prepare(options.local_dns).await;
    let cleanup = fixture.cleanup_challenge().await;
    let stored = outcome?;
    cleanup?;
    eprintln!("Preparing internal DNS and the Proxmox OCI image tools...");
    crate::install::run(
        ssh(dir),
        include_str!("service-network.sh"),
        Duration::from_secs(600),
        &log,
    )
    .await?;
    let profile = dir.join("client/config/homestack");
    let keys = dir.join("client/home/.ssh");
    std::fs::create_dir_all(&keys)?;
    std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700))?;
    if !keys.join("e2e-workspace").exists() {
        let status = Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                &keys.join("e2e-workspace").display().to_string(),
            ])
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            return Err("cannot generate the isolated workspace SSH key".into());
        }
    }
    if !profile.join("config.toml").exists() {
        write_private(&profile.join("config.toml"), config.as_bytes())?;
    }
    let old = hs_config::legacy::read(&std::fs::read_to_string(profile.join("config.toml"))?)?;
    if old.server.network.gateway != "10.77.0.1"
        || old.server.network.cidr != 24
        || old.server.network.prefix != "10.77.0"
    {
        return Err("the E2E install profile must use its isolated 10.77.0.0/24 network".into());
    }
    write_private(&profile.join("acme/cert.pem"), stored.cert.as_bytes())?;
    write_private(&profile.join("acme/key.pem"), stored.key.as_bytes())?;
    // The same lab-only wildcard covers the browser alias and the numeric
    // node's internal API name. Provider credentials stay on the runner.
    use base64::Engine;
    let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
    let tls = format!(
        "umask 077\ndirectory=$(mktemp -d /run/hs-e2e-service-tls.XXXXXX)\ntrap 'rm -rf -- \"$directory\"' EXIT\nprintf '%s' '{}' | base64 -d > \"$directory/cert.pem\"\nprintf '%s' '{}' | base64 -d > \"$directory/key.pem\"\npvenode cert set \"$directory/cert.pem\" \"$directory/key.pem\" --force\nsystemctl restart pveproxy\n",
        encode(&stored.cert),
        encode(&stored.key)
    );
    checked_ssh(dir, &tls, &log).await?;
    let image = if let Ok(bytes) = std::fs::read(dir.join("service.json")) {
        let previous: Value = serde_json::from_slice(&bytes)?;
        if previous["run_id"] != run_id.to_string() {
            return Err("service settings belong to a different run".into());
        }
        previous["image"]
            .as_str()
            .ok_or("service settings have no image")?
            .to_owned()
    } else {
        eprintln!("Selecting the newest published HomeStack server image...");
        hs_deploy::registry::recent("ghcr.io/hipotures/homestack-server:latest", 10, &[])
            .await?
            .into_iter()
            .next()
            .ok_or("the HomeStack registry has no published image")?
            .image
    };
    let volume = format!("local:vztmpl/{}", hs_deploy::node::image_file(&image)?);
    let pull = include_str!("service-image.py");
    let registry = crate::registry_proxy::start().await?;
    let mut tunnel = crate::tunnel::Tunnel::start(dir, registry.port).await?;
    eprintln!("Checking the native Proxmox OCI image cache for {image}...");
    let script = format!(
        "export HTTPS_PROXY=http://127.0.0.1:3129 https_proxy=http://127.0.0.1:3129\npython3 - {} {} {} <<'PULL'\n{pull}\nPULL\n",
        crate::phases::shell_quote(&image),
        crate::phases::shell_quote(&node),
        crate::phases::shell_quote(&volume)
    );
    let result = crate::install::run(ssh(dir), &script, Duration::from_secs(950), &log).await;
    tunnel.stop().await?;
    result?;
    let metadata = json!({"run_id": run_id.to_string(), "image": image, "hostname": crate::service_fixture::HOST,
        "address": crate::service_fixture::ADDRESS, "certificate_not_after": stored.not_after,
        "certificate_names": stored.names, "image_volume": volume,
        "fixture_directory": fixture.dir, "profile": profile, "prepared_at_ms": crate::phases::now_ms()});
    write_private(&dir.join("service.json"), &serde_json::to_vec_pretty(&metadata)?)?;
    write_private(
        &artifacts.join("service-preparation.json"),
        &serde_json::to_vec_pretty(&metadata)?,
    )?;
    eprintln!(
        "HomeStack install inputs ready: {} -> {}; image {image}",
        crate::service_fixture::HOST,
        crate::service_fixture::ADDRESS
    );
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_config_is_complete_without_gold_or_production_settings() -> Result<()> {
        let config = include_str!("service-config.toml")
            .replace("@NODE@", "pve1")
            .replace("@LAYOUT@", "homestack-storage-1");
        let parsed = hs_config::legacy::read(&config)?;
        assert_eq!(parsed.server.gold_vmid, None);
        assert_eq!(parsed.server.network.gateway, "10.77.0.1");
        assert!(parsed.client.server.is_none());
        assert_eq!(parsed.client.workspace_ssh.identity_files, ["~/.ssh/e2e-workspace"]);
        Ok(())
    }

    #[test]
    fn native_image_cache_accepts_cli_progress_and_reuses_the_archive() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let state = dir.path().join("cached");
        let count = dir.path().join("pull-count");
        let pvesh = dir.path().join("pvesh");
        std::fs::write(
            &pvesh,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
cache = pathlib.Path(os.environ['E2E_CACHE'])
if sys.argv[1] == 'get':
    print(json.dumps([{'volid': 'local:vztmpl/server_tag.tar'}] if cache.exists() else []))
else:
    if os.environ.get('E2E_FAIL'):
        sys.exit('registry unavailable')
    pathlib.Path(os.environ['E2E_COUNT']).write_text('one pull')
    cache.write_text('archive')
    print('Getting image source signatures\nCopying blob\nWriting manifest')
"#,
        )?;
        std::fs::set_permissions(&pvesh, std::fs::Permissions::from_mode(0o700))?;
        let mut paths = vec![dir.path().to_owned()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let run = |fail: bool| -> Result<std::process::Output> {
            let mut command = std::process::Command::new("python3");
            command
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service-image.py"))
                .args(["ghcr.io/test/server:tag", "pve1", "local:vztmpl/server_tag.tar"])
                .env("PATH", std::env::join_paths(&paths)?)
                .env("E2E_CACHE", &state)
                .env("E2E_COUNT", &count);
            if fail {
                command.env("E2E_FAIL", "1");
            }
            Ok(command.output()?)
        };
        let first = run(false)?;
        assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
        assert!(String::from_utf8(first.stdout)?.contains("[READY]"));
        let reused = run(false)?;
        assert!(reused.status.success());
        assert!(String::from_utf8(reused.stdout)?.contains("[REUSED]"));
        assert_eq!(std::fs::read_to_string(&count)?, "one pull");
        std::fs::remove_file(&state)?;
        assert!(!run(true)?.status.success());
        assert!(!state.exists());
        Ok(())
    }
}
