//! Replace a container under the same CTID. A container without mount-point
//! volumes is destroyed and made again. Its mount-point volumes wait in a
//! temporary holder container on the same node, together with the
//! configuration the new container keeps; the holder is removed once the new
//! container has them. Every run continues from what Proxmox shows, so an
//! interrupted replacement resumes where it stopped.

use hs_core::pve::Pve;
use serde_json::Value;

use crate::node::{self, NewContainer};

/// What replaces the container and what it keeps from the old one.
pub struct Replace<'a> {
    /// The new container; its description tells it from the old one.
    pub spec: NewContainer<'a>,
    /// Shut the old container down gracefully instead of stopping it.
    pub graceful: bool,
    /// Configuration keys the new container keeps; `env` is merged over the
    /// new image's own entries. Kept only when the container has volumes.
    pub keep: &'a [&'a str],
}

const HOLDER: &str = "HomeStack holder for CT";
const READY: &str = "holder ready";

#[must_use]
pub fn holder_name(ctid: u32) -> String {
    format!("hs-holder-{ctid}")
}

fn text<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg[key].as_str().unwrap_or_default()
}

/// The mount-point keys whose volumes must survive, sorted. Bind mounts,
/// devices and unused volumes are refused: nothing here can carry them.
pub fn mounts(cfg: &Value) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for (key, value) in cfg.as_object().ok_or("Invalid container configuration")? {
        if key.starts_with("unused") || key.starts_with("dev") {
            return Err(format!("{key}: unused volumes and devices are not supported yet"));
        }
        if key.strip_prefix("mp").is_some_and(|n| n.parse::<u32>().is_ok()) {
            let volume = value.as_str().unwrap_or_default().split(',').next().unwrap_or_default();
            if volume.starts_with('/') || !volume.contains(':') {
                return Err(format!("{key}: bind and network mounts are not supported yet"));
            }
            keys.push(key.clone());
        }
    }
    keys.sort();
    Ok(keys)
}

async fn config(pve: &impl Pve, node: &str, ctid: u32) -> Result<Value, String> {
    let cfg = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    if let Some(lock) = cfg.get("lock") {
        return Err(format!("CT {ctid} is locked ({lock}); wait for its task, then resume"));
    }
    Ok(cfg)
}

async fn running(pve: &impl Pve, node: &str, ctid: u32) -> Result<bool, String> {
    let current = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/status/current"), &[])
        .await
        .map_err(|e| format!("CT {ctid} state: {e}"))?;
    match current["status"].as_str() {
        Some("running") => Ok(true),
        Some("stopped") => Ok(false),
        _ => Err(format!("CT {ctid}: power state unavailable")),
    }
}

/// The holder of `ctid`: same node, made by this module; whether it is ready.
async fn holder(pve: &impl Pve, node: &str, ctid: u32) -> Result<Option<(u32, bool)>, String> {
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|e| format!("cluster resources: {e}"))?;
    let name = holder_name(ctid);
    let found: Vec<&Value> = resources
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "lxc" && item["name"] == name.as_str())
        .collect();
    let [item] = found.as_slice() else {
        return if found.is_empty() {
            Ok(None)
        } else {
            Err(format!("Several holders of CT {ctid}; inspect them"))
        };
    };
    let id = item["vmid"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("Holder CTID unavailable")?;
    if item["node"] != node {
        return Err(format!("Holder CT {id} of CT {ctid} is on another node; inspect it"));
    }
    let cfg = config(pve, node, id).await?;
    let mut lines = text(&cfg, "description").lines();
    if lines.next() != Some(format!("{HOLDER} {ctid}").as_str()) {
        return Err(format!("CT {id} is named {name} but is not its holder; inspect it"));
    }
    Ok(Some((id, lines.any(|line| line == READY))))
}

