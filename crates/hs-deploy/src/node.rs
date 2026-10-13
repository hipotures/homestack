//! A server container on a node, through the Proxmox API with the deploy
//! token only (`decisions.md`, phase 11): the image pulled into the
//! template storage, the container made from it in pool
//! `homestack-service`, its environment, start, stop and removal.

use std::time::Duration;

use hs_core::pve::Pve;
use hs_core::tasks::{NoObserver, TaskWait, wait_task};
use serde_json::Value;

/// A task of the node takes at most this long (an image pull included).
const TASK_LIMIT: Duration = Duration::from_secs(900);

/// The file `oci-registry-pull` writes for `reference`: the last path
/// segment and the tag, `alpine_3.24.tar` for `docker.io/library/alpine:3.24`
/// (phase 11 probe).
pub fn image_file(reference: &str) -> Result<String, String> {
    let (path, tag) = reference
        .rsplit_once(':')
        .filter(|(path, tag)| !path.is_empty() && !tag.contains('/'))
        .ok_or_else(|| format!("image {reference:?} needs a tag"))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    Ok(format!("{name}_{tag}.tar"))
}

async fn wait(pve: &impl Pve, upid: &Value, what: &str) -> Result<(), String> {
    let upid = upid.as_str().ok_or_else(|| format!("{what}: Proxmox gave no task"))?;
    let result = wait_task(pve, upid, &TaskWait::up_to(TASK_LIMIT), &NoObserver)
        .await
        .map_err(|error| format!("{what}: {error}"))?;
    if result.ok() {
        Ok(())
    } else {
        Err(format!("{what}: {}", result.exitstatus))
    }
}

/// Pull `reference` into `storage` of `node` unless it is there; its
/// volume id.
pub async fn ensure_image(
    pve: &impl Pve,
    node: &str,
    storage: &str,
    reference: &str,
    say: &impl Fn(&str),
) -> Result<String, String> {
    let volid = format!("{storage}:vztmpl/{}", image_file(reference)?);
    let content = pve
        .get(
            &format!("/nodes/{node}/storage/{storage}/content"),
            &[("content", "vztmpl")],
        )
        .await
        .map_err(|e| format!("{node} {storage}: {e}"))?;
    let present = content
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["volid"].as_str() == Some(volid.as_str()));
    if present {
        say(&format!("  image {volid} is on {node}"));
        return Ok(volid);
    }
    say(&format!("  pulling {reference} into {storage} on {node}"));
    let upid = pve
        .post(
            &format!("/nodes/{node}/storage/{storage}/oci-registry-pull"),
            &[("reference", reference)],
        )
        .await
        .map_err(|e| format!("pull on {node}: {e}"))?;
    wait(pve, &upid, &format!("pull on {node}")).await?;
    Ok(volid)
}

/// Server images in `storage` of `node` other than `current` (a file name).
pub async fn old_images(pve: &impl Pve, node: &str, storage: &str, current: &str) -> Result<Vec<String>, String> {
    let content = pve
        .get(
            &format!("/nodes/{node}/storage/{storage}/content"),
            &[("content", "vztmpl")],
        )
        .await
        .map_err(|e| format!("{node} {storage}: {e}"))?;
    let prefix = format!("{storage}:vztmpl/homestack-server_");
    let current = format!("{storage}:vztmpl/{current}");
    Ok(content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["volid"].as_str())
        .filter(|volid| volid.starts_with(&prefix) && *volid != current)
        .map(str::to_owned)
        .collect())
}

/// A container the token sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub node: String,
    pub status: String,
    pub name: String,
}

/// Container `ctid`, if the token sees it (only those of its pool).
pub async fn container(pve: &impl Pve, ctid: u32) -> Result<Option<Container>, String> {
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|e| format!("cluster resources: {e}"))?;
    Ok(resources
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "lxc" && item["vmid"].as_u64() == Some(u64::from(ctid)))
        .map(|item| Container {
            node: item["node"].as_str().unwrap_or_default().to_owned(),
            status: item["status"].as_str().unwrap_or_default().to_owned(),
            name: item["name"].as_str().unwrap_or_default().to_owned(),
        }))
}

/// Stop (if running) and destroy container `ctid` with its volumes.
pub async fn destroy(pve: &impl Pve, found: &Container, ctid: u32, say: &impl Fn(&str)) -> Result<(), String> {
    let node = &found.node;
    if found.status == "running" {
        say(&format!("  stopping CT {ctid} on {node}"));
        let upid = pve
            .post(&format!("/nodes/{node}/lxc/{ctid}/status/stop"), &[])
            .await
            .map_err(|e| format!("stop CT {ctid}: {e}"))?;
        wait(pve, &upid, &format!("stop CT {ctid}")).await?;
    }
    say(&format!("  destroying CT {ctid} on {node}"));
    let upid = pve
        .delete(
            &format!("/nodes/{node}/lxc/{ctid}"),
            &[("purge", "1"), ("destroy-unreferenced-disks", "1")],
        )
        .await
        .map_err(|e| format!("destroy CT {ctid}: {e}"))?;
    wait(pve, &upid, &format!("destroy CT {ctid}")).await
}

/// What a new container gets.
pub struct NewContainer<'a> {
    pub node: &'a str,
    pub ctid: u32,
    pub hostname: &'a str,
    pub ostemplate: &'a str,
    pub pool: &'a str,
    pub storage: &'a str,
    pub disk_gib: u32,
    pub memory_mib: u32,
    pub cores: u32,
    pub net0: String,
    pub nameserver: Option<String>,
    pub description: String,
}

