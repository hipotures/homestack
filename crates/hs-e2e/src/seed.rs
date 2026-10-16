use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use base64::Engine;
use serde_json::{Value, json};

use crate::Result;
use crate::vm::command;

pub async fn create(dir: &Path, data: &Value, hostname: &str, artifacts: &Path) -> Result<()> {
    for name in ["identity", "host-key"] {
        command(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                &dir.join(name).display().to_string(),
            ],
            artifacts,
            "key generation",
        )
        .await?;
    }
    let identity = std::fs::read_to_string(dir.join("identity.pub"))?;
    let host_private = std::fs::read_to_string(dir.join("host-key"))?;
    let host_public = std::fs::read_to_string(dir.join("host-key.pub"))?;
    let client = reqwest::Client::builder().no_proxy().https_only(true).build()?;
    let key = client
        .get("https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let seed = user_data(data, hostname, &identity, &host_private, &host_public, &key)?;
    std::fs::write(
        dir.join("user-data"),
        format!("#cloud-config\n{}\n", serde_json::to_string_pretty(&seed)?),
    )?;
    std::fs::write(
        dir.join("meta-data"),
        serde_json::to_vec(&json!({"instance-id": data["run_id"], "local-hostname": hostname}))?,
    )?;
    for name in ["user-data", "meta-data"] {
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600))?;
    }
    command(
        "mkfs.vfat",
        &[
            "-n",
            "CIDATA",
            "-C",
            &dir.join("seed.img").display().to_string(),
            "2048",
        ],
        artifacts,
        "seed filesystem",
    )
    .await?;
    std::fs::set_permissions(dir.join("seed.img"), std::fs::Permissions::from_mode(0o600))?;
    for name in ["user-data", "meta-data"] {
        command(
            "mcopy",
            &[
                "-i",
                &dir.join("seed.img").display().to_string(),
                &dir.join(name).display().to_string(),
                &format!("::{name}"),
            ],
            artifacts,
            "seed content",
        )
        .await?;
    }
    Ok(())
}

fn user_data(data: &Value, hostname: &str, identity: &str, private: &str, public: &str, key: &[u8]) -> Result<Value> {
    Ok(json!({
        "hostname": hostname.split('.').next().ok_or("missing hostname")?,
        "fqdn": hostname,
        "preserve_hostname": false,
        "manage_etc_hosts": false,
        "disable_root": false,
        "ssh_pwauth": false,
        "ssh_deletekeys": true,
        "ssh_keys": {"ed25519_private": private, "ed25519_public": public},
        "users": [{"name": "root", "lock_passwd": true, "ssh_authorized_keys": [identity.trim()]}],
        "package_update": false,
        "package_upgrade": false,
        "apt": {"preserve_sources_list": true},
        "runcmd": [["systemctl", "restart", "systemd-resolved"]],
        "write_files": [
            {"path": "/etc/systemd/resolved.conf.d/homestack-e2e.conf", "content": "[Resolve]\nDomains=e2e.test\n"},
            {"path": "/etc/homestack-e2e.json", "permissions": "0600", "content": serde_json::to_string(data)?},
            {"path": "/etc/hosts", "content": format!("127.0.0.1 localhost\n10.0.2.15 {hostname} {}\n", hostname.split('.').next().ok_or("missing hostname")?)},
            {"path": "/usr/share/keyrings/proxmox-archive-keyring.gpg", "encoding": "b64", "content": base64::engine::general_purpose::STANDARD.encode(key)},
            {"path": "/etc/apt/apt.conf.d/80homestack-e2e", "content": "Acquire::http::Proxy \"http://10.0.2.100:3128/\";\nAcquire::Retries \"2\";\n"},
            {"path": "/etc/apt/sources.list.d/debian.sources", "content": "Types: deb\nURIs: http://deb.debian.org/debian\nSuites: trixie trixie-updates\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\nTypes: deb\nURIs: http://security.debian.org/debian-security\nSuites: trixie-security\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n"},
            {"path": "/etc/apt/sources.list.d/pve.sources", "content": "Types: deb\nURIs: http://download.proxmox.com/debian/pve\nSuites: trixie\nComponents: pve-no-subscription\nSigned-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n"}
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_data_is_json_not_interpolated_shell() -> Result<()> {
        let data = json!({"run_id": "id", "message": "quote\"\n$(touch /bad)"});
        let seed = user_data(&data, "test.e2e.test", "ssh-ed25519 test", "private", "public", b"key")?;
        let file = seed["write_files"]
            .as_array()
            .ok_or("missing files")?
            .iter()
            .find(|file| file["path"] == "/etc/homestack-e2e.json")
            .ok_or("missing initialization file")?;
        let content = file["content"].as_str().ok_or("missing content")?;
        assert_eq!(serde_json::from_str::<Value>(content)?, data);
        assert_eq!(seed["users"][0]["lock_passwd"], true);
        assert_eq!(seed["ssh_pwauth"], false);
        assert_eq!(seed["hostname"], "test");
        assert_eq!(seed["fqdn"], "test.e2e.test");
        Ok(())
    }
}
