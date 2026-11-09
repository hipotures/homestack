//! Critical root/home/device preservation, including interruption after API mutations.
#![allow(clippy::unwrap_used)]
use super::*;
use hs_pve::PveError;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Mutex;
const TASK: &str = "UPID:pve2:00000001:00000001:00000001:task:1:operator@pve:";
const HOME: &str = "local-lvm:vm-120-disk-1,mp=/config,backup=1,size=10G";
const NFS: &str = "/mnt/nfs/tklivetracker,mp=/home/app/vbc1,backup=0,ro=1";
const LOCAL: &str = "/mnt/wdred/ztz,mp=/home/app/vbc2,backup=0";
const DISK: &str = "local-lvm:vm-120-disk-3,mp=/home/app/archive,size=20G,backup=1";
const DEVICE: &str = "/dev/serial/by-id/usb-test";
struct State {
    cts: BTreeMap<u32, Value>,
    running: bool,
    mutations: usize,
    interrupt: Option<usize>,
}
struct Fake(Mutex<State>);
impl Fake {
    fn after(state: &mut State) -> Result<Value, PveError> {
        state.mutations += 1;
        assert_eq!(state.cts[&120]["mp0"], HOME);
        assert_eq!(state.cts[&120]["dev0"], DEVICE);
        assert_eq!(state.cts[&120]["mp1"], NFS);
        assert_eq!(state.cts[&120]["mp2"], LOCAL);
        assert_eq!(state.cts[&120]["mp10"], DISK);
        if state.interrupt == Some(state.mutations) {
            state.interrupt = None;
            return Err(PveError::Response("simulated interrupted response".into()));
        }
        Ok(json!(TASK))
    }
}
impl Pve for Fake {
    async fn get(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        let state = self.0.lock().unwrap();
        if path == "/nodes/pve2/lxc" {
            return Ok(json!(state.cts.keys().map(|id| json!({"vmid":id})).collect::<Vec<_>>()));
        }
        if path == "/cluster/resources" {
            return Ok(json!(
                state
                    .cts
                    .keys()
                    .map(|id| json!({"vmid":id,"node":"pve2","pool":"services","type":"lxc"}))
                    .collect::<Vec<_>>()
            ));
        }
        if path.contains("/tasks/") {
            return Ok(json!({"status":"stopped","exitstatus":"OK"}));
        }
        let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
        if path.ends_with("/config") {
            return Ok(state.cts.get(&id).cloned().unwrap_or(Value::Null));
        }
        if path.ends_with("/status/current") {
            return Ok(json!({"status":if id == 120 && state.running {"running"} else {"stopped"}}));
        }
        panic!("Unexpected read {path}");
    }
    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        let mut state = self.0.lock().unwrap();
        let field = |key| form.iter().find(|(name, _)| *name == key).unwrap().1;
        if path == "/nodes/pve2/lxc" {
            let id: u32 = field("vmid").parse().unwrap();
            assert_eq!(id, HOLDER_ID);
            assert!(!state.cts.contains_key(&id));
            let mut cfg = json!({"digest":"d","env":"PATH=/bin\0IMAGE=new","entrypoint":"/init","ostype":"unmanaged","arch":"amd64"});
            for (key, value) in form {
                assert!(!key.starts_with("dev"));
                cfg[*key] = if ["onboot", "unprivileged"].contains(key) {
                    json!(value.parse::<u32>().unwrap())
                } else {
                    json!(value)
                };
            }
            cfg["rootfs"] = json!(format!("local-lvm:vm-{id}-disk-0,size=8G"));
            cfg["mp0"] = json!(format!("local-lvm:vm-{id}-disk-1,mp=/.homestack-placeholder,size=128M"));
            state.cts.insert(id, cfg);
        } else if path.ends_with("/status/shutdown") {
            assert!(path.contains("/120/"));
            state.running = false;
        } else if path.ends_with("/move_volume") {
            assert!(!state.running);
            let from: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
            let to: u32 = field("target-vmid").parse().unwrap();
            assert_eq!((from, to), (HOLDER_ID, 120));
            let value = state
                .cts
                .get_mut(&from)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field("volume"))
                .unwrap();
            let value = value
                .as_str()
                .unwrap()
                .replace(&format!("vm-{from}-disk-0"), &format!("vm-{to}-disk-2"));
            assert!(state.cts[&to].get(field("target-volume")).is_none());
            state.cts.get_mut(&to).unwrap()[field("target-volume")] = json!(value);
        } else {
            panic!("Unexpected mutation {path}");
        }
        Self::after(&mut state)
    }
    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        let mut state = self.0.lock().unwrap();
        let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
        assert!(form.iter().any(|(key, _)| *key == "digest"));
        let cfg = state.cts.get_mut(&id).unwrap();
        for (key, value) in form {
            assert!(!key.starts_with("dev"));
            if id == 120 {
                assert!(
                    !["mp0", "mp1", "mp2", "mp10"].contains(key),
                    "persistent mounts are never written"
                );
            }
            if *key == "digest" {
                continue;
            }
            if *key == "delete" {
                let removed = cfg.as_object_mut().unwrap().remove(*value).unwrap();
                if value.starts_with("mp") {
                    cfg["unused0"] = json!(volume(removed.as_str().unwrap()));
                } else {
                    assert_eq!(volume(removed.as_str().unwrap()), "local-lvm:vm-120-disk-0");
                }
            } else {
                cfg[*key] = json!(value);
            }
        }
        Self::after(&mut state)
    }
    async fn delete(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        let mut state = self.0.lock().unwrap();
        let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
        assert_eq!(id, HOLDER_ID, "the original CT is never destroyed");
        assert!(state.cts[&id].get("mp0").is_none());
        state.cts.remove(&id);
        Self::after(&mut state)
    }
}
fn spec() -> NewContainer<'static> {
    NewContainer { devices: Default::default(), node:"pve2", ctid:120, hostname:"ha", ostemplate:"local:vztmpl/image.tar", pool:"services", storage:"local-lvm", disk_mib:8192,memory_mib:2048,cores:2,net0:None,onboot:false,nameserver:None,searchdomain:None,swap_mib:0,description:"HomeStack service homeassistant\nimage registry/app:2.0.0\ndigest sha256:new\nreplacement-operation 00000000-0000-0000-0000-000000000001\nreplacement-running 1\nreplacement-onboot 1".into(),tag:None,mp0:None }
}
#[tokio::test]
async fn root_switch_and_every_interruption_preserve_home_devices_and_ctid() {
    for interruption in 0..=10 {
        let fake = Fake(Mutex::new(State {
            cts: BTreeMap::from([(
                120,
                json!({"digest":"d","description":"HomeStack service homeassistant\nimage registry/app:1.0.0", "rootfs":"local-lvm:vm-120-disk-0,size=8G", "mp0":HOME,"mp1":NFS,"mp2":LOCAL,"mp10":DISK,"dev0":DEVICE,"env":"TZ=Europe/Warsaw","onboot":1}),
            )]),
            running: true,
            mutations: 0,
            interrupt: (interruption > 0).then_some(interruption),
        }));
        let spec = spec();
        if replace(&fake, &spec, &[], 60, &|_| {}).await.is_err() {
            replace(&fake, &spec, &[], 60, &|_| {}).await.unwrap();
        }
        let state = fake.0.lock().unwrap();
        assert_eq!(state.cts.len(), 1);
        assert_eq!(state.cts[&120]["mp0"], HOME);
        assert_eq!(state.cts[&120]["dev0"], DEVICE);
        assert_eq!(state.cts[&120]["mp1"], NFS);
        assert_eq!(state.cts[&120]["mp2"], LOCAL);
        assert_eq!(state.cts[&120]["mp10"], DISK);
        assert_eq!(state.cts[&120]["description"], spec.description);
        assert_ne!(
            volume(state.cts[&120]["rootfs"].as_str().unwrap()),
            "local-lvm:vm-120-disk-0"
        );
        assert!(state.cts[&120].get("mp255").is_none());
        assert!(state.cts[&120].get("unused0").is_none());
    }
}

#[test]
fn root_transfer_metadata_cannot_name_the_persistent_home_or_an_unproven_root() {
    let mut cfg =
        json!({"description":"root-image-old local-lvm:vm-120-disk-1", "rootfs":"local-lvm:vm-120-disk-0", "mp0":HOME});
    assert!(temporary_fields(&cfg, 120).is_err());
    cfg["description"] = json!("root-image-old local-lvm:vm-120-disk-0");
    cfg["rootfs"] = json!("local-lvm:vm-120-disk-2");
    assert!(temporary_fields(&cfg, 120).is_err());
    cfg["description"] = json!("root-image-old local-lvm:vm-120-disk-0\nroot-image-new local-lvm:vm-120-disk-2");
    assert!(temporary_fields(&cfg, 120).is_ok());
    cfg["mp1"] = json!("local-lvm:vm-120-disk-0,mp=/archive");
    assert!(persistent_mounts(&cfg, 120).is_err());
    cfg["mp1"] = json!(NFS);
    assert!(persistent_mounts(&cfg, 120).is_ok());
    cfg["mp255"] = json!(LOCAL);
    assert!(persistent_mounts(&cfg, 120).is_err());
}
