//! Only device writes need root@pam; ordinary lifecycle calls keep the deploy token.
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use hs_core::pve::Pve;
use hs_deploy::{config::DeployConfig, devices, node};
use hs_install::Root;
use hs_pve::{PveClient, PveError};
use serde_json::{Value, json};

use crate::{root_ssh::SshRoot, system::System, touch::Sounds};

pub struct Access {
    pub system: Arc<dyn System>,
    pub home: PathBuf,
    pub prefix: Vec<String>,
    pub sounds: Sounds,
    pub stop: Arc<AtomicBool>,
}

pub struct DevicePve {
    pve: PveClient,
    target: String,
    access: Option<Access>,
    root: Mutex<Option<SshRoot>>,
}

impl DevicePve {
    pub fn new(config: &DeployConfig, access: Option<Access>) -> Result<Self, String> {
        let host = config
            .pve
            .endpoints
            .first()
            .and_then(|url| url.host_str())
            .ok_or("Proxmox endpoint is missing")?;
        let target = if host.contains(':') {
            format!("root@[{host}]")
        } else {
            format!("root@{host}")
        };
        Ok(Self {
            pve: hs_deploy::deploy::deploy_client(config)?,
            target,
            access,
            root: Mutex::new(None),
        })
    }

    fn write_devices(&self, path: &str, form: &[(&str, &str)]) -> Result<(), PveError> {
        let fail = |message: String| PveError::Response(message);
        let segments: Vec<_> = path.split('/').collect();
        if segments.len() != 6
            || segments[1] != "nodes"
            || segments[3] != "lxc"
            || segments[5] != "config"
            || segments[4].parse::<u32>().is_err()
        {
            return Err(fail("Invalid device configuration endpoint".into()));
        }
        if !form.iter().any(|(key, value)| *key == "digest" && !value.is_empty()) {
            return Err(fail("Device writes require the reviewed configuration revision".into()));
        }
        let mut config = json!({});
        for (key, value) in form {
            if devices::valid_slot(key) {
                config[*key] = json!(value);
            }
        }
        devices::read(&config).map_err(fail)?;
        let mut command = format!("pvesh set {} --output-format json", hs_install::shell::quote(path));
        for (key, value) in form {
            if devices::valid_slot(key) || *key == "digest" {
                command.push_str(&format!(" --{key} {}", hs_install::shell::quote(value)));
            }
        }
        let access = self
            .access
            .as_ref()
            .ok_or_else(|| fail("Device passthrough requires root SSH access and authentication prompts".into()))?;
        let mut root = self
            .root
            .lock()
            .map_err(|_| fail("Device SSH session is unavailable".into()))?;
        if root.is_none() {
            *root = Some(
                SshRoot::open_asking(
                    Arc::clone(&access.system),
                    &access.home,
                    &self.target,
                    None,
                    &access.sounds,
                    access.prefix.clone(),
                    Arc::clone(&access.stop),
                )
                .map_err(|e| fail(e.to_string()))?,
            );
        }
        let output = root
            .as_mut()
            .ok_or_else(|| fail("Device SSH session is unavailable".into()))?
            .run(&command)
            .map_err(|e| fail(e.to_string()))?;
        if output.code != 0 {
            return Err(fail(format!("Device passthrough: {}", output.detail())));
        }
        Ok(())
    }
}

fn verify_write(before: &Value, after: &Value, form: &[(&str, &str)]) -> Result<(), PveError> {
    let mut requested = json!({});
    for (key, value) in form {
        if devices::valid_slot(key) {
            requested[*key] = json!(value);
        }
    }
    let expected = devices::merged(
        &devices::read(before).map_err(PveError::Response)?,
        &devices::read(&requested).map_err(PveError::Response)?,
    )
    .map_err(PveError::Response)?;
    let ordinary = |cfg: &Value| {
        cfg.as_object().map(|fields| {
            fields
                .iter()
                .filter(|(key, _)| key.as_str() != "digest" && !devices::valid_slot(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>()
        })
    };
    if ordinary(before) != ordinary(after) || devices::read(after).map_err(PveError::Response)? != expected {
        return Err(PveError::Response(
            "Configuration changed while writing devices; review again".into(),
        ));
    }
    Ok(())
}

impl Pve for DevicePve {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.pve.get(path, query).await
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        if !form.iter().any(|(key, _)| devices::valid_slot(key)) {
            return self.pve.post(path, form).await;
        }
        let plain: Vec<_> = form
            .iter()
            .copied()
            .filter(|(key, _)| !devices::valid_slot(key))
            .collect();
        let upid = self.pve.post(path, &plain).await?;
        node::wait(&self.pve, &upid, "create container before attaching devices")
            .await
            .map_err(PveError::Response)?;
        let ctid = form
            .iter()
            .find(|(key, _)| *key == "vmid")
            .map(|(_, value)| *value)
            .ok_or_else(|| PveError::Response("Device CTID is missing".into()))?;
        let path = format!("{path}/{ctid}/config");
        let cfg = self.pve.get(&path, &[]).await?;
        let mut fields: Vec<_> = form
            .iter()
            .copied()
            .filter(|(key, _)| devices::valid_slot(key))
            .collect();
        fields.push((
            "digest",
            cfg["digest"]
                .as_str()
                .ok_or_else(|| PveError::Response("Configuration revision is missing".into()))?,
        ));
        tokio::task::block_in_place(|| self.write_devices(&path, &fields))?;
        verify_write(&cfg, &self.pve.get(&path, &[]).await?, &fields)?;
        Ok(upid)
    }

    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        if !form.iter().any(|(key, _)| devices::valid_slot(key)) {
            return self.pve.put(path, form).await;
        }
        let before = self.pve.get(path, &[]).await?;
        tokio::task::block_in_place(|| self.write_devices(path, form))?;
        let cfg = self.pve.get(path, &[]).await?;
        verify_write(&before, &cfg, form)?;
        let mut plain: Vec<_> = form
            .iter()
            .copied()
            .filter(|(key, _)| !devices::valid_slot(key) && *key != "digest")
            .collect();
        if plain.is_empty() {
            return Ok(Value::Null);
        }
        plain.push((
            "digest",
            cfg["digest"]
                .as_str()
                .ok_or_else(|| PveError::Response("Configuration revision is missing".into()))?,
        ));
        self.pve.put(path, &plain).await
    }

    async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.pve.delete(path, query).await
    }
}