fn is_new(cfg: &Value, spec: &NewContainer<'_>) -> bool {
    text(cfg, "description").trim_end() == spec.description.trim_end()
        && spec
            .net0
            .as_ref()
            .is_none_or(|net0| node::same_net(text(cfg, "net0"), net0))
}

async fn put(pve: &impl Pve, node: &str, ctid: u32, cfg: &Value, fields: &[(String, String)]) -> Result<(), String> {
    let mut form: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    form.push(("digest", text(cfg, "digest")));
    pve.put(&format!("/nodes/{node}/lxc/{ctid}/config"), &form)
        .await
        .map_err(|e| format!("CT {ctid} configuration: {e}"))?;
    Ok(())
}

fn kept(cfg: &Value, keep: &[&str]) -> Vec<(String, String)> {
    keep.iter()
        .filter_map(|key| {
            let value = cfg.get(*key)?;
            Some((
                (*key).to_owned(),
                value.as_str().map_or_else(|| value.to_string(), str::to_owned),
            ))
        })
        .collect()
}

/// Make the holder (stopped, never started, no network) and copy the kept
/// configuration into it; `holder ready` marks the copy as complete.
async fn prepare_holder(
    pve: &impl Pve,
    r: &Replace<'_>,
    old: &Value,
    existing: Option<u32>,
    say: &impl Fn(&str),
) -> Result<(), String> {
    let (node, ctid) = (r.spec.node, r.spec.ctid);
    let id = if let Some(id) = existing {
        id
    } else {
        let id = pve
            .get("/cluster/nextid", &[])
            .await
            .map_err(|e| format!("free CTID: {e}"))?;
        let id = id
            .as_str()
            .and_then(|s| s.parse::<u32>().ok())
            .or_else(|| id.as_u64().and_then(|n| u32::try_from(n).ok()))
            .ok_or("Proxmox gave no free CTID")?;
        let name = holder_name(ctid);
        node::create(
            pve,
            &NewContainer {
                node,
                ctid: id,
                hostname: &name,
                ostemplate: r.spec.ostemplate,
                pool: r.spec.pool,
                storage: r.spec.storage,
                disk_gib: r.spec.disk_gib,
                memory_mib: 64,
                cores: 1,
                net0: None,
                onboot: false,
                nameserver: None,
                description: format!("{HOLDER} {ctid}"),
                tag: None,
                mp0: None,
            },
            say,
        )
        .await?;
        id
    };
    let cfg = config(pve, node, id).await?;
    let mut fields = kept(old, r.keep);
    fields.push(("description".to_owned(), format!("{HOLDER} {ctid}\n{READY}")));
    put(pve, node, id, &cfg, &fields).await?;
    say(&format!("  holder CT {id} keeps the configuration of CT {ctid}"));
    Ok(())
}

async fn stop(pve: &impl Pve, node: &str, ctid: u32, graceful: bool, say: &impl Fn(&str)) -> Result<(), String> {
    if !running(pve, node, ctid).await? {
        return Ok(());
    }
    say(&format!("  stopping CT {ctid}"));
    if graceful {
        let upid = pve
            .post(
                &format!("/nodes/{node}/lxc/{ctid}/status/shutdown"),
                &[("timeout", "60"), ("forceStop", "0")],
            )
            .await
            .map_err(|e| format!("shutdown CT {ctid}: {e}"))?;
        node::wait(pve, &upid, &format!("shutdown CT {ctid}")).await?;
        if running(pve, node, ctid).await? {
            return Err(format!("CT {ctid} did not shut down gracefully; no forced stop"));
        }
        Ok(())
    } else {
        node::power(pve, node, ctid, "stop").await
    }
}

