//! The container steps of `homestack-deploy` against a stand-in for the
//! Proxmox API that records every call and answers tasks at once.

use std::sync::Mutex;

use hs_core::pve::Pve;
use hs_deploy::node::{self, NewContainer};
use hs_pve::PveError;
use serde_json::{Value, json};

#[derive(Default)]
struct FakePve {
    calls: Mutex<Vec<String>>,
    images: Vec<String>,
    containers: Value,
    env: String,
    pending_env: bool,
    description: String,
    net0: String,
    tags: String,
}

impl FakePve {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }

    fn record(&self, call: String) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
    }
}

const UPID: &str = "UPID:pve2:00000001:00000001:00000001:vzcreate:998:homestack@pve!deploy:";

impl Pve for FakePve {
    async fn get(&self, path: &str, _query: &[(&str, &str)]) -> Result<Value, PveError> {
        Ok(match path {
            p if p.ends_with("/content") => {
                Value::Array(self.images.iter().map(|volid| json!({"volid": volid})).collect())
            }
            "/cluster/resources" => self.containers.clone(),
            p if p.ends_with("/config") => {
                json!({"env": self.env, "digest": "d1", "description": self.description, "net0": self.net0, "tags": self.tags})
            }
            p if p.ends_with("/pending") => {
                if self.pending_env {
                    json!([{"key": "env", "value": "old", "pending": "new"}])
                } else {
                    json!([{"key": "env", "value": "same"}])
                }
            }
            p if p.contains("/tasks/") && p.ends_with("/status") => json!({"status": "stopped", "exitstatus": "OK"}),
            p if p.contains("/tasks/") && p.ends_with("/log") => json!([]),
            other => json!({"unexpected": other}),
        })
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        let keys: Vec<&str> = form.iter().map(|(key, _)| *key).collect();
        self.record(format!("POST {path} {}", keys.join(",")));
        Ok(json!(UPID))
    }

    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        let text: Vec<String> = form.iter().map(|(key, value)| format!("{key}={value}")).collect();
        self.record(format!("PUT {path} {}", text.join("&")));
        Ok(Value::Null)
    }

    async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        let keys: Vec<String> = query.iter().map(|(key, value)| format!("{key}={value}")).collect();
        self.record(format!("DELETE {path} {}", keys.join("&")));
        Ok(json!(UPID))
    }
}

const IMAGE: &str = "ghcr.io/hipotures/homestack-server:0.1.0-0123456789ab";

