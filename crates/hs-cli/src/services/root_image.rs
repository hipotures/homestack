//! Replace only the root disk, preserving every persistent mount and device mapping.
use hs_core::pve::Pve;
use hs_deploy::node::{self, NewContainer};
use hs_deploy::recreate::HOLDER_ID;
use serde_json::Value;

pub const OLD: &str = "root-image-old ";
const NEW: &str = "root-image-new ";
const IMAGE_PATH: &str = "/.homestack-root-image";
const OLD_PATH: &str = "/.homestack-old-root";
const SLOT: &str = "mp255";

fn text<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg[key].as_str().unwrap_or_default()
}
fn volume(property: &str) -> &str {
    property.split(',').next().unwrap_or_default()
}
pub fn owner(description: &str) -> Option<u32> {
    description
        .lines()
        .next()?
        .strip_prefix("HomeStack root image for CT ")?
        .parse()
        .ok()
}
pub fn old_root(cfg: &Value) -> Result<Option<&str>, String> {
    let roots: Vec<_> = text(cfg, "description")
        .lines()
        .filter_map(|line| line.strip_prefix(OLD))
        .collect();
    match roots.as_slice() {
        [] => Ok(None),
        [root] if root.contains(':') && !root.starts_with('/') => Ok(Some(root)),
        _ => Err("Invalid root image replacement metadata".into()),
    }
}

/// Only the reserved transfer mount and the exact previous root may accompany a pending swap.
pub fn temporary_fields(cfg: &Value, ctid: u32) -> Result<Vec<String>, String> {
    let Some(old) = old_root(cfg)? else {
        return Ok(Vec::new());
    };
    if !volume(old).contains(&format!("-{ctid}-")) || volume(old) == volume(text(cfg, "mp0")) {
        return Err("Previous root ownership or root/home separation is unproven".into());
    }
    let current = volume(text(cfg, "rootfs"));
    let new: Vec<_> = text(cfg, "description")
        .lines()
        .filter_map(|line| line.strip_prefix(NEW))
        .collect();
    if current != volume(old) && (new.len() != 1 || new[0] != current) || current == volume(old) && !new.is_empty() {
        return Err("Current root does not match the recorded image transfer".into());
    }
    let mut fields = Vec::new();
    if let Some(property) = cfg[SLOT].as_str() {
        let vol = volume(property);
        let path = property.split(',').find_map(|part| part.strip_prefix("mp="));
        let valid = match path {
            Some(IMAGE_PATH) => volume(text(cfg, "rootfs")) == volume(old) && vol != volume(old),
            Some(OLD_PATH) => vol == volume(old) && volume(text(cfg, "rootfs")) != vol,
            _ => false,
        };
        if !valid || !vol.contains(&format!("-{ctid}-")) || vol == volume(text(cfg, "mp0")) {
            return Err("Unproven root image transfer mount".into());
        }
        fields.push(SLOT.into());
    }
    for (key, value) in cfg.as_object().ok_or("Invalid container configuration")? {
        if key.starts_with("unused")
            && volume(value.as_str().unwrap_or_default()) == volume(old)
            && volume(text(cfg, "rootfs")) != volume(old)
        {
            fields.push(key.clone());
        }
    }
    Ok(fields)
}

/// Persistent PVE mounts, including host bind mounts backed by NFS or local disks.
/// The reserved root-transfer slot is excluded only after its metadata is verified.
pub fn persistent_mounts(cfg: &Value, ctid: u32) -> Result<std::collections::BTreeMap<String, String>, String> {
    let temporary = temporary_fields(cfg, ctid)?;
    let mut mounts = std::collections::BTreeMap::new();
    for (key, value) in cfg.as_object().ok_or("Invalid container configuration")? {
        if !key.strip_prefix("mp").is_some_and(|n| n.parse::<u32>().is_ok()) || temporary.contains(key) {
            continue;
        }
        if key == SLOT {
            return Err(format!(
                "{SLOT} is reserved for root image transfer; move this mount to another slot before updating"
            ));
        }
        let property = value.as_str().ok_or_else(|| format!("{key}: unreadable mount"))?;
        let source = volume(property);
        if source.is_empty() || (!source.starts_with('/') && !source.contains(':')) {
            return Err(format!("{key}: invalid mount source"));
        }
        let path = super::manifest::mount_path(property)
            .ok_or_else(|| format!("{key}: mount path is missing or ambiguous"))?;
        if !path.starts_with('/') || path == "/" || matches!(path, IMAGE_PATH | OLD_PATH) {
            return Err(format!("{key}: mount path conflicts with root image replacement"));
        }
        if source == volume(text(cfg, "rootfs")) || old_root(cfg)?.is_some_and(|old| source == volume(old)) {
            return Err(format!("{key}: root and persistent mount separation is unproven"));
        }
        let mut options: Vec<_> = property.split(',').skip(1).collect();
        options.sort_unstable();
        mounts.insert(key.clone(), format!("{source},{}", options.join(",")));
    }
    Ok(mounts)
}