async fn move_volume(
    pve: &impl Pve,
    node: &str,
    from: u32,
    to: u32,
    key: &str,
    say: &impl Fn(&str),
) -> Result<(), String> {
    let source = config(pve, node, from).await?;
    let target = config(pve, node, to).await?;
    if target.get(key).is_some() {
        return Err(format!("CT {to} already has {key}; inspect both containers"));
    }
    let to_text = to.to_string();
    let upid = pve
        .post(
            &format!("/nodes/{node}/lxc/{from}/move_volume"),
            &[
                ("volume", key),
                ("target-vmid", &to_text),
                ("target-volume", key),
                ("digest", text(&source, "digest")),
                ("target-digest", text(&target, "digest")),
            ],
        )
        .await
        .map_err(|e| format!("move {key} CT {from} → {to}: {e}"))?;
    node::wait(pve, &upid, &format!("move {key} CT {from} → {to}")).await?;
    let moved = config(pve, node, to).await?;
    if config(pve, node, from).await?.get(key).is_some() || moved.get(key).is_none() {
        return Err(format!("{key} did not move from CT {from} to CT {to}; inspect both"));
    }
    say(&format!("  {key} CT {from} → {to}: {}", text(&moved, key)));
    Ok(())
}

/// Delete a stopped container that holds no volume but its own root.
async fn remove_bare(pve: &impl Pve, node: &str, ctid: u32, say: &impl Fn(&str)) -> Result<(), String> {
    let cfg = config(pve, node, ctid).await?;
    if !mounts(&cfg)?.is_empty() || running(pve, node, ctid).await? {
        return Err(format!("CT {ctid} still holds volumes or runs; nothing is removed"));
    }
    let upid = pve
        .delete(
            &format!("/nodes/{node}/lxc/{ctid}"),
            &[("purge", "0"), ("destroy-unreferenced-disks", "0")],
        )
        .await
        .map_err(|e| format!("destroy CT {ctid}: {e}"))?;
    node::wait(pve, &upid, &format!("destroy CT {ctid}")).await?;
    say(&format!("  CT {ctid} destroyed"));
    Ok(())
}

/// Give the new container the holder's volumes and kept configuration, then
/// remove the holder.
async fn finish(pve: &impl Pve, r: &Replace<'_>, id: u32, say: &impl Fn(&str)) -> Result<(), String> {
    let (node, ctid) = (r.spec.node, r.spec.ctid);
    if running(pve, node, ctid).await? {
        return Err(format!(
            "CT {ctid} runs while holder CT {id} still exists; inspect them"
        ));
    }
    let held = config(pve, node, id).await?;
    for key in mounts(&held)? {
        move_volume(pve, node, id, ctid, &key, say).await?;
    }
    let new = config(pve, node, ctid).await?;
    let mut fields = kept(&held, r.keep);
    for (key, value) in &mut fields {
        if key == "env" {
            *value = node::merge_env(&node::parse_env(text(&new, "env")), &node::parse_env(value), &[]);
        }
    }
    if !fields.is_empty() {
        put(pve, node, ctid, &new, &fields).await?;
    }
    remove_bare(pve, node, id, say).await
}

