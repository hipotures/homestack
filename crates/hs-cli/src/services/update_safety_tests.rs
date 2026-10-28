//! Retained authorization safeguard: stale service reviews cannot mutate PVE.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use hs_pve::PveError;
use serde_json::json;

use super::*;

const TASK: &str = "UPID:pve3:00000001:00000001:00000001:task:1:operator@pve:";
struct Fake {
    containers: Mutex<BTreeMap<u32, Value>>,
    writes: Mutex<Vec<String>>,
    rejected: Mutex<Option<&'static str>>,
}
impl Pve for Fake {
    async fn get(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        if path.ends_with("/config") && *self.rejected.lock().unwrap() == Some("pve") {
            return Err(PveError::Response("PRIVATE-PVE-SAFETY-DETAIL".to_owned()));
        }
        let cts = self.containers.lock().unwrap();
        let id = || path.split('/').nth(4).unwrap().parse::<u32>().unwrap();
        Ok(match path {
            "/cluster/resources" => json!(
                cts.keys()
                    .map(|id| json!({"vmid":id,"type":"lxc","node":"pve3","pool":"services"}))
                    .collect::<Vec<_>>()
            ),
            "/nodes/pve3/lxc" => json!(cts.keys().map(|id| json!({"vmid":id})).collect::<Vec<_>>()),
            p if p.contains("/tasks/") && p.ends_with("/status") => json!({"status":"stopped","exitstatus":"OK"}),
            p if p.contains("/tasks/") => json!([]),
            p if p.ends_with("/config") => cts[&id()].clone(),
            p if p.ends_with("/status/current") => {
                json!({"status":if *self.rejected.lock().unwrap() == Some("running") {"running"} else {"stopped"},"ha":{"managed":u8::from(*self.rejected.lock().unwrap() == Some("ha"))}})
            }
            p if p.ends_with("/firewall/options") => json!({}),
            p if p.ends_with("/content") => json!([{"volid":"local:vztmpl/hs-120-abcdefabcdef.tar"}]),
            p if p.ends_with("/pending") && *self.rejected.lock().unwrap() == Some("pending") => {
                json!([{"pending":"x"}])
            }
            p if p.ends_with("/snapshot") && *self.rejected.lock().unwrap() == Some("snapshot") => {
                json!([{"name":"saved"}])
            }
            p if p.ends_with("/replication") && *self.rejected.lock().unwrap() == Some("replication") => {
                json!([{"id":"120-0"}])
            }
            p if p.contains("/firewall/") && *self.rejected.lock().unwrap() == Some("firewall") => {
                json!([{"action":"ACCEPT"}])
            }
            _ => json!([]),
        })
    }
    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.writes.lock().unwrap().push(path.to_owned());
        let mut cts = self.containers.lock().unwrap();
        let field = |key| form.iter().find(|(k, _)| *k == key).unwrap().1;
        if path == "/nodes/pve3/lxc" {
            let id: u32 = field("vmid").parse().unwrap();
            let mut cfg = json!({"digest":"new"});
            for (key, value) in form {
                cfg[*key] = match *key {
                    "memory" | "cores" | "swap" | "unprivileged" | "onboot" => json!(value.parse::<u32>().unwrap()),
                    _ => json!(value),
                };
            }
            cfg["rootfs"] = json!(format!("local-lvm:vm-{id}-disk-0,size=128M"));
            cts.insert(id, cfg);
        } else if path.ends_with("/move_volume") {
            let from: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
            let to: u32 = field("target-vmid").parse().unwrap();
            let value = cts
                .get_mut(&from)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field("volume"))
                .unwrap();
            cts.get_mut(&to).unwrap()[field("volume")] = json!(
                value
                    .as_str()
                    .unwrap()
                    .replace(&format!("vm-{from}-"), &format!("vm-{to}-"))
            );
        } else {
            panic!("Unexpected mutation {path}");
        }
        Ok(json!(TASK))
    }
    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.writes.lock().unwrap().push(path.to_owned());
        let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
        let mut cts = self.containers.lock().unwrap();
        for (key, value) in form.iter().filter(|(k, _)| *k != "digest") {
            cts.get_mut(&id).unwrap()[*key] = json!(value);
        }
        Ok(Value::Null)
    }
    async fn delete(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        self.writes.lock().unwrap().push(path.to_owned());
        if path.contains("/storage/") {
            return Ok(json!(TASK));
        }
        let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
        let cfg = self.containers.lock().unwrap().remove(&id).unwrap();
        assert!(cfg.get("mp0").is_none(), "never delete persistent home");
        Ok(json!(TASK))
    }
}
fn source() -> Source {
    use super::super::manifest::*;
    Source {
        repository: "owner/app".into(),
        branch: "main".into(),
        commit: "c".into(),
        path: "app.yaml".into(),
        blob: "b".into(),
        manifest: Manifest {
            schema3: None,
            id: "app".into(),
            name: "App".into(),
            image_repository: "registry/app".into(),
            resources: Resources {
                cores: 1,
                memory_mib: 512,
                swap_mib: 0,
                rootfs_mib: 128,
            },
            ports: vec![],
            healthcheck: None,
            persistence: None,
            pangolin: None,
            deployment: Deployment {
                hostname: "app".into(),
                instances: 1,
                placement: Placement::Single,
                state_model: StateModel::Local,
            },
        },
    }
}
fn fake() -> Fake {
    Fake {
        containers: Mutex::new(BTreeMap::from([(
            120,
            json!({
                "hostname":"app","rootfs":"local-lvm:vm-120-disk-0,size=128M",
                "mp0":"local-lvm:vm-120-disk-1,mp=/home/app,backup=1,size=10M",
                "net0":"name=eth0,bridge=vmbr0,ip=192.0.2.10/24,hwaddr=02:00:00:00:00:10",
                "description":deploy::description(&source(),"registry/app:1.0.0","sha256:old"),
                "unprivileged":1,"memory":512,"cores":1,"onboot":0,"digest":"old",
            }),
        )])),
        writes: Mutex::default(),
        rejected: Mutex::default(),
    }
}
async fn review(fake: &Fake) -> Plan {
    let target = instance::read(fake, 120).await.unwrap();
    let reviewed = reviewed_state(fake, &target, "registry/app:2.0.0", "sha256:abcdefabcdef")
        .await
        .unwrap();
    Plan {
        selection_root: "unused".into(),
        image: "registry/app:2.0.0".into(),
        digest: "sha256:abcdefabcdef".into(),
        platform: "linux/amd64".into(),
        template_storage: "local".into(),
        backup_storage: String::new(),
        api_config: None,
        targets: vec![target],
        reviewed: BTreeMap::from([(120, reviewed)]),
        lines: vec![],
        problems: vec![],
    }
}
#[tokio::test]
async fn stale_review_refuses_every_mutation() {
    for (key, value) in [
        ("lxc", json!([["lxc.mount.entry", "/unsafe"]])),
        ("memory", json!(1024)),
        ("cores", json!(2)),
        ("onboot", json!(1)),
        (
            "description",
            json!(deploy::description(
                &Source {
                    repository: "other/app".into(),
                    ..source()
                },
                "registry/app:1.0.0",
                "sha256:old"
            )),
        ),
        ("rootfs", json!("local-lvm:vm-120-disk-2,size=128M")),
        ("mp0", json!("local-lvm:vm-120-disk-3,mp=/home/app,backup=1,size=10M")),
        ("features", json!("nesting=1")),
    ] {
        let fake = fake();
        let plan = review(&fake).await;
        fake.containers.lock().unwrap().get_mut(&120).unwrap()[key] = value;
        assert!(apply(&fake, &source(), &plan, &|_| {}).await.is_err(), "{key}");
        assert!(fake.writes.lock().unwrap().is_empty(), "{key}");
    }
    for state in ["pending", "snapshot", "replication", "firewall", "ha", "running", "pve"] {
        let fake = fake();
        let plan = review(&fake).await;
        *fake.rejected.lock().unwrap() = Some(state);
        assert!(apply(&fake, &source(), &plan, &|_| {}).await.is_err(), "{state}");
        assert!(fake.writes.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn unchanged_and_reviewed_holder_recovery_preserve_home() {
    let fake = fake();
    let plan = review(&fake).await;
    // PVE digest alone is not semantic configuration drift.
    fake.containers.lock().unwrap().get_mut(&120).unwrap()["digest"] = json!("volatile");
    apply(&fake, &source(), &plan, &|_| {}).await.unwrap();
    let fake = self::fake();
    let target = instance::read(&fake, 120).await.unwrap();
    let op = uuid::Uuid::new_v4();
    let mut held = fake.containers.lock().unwrap().remove(&120).unwrap();
    held["description"] = json!(format!(
        "HomeStack holder for CT 120\n{}\nreplacement-operation {op}\nholder ready",
        instance::note(&target)
    ));
    held["rootfs"] = json!("local-lvm:vm-999999-disk-0,size=128M");
    held["mp0"] = json!("local-lvm:vm-999999-disk-1,mp=/home/app,backup=1,size=10M");
    fake.containers.lock().unwrap().insert(recreate::HOLDER_ID, held);
    let plan = review(&fake).await;
    assert_eq!(plan.reviewed[&120].recovery_operation, Some(op.to_string()));
    apply(&fake, &source(), &plan, &|_| {}).await.unwrap();
    let cts = fake.containers.lock().unwrap();
    assert_eq!(cts.len(), 1);
    assert!(text(&cts[&120], "mp0").starts_with("local-lvm:vm-120-disk-1,"));
}

#[tokio::test]
async fn stale_schema3_review_is_rejected_before_resolving_or_writing_environment() {
    let fake = fake();
    let plan = review(&fake).await;
    fake.containers.lock().unwrap().get_mut(&120).unwrap()["memory"] = json!(1024);
    let mut source = source();
    source.manifest.schema3 = Some(super::super::schema3::Contract {
        inputs: vec![],
        always_numbered: false,
        configuration: None,
    });
    // This root has no saved environment: reaching its resolver would produce
    // a different error. The safety refusal must precede that entire path.
    let error = one(&fake, &source, &plan, &plan.targets[0], None, &|_| {})
        .await
        .unwrap_err();
    assert!(error.contains("review again"), "{error}");
    assert!(fake.writes.lock().unwrap().is_empty());
}