pub async fn config(pve: &impl Pve, node: &str, id: u32) -> Result<Value, String> {
    pve.get(&format!("/nodes/{node}/lxc/{id}/config"), &[])
        .await
        .map_err(|e| e.to_string())
}
async fn running(pve: &impl Pve, node: &str, id: u32) -> Result<bool, String> {
    let cfg = pve
        .get(&format!("/nodes/{node}/lxc/{id}/status/current"), &[])
        .await
        .map_err(|e| e.to_string())?;
    match cfg["status"].as_str() {
        Some("running") => Ok(true),
        Some("stopped") => Ok(false),
        _ => Err("Container power state unavailable".into()),
    }
}
async fn put(pve: &impl Pve, node: &str, id: u32, cfg: &Value, fields: &[(&str, &str)]) -> Result<(), String> {
    let mut fields = fields.to_vec();
    fields.push((
        "digest",
        cfg["digest"].as_str().ok_or("Configuration revision missing")?,
    ));
    pve.put(&format!("/nodes/{node}/lxc/{id}/config"), &fields)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn validate_stage(cfg: &Value, ctid: u32) -> Result<(), String> {
    if owner(text(cfg, "description")) != Some(ctid)
        || cfg["unprivileged"].as_u64() != Some(1)
        || cfg["onboot"].as_u64() != Some(0)
        || cfg.get("lock").is_some()
    {
        return Err("Root image staging ownership or stopped initialization is unproven".into());
    }
    for key in cfg.as_object().ok_or("Invalid staging configuration")?.keys() {
        if key.starts_with("dev")
            || key.starts_with("net")
            || key == "hookscript"
            || key.starts_with("unused")
            || (key.starts_with("mp") && key != "mp0")
        {
            return Err(format!("Unexpected root image staging configuration: {key}"));
        }
    }
    for key in ["rootfs", "mp0"] {
        if let Some(property) = cfg[key].as_str()
            && (!volume(property).contains(&format!("-{HOLDER_ID}-")) || property.starts_with('/'))
        {
            return Err("Root image staging volume ownership is unproven".into());
        }
    }
    if cfg["mp0"].as_str().is_some_and(|property| {
        !property
            .split(',')
            .any(|part| matches!(part, "mp=/.homestack-placeholder" | "mp=/.homestack-root-image"))
            || volume(property) == volume(text(cfg, "rootfs"))
    }) {
        return Err("Invalid root image staging mount".into());
    }
    Ok(())
}

/// New image is unpacked in a stopped staging CT. Transfer its root as a volume,
/// then switch the original CT's rootfs while keeping its identity, home and devices.
/// Original environment entries equal to the old image's `defaults` give way to the new image.
pub async fn replace(
    pve: &impl Pve,
    spec: &NewContainer<'_>,
    defaults: &[(String, String)],
    say: &(impl Fn(&str) + ?Sized),
) -> Result<(), String> {
    let node = spec.node;
    let id = spec.ctid;
    let mut original = config(pve, node, id).await?;
    let devices = hs_deploy::devices::read(&original)?;
    let mounts = persistent_mounts(&original, id)?;
    if text(&original, "description").trim_end() == spec.description.trim_end() && old_root(&original)?.is_none() {
        return Ok(());
    }
    let description = format!("HomeStack root image for CT {id}\n{}", spec.description);
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|e| e.to_string())?;
    let staged = resources
        .as_array()
        .ok_or("Cluster inventory unavailable")?
        .iter()
        .find(|item| item["vmid"].as_u64() == Some(u64::from(HOLDER_ID)));
    if staged.is_some_and(|item| item["node"] != node || item["pool"] != spec.pool || item["type"] != "lxc") {
        return Err("Root image staging CTID is already in use elsewhere".into());
    }
    let inventory = pve
        .get(&format!("/nodes/{node}/lxc"), &[])
        .await
        .map_err(|e| e.to_string())?;
    let stage_exists = inventory
        .as_array()
        .ok_or("Node container inventory unavailable")?
        .iter()
        .any(|item| item["vmid"].as_u64() == Some(u64::from(HOLDER_ID)));
    if !stage_exists {
        if let Some(old) = old_root(&original)? {
            if volume(text(&original, "rootfs")) != volume(old) && temporary_fields(&original, id)?.is_empty() {
                put(pve, node, id, &original, &[("description", &spec.description)]).await?;
                return Ok(());
            }
            return Err("Pending root image staging container is missing; inspect volumes before resuming".into());
        }
        let name = format!("hs-root-image-{id}");
        node::create(
            pve,
            &NewContainer {
                devices: Default::default(),
                node,
                ctid: HOLDER_ID,
                hostname: &name,
                ostemplate: spec.ostemplate,
                pool: spec.pool,
                storage: spec.storage,
                disk_mib: spec.disk_mib,
                memory_mib: 64,
                cores: 1,
                net0: None,
                onboot: false,
                nameserver: None,
                searchdomain: None,
                swap_mib: 0,
                description: description.clone(),
                tag: None,
                mp0: Some(format!("{}:0.125,mp=/.homestack-placeholder,backup=0", spec.storage)),
            },
            &|line| say(line),
        )
        .await?;
    }
    let mut staged = config(pve, node, HOLDER_ID).await?;
    validate_stage(&staged, id)?;
    // The manifest tree SHA binds its content; a docs-only repository commit
    // does not change the staged image or the reviewed service contract.
    let revision = |description: &str| {
        description
            .lines()
            .filter(|line| !line.starts_with("commit "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    if revision(text(&staged, "description")) != revision(&description) || running(pve, node, HOLDER_ID).await? {
        return Err("Root image staging revision changed; review again".into());
    }
    // Raw runtime identity cannot be rewritten by the deploy token. Refuse an
    // incompatible image before stopping or modifying the original container.
    let identity = |cfg: &Value| {
        let mut values = std::collections::BTreeMap::from([
            ("lxc.init.uid".to_owned(), "0".to_owned()),
            ("lxc.init.gid".to_owned(), "0".to_owned()),
            ("lxc.init.cwd".to_owned(), "/".to_owned()),
            ("lxc.init.groups".to_owned(), String::new()),
        ]);
        if let Some(entries) = cfg["lxc"].as_array() {
            for entry in entries {
                if let (Some(key), Some(value)) = (entry[0].as_str(), entry[1].as_str())
                    && values.contains_key(key)
                {
                    values.insert(key.to_owned(), value.to_owned());
                }
            }
        }
        values
    };
    if identity(&staged) != identity(&original) {
        return Err("Image changes raw LXC runtime identity; original root and devices were retained".into());
    }
    if staged["mp0"]
        .as_str()
        .is_some_and(|property| property.split(',').any(|part| part == "mp=/.homestack-placeholder"))
    {
        let image = volume(text(&staged, "rootfs")).to_owned();
        let placeholder = volume(text(&staged, "mp0")).to_owned();
        let image_mount = format!("{image},mp={IMAGE_PATH},backup=0");
        put(
            pve,
            node,
            HOLDER_ID,
            &staged,
            &[("rootfs", &placeholder), ("mp0", &image_mount)],
        )
        .await?;
        staged = config(pve, node, HOLDER_ID).await?;
        validate_stage(&staged, id)?;
    }
    if old_root(&original)?.is_none() {
        if staged.get("mp0").is_none() {
            return Err("Staging image volume is missing; original container unchanged".into());
        }
        let old = text(&original, "rootfs").to_owned();
        let pending = format!("{}\n{OLD}{old}", spec.description);
        put(pve, node, id, &original, &[("description", &pending), ("onboot", "0")]).await?;
        original = config(pve, node, id).await?;
    }
    let old = old_root(&original)?.ok_or("Previous root metadata missing")?.to_owned();
    temporary_fields(&original, id)?;
    if running(pve, node, id).await? {
        say(&format!("  stopping CT {id} gracefully for root image switch"));
        let upid = pve
            .post(
                &format!("/nodes/{node}/lxc/{id}/status/shutdown"),
                &[("timeout", "60"), ("forceStop", "0")],
            )
            .await
            .map_err(|e| e.to_string())?;
        super::update::wait_task(pve, &upid, "stop container for root image switch").await?;
    }
    if running(pve, node, id).await? {
        return Err("Container did not stop; root was not switched".into());
    }
    original = config(pve, node, id).await?;
    if staged["mp0"].as_str().is_some() {
        if original.get(SLOT).is_some() {
            return Err("Both staging and target carry a root image transfer volume; inspect before resuming".into());
        }
        let to = id.to_string();
        let upid = pve
            .post(
                &format!("/nodes/{node}/lxc/{HOLDER_ID}/move_volume"),
                &[
                    ("volume", "mp0"),
                    ("target-vmid", &to),
                    ("target-volume", SLOT),
                    ("digest", text(&staged, "digest")),
                    ("target-digest", text(&original, "digest")),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        super::update::wait_task(pve, &upid, "transfer new root image").await?;
        original = config(pve, node, id).await?;
    }
    if volume(text(&original, "rootfs")) == volume(&old) {
        temporary_fields(&original, id)?;
        let new = volume(original[SLOT].as_str().ok_or("Transferred root image missing")?).to_owned();
        let options = old.split_once(',').map(|(_, options)| options).unwrap_or("");
        let new_root = if options.is_empty() {
            format!("{new},size={}M", spec.disk_mib)
        } else {
            format!("{new},{options}")
        };
        let old_mount = format!("{},mp={OLD_PATH},backup=0", volume(&old));
        let environment = node::carried_env(
            &node::parse_env(text(&staged, "env")),
            &node::parse_env(text(&original, "env")),
            defaults,
        );
        let pending = format!("{}\n{NEW}{new}", text(&original, "description"));
        let mut fields = vec![
            ("description", pending.as_str()),
            ("rootfs", new_root.as_str()),
            (SLOT, old_mount.as_str()),
            ("env", environment.as_str()),
        ];
        let entrypoint = staged["entrypoint"].as_str().unwrap_or("/sbin/init");
        fields.push(("entrypoint", entrypoint));
        for key in ["ostype", "arch"] {
            if let Some(value) = staged[key].as_str() {
                fields.push((key, value));
            }
        }
        put(pve, node, id, &original, &fields).await?;
        original = config(pve, node, id).await?;
    }
    if hs_deploy::devices::read(&original)? != devices
        || persistent_mounts(&original, id)? != mounts
        || volume(text(&original, "rootfs")) == volume(&old)
    {
        return Err("Root image verification failed; previous root and staging retained".into());
    }
    temporary_fields(&original, id)?;
    if original.get(SLOT).is_some() {
        put(pve, node, id, &original, &[("delete", SLOT)]).await?;
        original = config(pve, node, id).await?;
    }
    // Delete only the identified old disposable root, never the persistent home.
    let unused = temporary_fields(&original, id)?;
    for key in unused.into_iter().filter(|key| key.starts_with("unused")) {
        put(pve, node, id, &original, &[("delete", &key)]).await?;
        original = config(pve, node, id).await?;
    }
    let staged = config(pve, node, HOLDER_ID).await?;
    validate_stage(&staged, id)?;
    if staged.get("mp0").is_some() {
        return Err("Staging still holds the transferred image; cleanup refused".into());
    }
    let upid = pve
        .delete(
            &format!("/nodes/{node}/lxc/{HOLDER_ID}"),
            &[("purge", "0"), ("destroy-unreferenced-disks", "0")],
        )
        .await
        .map_err(|e| e.to_string())?;
    super::update::wait_task(pve, &upid, "remove root image staging container").await?;
    put(pve, node, id, &original, &[("description", &spec.description)]).await?;
    say(&format!(
        "  CT {id}: root image replaced; all persistent mounts, device mappings and container identity retained"
    ));
    Ok(())
}

#[cfg(test)]
#[path = "root_image_safety_tests.rs"]
mod safety_tests;