/// Replace `r.spec.ctid` with `r.spec`, or continue an interrupted
/// replacement. The new container is left stopped for the caller to finish.
pub async fn replace(pve: &impl Pve, r: &Replace<'_>, say: &impl Fn(&str)) -> Result<(), String> {
    let (node, ctid) = (r.spec.node, r.spec.ctid);
    loop {
        let holder = holder(pve, node, ctid).await?;
        let Some(found) = node::container(pve, ctid).await? else {
            node::create(pve, &r.spec, say).await?;
            continue;
        };
        if found.node != node {
            return Err(format!("CT {ctid} is on {}, not {node}", found.node));
        }
        let cfg = config(pve, node, ctid).await?;
        if is_new(&cfg, &r.spec) {
            if let Some((id, _)) = holder {
                finish(pve, r, id, say).await?;
            }
            return Ok(());
        }
        let keys = mounts(&cfg)?;
        match holder {
            None if keys.is_empty() => {
                stop(pve, node, ctid, r.graceful, say).await?;
                remove_bare(pve, node, ctid, say).await?;
            }
            Some((id, false)) => prepare_holder(pve, r, &cfg, Some(id), say).await?,
            None => prepare_holder(pve, r, &cfg, None, say).await?,
            Some((id, true)) => {
                stop(pve, node, ctid, r.graceful, say).await?;
                for key in &keys {
                    move_volume(pve, node, ctid, id, key, say).await?;
                }
                remove_bare(pve, node, ctid, say).await?;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use hs_pve::PveError;
    use serde_json::json;

    use super::*;

    const TASK: &str = "UPID:pve3:00000001:00000001:00000001:task:1:operator@pve:";

    /// One node: containers by CTID with their configuration and power state.
    struct Node(Mutex<BTreeMap<u32, (Value, bool)>>);

    impl Pve for Node {
        async fn get(&self, path: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
            let cts = self.0.lock().unwrap();
            let id = |p: &str| p.split('/').nth(4).and_then(|s| s.parse::<u32>().ok()).unwrap();
            Ok(match path {
                "/cluster/resources" => json!(
                    cts.iter()
                        .map(|(id, (cfg, _))| json!({"type":"lxc", "vmid":id, "node":"pve3", "name":cfg["hostname"]}))
                        .collect::<Vec<_>>()
                ),
                "/cluster/nextid" => json!("130"),
                p if p.contains("/tasks/") && p.ends_with("/status") => json!({"status":"stopped", "exitstatus":"OK"}),
                p if p.contains("/tasks/") => json!([]),
                p if p.ends_with("/status/current") => {
                    json!({"status": if cts[&id(p)].1 { "running" } else { "stopped" }})
                }
                p if p.ends_with("/config") => cts[&id(p)].0.clone(),
                _ => panic!("Unexpected read: {path}"),
            })
        }

        async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
            let mut cts = self.0.lock().unwrap();
            let field = |key: &str| form.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
            if path == "/nodes/pve3/lxc" {
                assert!(field("mp0").is_none(), "a new container never gets a new home");
                let id: u32 = field("vmid").unwrap().parse().unwrap();
                assert!(!cts.contains_key(&id));
                let mut cfg = json!({
                    "hostname": field("hostname").unwrap(),
                    "description": field("description").unwrap(),
                    "rootfs": format!("local-lvm:vm-{id}-disk-0,size=4G"),
                    "env": "PATH=/bin\0IMAGE=new",
                    "digest": "d",
                });
                if let Some(net0) = field("net0") {
                    cfg["net0"] = json!(net0);
                }
                cts.insert(id, (cfg, false));
                return Ok(json!(TASK));
            }
            let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
            if path.ends_with("/status/shutdown") || path.ends_with("/status/stop") {
                cts.get_mut(&id).unwrap().1 = false;
            } else if path.ends_with("/move_volume") {
                let key = field("volume").unwrap();
                let to: u32 = field("target-vmid").unwrap().parse().unwrap();
                assert!(
                    !cts[&id].1 && !cts[&to].1,
                    "volumes move only between stopped containers"
                );
                let value = cts
                    .get_mut(&id)
                    .unwrap()
                    .0
                    .as_object_mut()
                    .unwrap()
                    .remove(key)
                    .unwrap();
                let renamed = value
                    .as_str()
                    .unwrap()
                    .replacen(&format!("vm-{id}-"), &format!("vm-{to}-"), 1);
                cts.get_mut(&to).unwrap().0[field("target-volume").unwrap()] = json!(renamed);
            } else {
                panic!("Unexpected request: {path}");
            }
            Ok(json!(TASK))
        }

        async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
            let mut cts = self.0.lock().unwrap();
            let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
            assert!(form.iter().any(|(k, _)| *k == "digest"));
            for (key, value) in form.iter().filter(|(k, _)| *k != "digest") {
                cts.get_mut(&id).unwrap().0[*key] = json!(value);
            }
            Ok(Value::Null)
        }

        async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
            let mut cts = self.0.lock().unwrap();
            let id: u32 = path.split('/').nth(4).unwrap().parse().unwrap();
            assert_eq!(query, [("purge", "0"), ("destroy-unreferenced-disks", "0")]);
            assert!(
                mounts(&cts[&id].0).unwrap().is_empty(),
                "CT {id} destroyed with its volumes"
            );
            cts.remove(&id);
            Ok(json!(TASK))
        }
    }

    fn spec(description: &str) -> NewContainer<'_> {
        NewContainer {
            node: "pve3",
            ctid: 120,
            hostname: "app",
            ostemplate: "local:vztmpl/app.tar",
            pool: "homestack-service",
            storage: "local-lvm",
            disk_gib: 4,
            memory_mib: 512,
            cores: 1,
            net0: Some("name=eth0,bridge=vmbr0,ip=192.0.2.10/24,hwaddr=02:00:00:00:00:10".to_owned()),
            onboot: false,
            nameserver: None,
            description: description.to_owned(),
            tag: None,
            mp0: None,
        }
    }

    const HOME: &str = "local-lvm:vm-120-disk-1,mp=/home/app,backup=1,size=8G";

    async fn finished(node: &Node) {
        let plan = Replace {
            spec: spec("new"),
            graceful: true,
            keep: &["env", "features"],
        };
        replace(node, &plan, &|_| {}).await.unwrap();
        let cts = node.0.lock().unwrap();
        assert_eq!(cts.keys().copied().collect::<Vec<_>>(), [120], "the holder is removed");
        let (cfg, running) = &cts[&120];
        assert!(!running);
        assert_eq!(cfg["description"], "new");
        assert_eq!(
            cfg["mp0"].as_str().unwrap().split_once(',').unwrap().1,
            HOME.split_once(',').unwrap().1
        );
        assert!(cfg["mp0"].as_str().unwrap().starts_with("local-lvm:vm-120-"));
        assert_eq!(cfg["env"], "IMAGE=new\0PATH=/bin\0SECRET=kept");
        assert_eq!(cfg["features"], "nesting=1");
    }

    #[tokio::test]
    async fn home_moves_through_the_holder_and_back() {
        let old = json!({"hostname":"app", "description":"old", "rootfs":"local-lvm:vm-120-disk-0,size=4G",
            "mp0":HOME, "env":"PATH=/bin\0SECRET=kept", "features":"nesting=1", "digest":"d",
            "net0":"name=eth0,bridge=vmbr0,ip=192.0.2.10/24,hwaddr=02:00:00:00:00:10"});
        finished(&Node(Mutex::new(BTreeMap::from([(120, (old, true))])))).await;
    }

    #[tokio::test]
    async fn interrupted_replacement_resumes_from_the_holder() {
        // The old container is gone; only the ready holder keeps the home.
        let holder = json!({"hostname":holder_name(120), "description":format!("{HOLDER} 120\n{READY}"),
            "rootfs":"local-lvm:vm-130-disk-0,size=4G", "mp0":"local-lvm:vm-130-disk-1,mp=/home/app,backup=1,size=8G",
            "env":"PATH=/bin\0SECRET=kept", "features":"nesting=1", "digest":"d"});
        finished(&Node(Mutex::new(BTreeMap::from([(130, (holder, false))])))).await;
    }

    #[test]
    fn bind_mounts_and_unused_volumes_are_refused() {
        assert!(mounts(&json!({"mp0":"/mnt/nfs,mp=/data"})).is_err());
        assert!(mounts(&json!({"unused0":"local-lvm:vm-120-disk-2"})).is_err());
        assert_eq!(
            mounts(&json!({"mp1":"local-lvm:vm-120-disk-2,mp=/b", "mp0":HOME})).unwrap(),
            ["mp0", "mp1"]
        );
    }
}