/// Create the container (stopped); the image sets its entrypoint and
/// environment.
pub async fn create(pve: &impl Pve, spec: &NewContainer<'_>, say: &impl Fn(&str)) -> Result<(), String> {
    say(&format!(
        "  creating CT {} ({}) on {}",
        spec.ctid, spec.hostname, spec.node
    ));
    let ctid = spec.ctid.to_string();
    let rootfs = format!("{}:{}", spec.storage, spec.disk_gib);
    let memory = spec.memory_mib.to_string();
    let cores = spec.cores.to_string();
    let mut form = vec![
        ("vmid", ctid.as_str()),
        ("ostemplate", spec.ostemplate),
        ("hostname", spec.hostname),
        ("pool", spec.pool),
        ("rootfs", rootfs.as_str()),
        ("memory", memory.as_str()),
        ("swap", "0"),
        ("cores", cores.as_str()),
        ("net0", spec.net0.as_str()),
        ("unprivileged", "1"),
        ("onboot", "1"),
        ("description", spec.description.as_str()),
    ];
    if let Some(nameserver) = &spec.nameserver {
        form.push(("nameserver", nameserver.as_str()));
    }
    let upid = pve
        .post(&format!("/nodes/{}/lxc", spec.node), &form)
        .await
        .map_err(|e| format!("create CT {}: {e}", spec.ctid))?;
    wait(pve, &upid, &format!("create CT {}", spec.ctid)).await
}

/// `env` of a container: `KEY=value` entries separated by NUL.
#[must_use]
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.split('\0')
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// `current` with `set` added or replaced and `remove` taken out, in order.
#[must_use]
pub fn merge_env(current: &[(String, String)], set: &[(String, String)], remove: &[&str]) -> String {
    let mut merged: Vec<(String, String)> = current
        .iter()
        .filter(|(key, _)| !remove.contains(&key.as_str()) && !set.iter().any(|(k, _)| k == key))
        .cloned()
        .collect();
    merged.extend(set.iter().filter(|(key, _)| !remove.contains(&key.as_str())).cloned());
    merged
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\0")
}

/// Change the container's environment: `set` added, `remove` taken out,
/// the image's own entries kept.
pub async fn set_env(
    pve: &impl Pve,
    node: &str,
    ctid: u32,
    set: &[(String, String)],
    remove: &[&str],
) -> Result<(), String> {
    let config = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    let current = parse_env(config["env"].as_str().unwrap_or_default());
    let env = merge_env(&current, set, remove);
    let digest = config["digest"].as_str().unwrap_or_default().to_owned();
    pve.put(
        &format!("/nodes/{node}/lxc/{ctid}/config"),
        &[("env", env.as_str()), ("digest", digest.as_str())],
    )
    .await
    .map_err(|e| format!("CT {ctid} env: {e}"))?;
    Ok(())
}

/// Whether a change of the environment waits for a restart.
pub async fn env_pending(pve: &impl Pve, node: &str, ctid: u32) -> Result<bool, String> {
    let pending = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/pending"), &[])
        .await
        .map_err(|e| format!("CT {ctid} pending: {e}"))?;
    Ok(pending
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["key"] == "env" && item.get("pending").is_some()))
}

/// The keys of the container's current environment (values stay unread).
pub async fn env_keys(pve: &impl Pve, node: &str, ctid: u32) -> Result<Vec<String>, String> {
    let config = pve
        .get(&format!("/nodes/{node}/lxc/{ctid}/config"), &[])
        .await
        .map_err(|e| format!("CT {ctid} config: {e}"))?;
    Ok(parse_env(config["env"].as_str().unwrap_or_default())
        .into_iter()
        .map(|(key, _)| key)
        .collect())
}

/// `POST status/{action}` (`start`, `stop`, `reboot`) and its task.
pub async fn power(pve: &impl Pve, node: &str, ctid: u32, action: &str) -> Result<(), String> {
    let upid = pve
        .post(&format!("/nodes/{node}/lxc/{ctid}/status/{action}"), &[])
        .await
        .map_err(|e| format!("{action} CT {ctid}: {e}"))?;
    wait(pve, &upid, &format!("{action} CT {ctid}")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_file_is_named_as_proxmox_names_it() -> Result<(), String> {
        assert_eq!(image_file("docker.io/library/alpine:3.24")?, "alpine_3.24.tar");
        assert_eq!(
            image_file("ghcr.io/hipotures/homestack-server:0.1.0-0123456789ab")?,
            "homestack-server_0.1.0-0123456789ab.tar"
        );
        assert!(image_file("ghcr.io/hipotures/homestack-server").is_err());
        assert!(image_file("registry:5000/image").is_err());
        Ok(())
    }

    #[test]
    fn the_environment_keeps_the_images_entries() {
        let current = parse_env("PATH=/usr/bin:/bin\0HOMESTACK_TLS_KEY_B64=k\0HOMESTACK_INSTANCE=old");
        let set = [("HOMESTACK_INSTANCE".to_owned(), "api1".to_owned())];
        assert_eq!(
            merge_env(&current, &set, &["HOMESTACK_TLS_KEY_B64"]),
            "PATH=/usr/bin:/bin\0HOMESTACK_INSTANCE=api1"
        );
        assert_eq!(parse_env("A=1=2"), vec![("A".to_owned(), "1=2".to_owned())]);
    }
}