#[tokio::test]
async fn the_image_is_pulled_once_per_node() -> Result<(), String> {
    let quiet = |_: &str| {};
    let pve = FakePve::default();
    let volid = node::ensure_image(&pve, "pve2", "local", IMAGE, &quiet).await?;
    assert_eq!(volid, "local:vztmpl/homestack-server_0.1.0-0123456789ab.tar");
    assert_eq!(
        pve.calls(),
        ["POST /nodes/pve2/storage/local/oci-registry-pull reference"]
    );

    let pulled = FakePve {
        images: vec![volid.clone()],
        ..FakePve::default()
    };
    assert_eq!(
        node::ensure_image(&pulled, "pve2", "local", IMAGE, &quiet).await?,
        volid
    );
    assert!(pulled.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_existing_container_is_stopped_then_destroyed_with_its_volumes() -> Result<(), String> {
    let quiet = |_: &str| {};
    let pve = FakePve {
        containers: json!([
            {"type": "qemu", "vmid": 998, "node": "pve1", "status": "running"},
            {"type": "lxc", "vmid": 998, "node": "pve3", "status": "running", "name": "api2"}
        ]),
        ..FakePve::default()
    };
    let found = node::container(&pve, 998).await?.ok_or("CT 998 not found")?;
    assert_eq!(found.node, "pve3", "only containers count");
    node::destroy(&pve, &found, 998, &quiet).await?;
    assert_eq!(
        pve.calls(),
        [
            "POST /nodes/pve3/lxc/998/status/stop ",
            "DELETE /nodes/pve3/lxc/998 purge=1&destroy-unreferenced-disks=1"
        ]
    );
    assert_eq!(node::container(&FakePve::default(), 998).await?, None);
    Ok(())
}

#[tokio::test]
async fn a_new_container_gets_its_settings_and_environment() -> Result<(), String> {
    let quiet = |_: &str| {};
    let pve = FakePve {
        env: "PATH=/usr/bin:/bin\0HOMESTACK_TLS_KEY_B64=secret".to_owned(),
        pending_env: true,
        ..FakePve::default()
    };
    let spec = NewContainer {
        node: "pve2",
        ctid: 998,
        hostname: "api2",
        ostemplate: "local:vztmpl/homestack-server_0.1.0-0123456789ab.tar",
        pool: "homestack-service",
        storage: "local-lvm",
        disk_gib: 2,
        memory_mib: 256,
        cores: 1,
        net0: "name=eth0,bridge=vmbr0,ip=192.0.2.98/24,gw=192.0.2.1".to_owned(),
        nameserver: None,
        description: "HomeStack server".to_owned(),
    };
    node::create(&pve, &spec, &quiet).await?;
    node::set_env(&pve, "pve2", 998, &[], &["HOMESTACK_TLS_KEY_B64"]).await?;
    assert!(node::env_pending(&pve, "pve2", 998).await?);
    assert_eq!(
        pve.calls(),
        [
            "POST /nodes/pve2/lxc vmid,ostemplate,hostname,pool,rootfs,memory,swap,cores,net0,unprivileged,onboot,description",
            "PUT /nodes/pve2/lxc/998/config tags=homestack-server&digest=d1",
            "PUT /nodes/pve2/lxc/998/config env=PATH=/usr/bin:/bin&digest=d1"
        ]
    );
    assert_eq!(
        node::env_keys(&pve, "pve2", 998).await?,
        ["PATH", "HOMESTACK_TLS_KEY_B64"]
    );
    Ok(())
}

#[tokio::test]
async fn old_server_images_are_listed_not_others() -> Result<(), String> {
    let pve = FakePve {
        images: vec![
            "local:vztmpl/homestack-server_0.1.0-0123456789ab.tar".to_owned(),
            "local:vztmpl/homestack-server_0.1.0-aaaaaaaaaaaa.tar".to_owned(),
            "local:vztmpl/python_3.14-alpine3.24.tar".to_owned(),
        ],
        ..FakePve::default()
    };
    let current = node::image_file(IMAGE)?;
    assert_eq!(
        node::old_images(&pve, "pve2", "local", &current).await?,
        ["local:vztmpl/homestack-server_0.1.0-aaaaaaaaaaaa.tar"]
    );
    Ok(())
}

#[tokio::test]
async fn a_container_with_the_same_image_certificate_and_address_is_kept() -> Result<(), String> {
    let description = "HomeStack server api1.example.net\nimage i:1\ncertificate 1700000000\nmade by homestack-deploy";
    let net0 = "name=eth0,bridge=vmbr0,ip=192.0.2.97/24,gw=192.0.2.1";
    let proxmox_net0 =
        "name=eth0,bridge=vmbr0,gw=192.0.2.1,hwaddr=BC:24:11:2E:C1:09,ip=192.0.2.97/24,type=veth,host-managed=1";
    let pve = FakePve {
        description: description.to_owned(),
        net0: proxmox_net0.to_owned(),
        ..FakePve::default()
    };
    let quiet = |_: &str| {};
    assert!(node::unchanged(&pve, "pve1", 2097, description, net0, &quiet).await?);
    let newer = description.replace("image i:1", "image i:2");
    assert!(
        !node::unchanged(&pve, "pve1", 2097, &newer, net0, &quiet).await?,
        "a new image"
    );
    let renewed = description.replace("1700000000", "1800000000");
    assert!(
        !node::unchanged(&pve, "pve1", 2097, &renewed, net0, &quiet).await?,
        "a new certificate"
    );
    let moved = net0.replace("192.0.2.97", "192.0.2.98");
    assert!(
        !node::unchanged(&pve, "pve1", 2097, description, &moved, &quiet).await?,
        "a new address"
    );
    assert!(pve.calls().is_empty(), "only read");
    Ok(())
}

#[tokio::test]
async fn a_container_made_before_the_tag_gets_it_and_keeps_its_others() -> Result<(), String> {
    let pve = FakePve {
        tags: "backup".to_owned(),
        ..FakePve::default()
    };
    let quiet = |_: &str| {};
    assert!(node::ensure_tag(&pve, "pve1", 2097, &quiet).await?);
    assert_eq!(
        pve.calls(),
        ["PUT /nodes/pve1/lxc/2097/config tags=backup;homestack-server&digest=d1"]
    );

    let tagged = FakePve {
        tags: "homestack-server".to_owned(),
        ..FakePve::default()
    };
    assert!(!node::ensure_tag(&tagged, "pve1", 2097, &quiet).await?);
    assert!(tagged.calls().is_empty(), "only read");
    Ok(())
}

#[tokio::test]
async fn network_diagnostics_include_the_mac_but_not_environment_or_description() -> Result<(), String> {
    let net0 = "name=eth0,bridge=vmbr0,hwaddr=02:00:00:00:00:01,ip=192.0.2.97/24,gw=192.0.2.1";
    let secret = "dummy-private-key-not-for-logging";
    let pve = FakePve {
        net0: net0.to_owned(),
        env: format!("HOMESTACK_TLS_KEY_B64={secret}"),
        description: secret.to_owned(),
        ..FakePve::default()
    };
    let lines = Mutex::new(Vec::new());
    let say = |line: &str| {
        if let Ok(mut lines) = lines.lock() {
            lines.push(line.to_owned());
        }
    };
    assert!(node::unchanged(&pve, "pve1", 2097, secret, net0, &say).await?);
    node::ensure_tag(&pve, "pve1", 2097, &say).await?;
    let log = lines.lock().map_err(|_| "log poisoned")?.join("\n");
    assert!(log.contains("CT 2097 on pve1: current net0="));
    assert!(log.contains("hwaddr=02:00:00:00:00:01"));
    assert!(log.contains("expected net0="));
    assert!(!log.contains(secret) && !log.contains("HOMESTACK_TLS_KEY_B64"));
    Ok(())
}
