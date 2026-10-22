//! Image replacement for existing Site connectors, without creating or rotating Sites.

use super::*;
use crate::tui::reinstall::{Image, Images, Server, local_time};
use serde_json::Value;

/// Non-secret identity and configuration revision approved in the image review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    ctid: u32,
    node: String,
    site: String,
    revision: String,
}

fn summary_path(deployment: &Path) -> Result<PathBuf, String> {
    Ok(deployment
        .parent()
        .ok_or("Deployment directory is missing")?
        .join("integrations/pangolin-sites.toml"))
}

fn key_path(deployment: &Path, summary: &Summary, node: &str) -> Result<PathBuf, String> {
    let identity = digest(&format!("{}\0{}", summary.target, summary.organization));
    Ok(deployment
        .parent()
        .ok_or("Deployment directory is missing")?
        .join("secrets")
        .join(format!("pangolin-site-{identity}-{node}.key")))
}

fn revision(config: &Value) -> Result<String, String> {
    config["digest"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Proxmox returned no container configuration revision".to_owned())
}

fn image_of(config: &Value) -> Result<String, String> {
    let image = config["description"]
        .as_str()
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("image "))
        .ok_or("Connector image metadata is missing")?;
    if hs_core::registry::split(image).map(|(host, repo, _)| (host, repo))
        != hs_core::registry::split(IMAGE).map(|(host, repo, _)| (host, repo))
    {
        return Err("The connector image is from another repository".to_owned());
    }
    Ok(image.to_owned())
}

/// Establish ownership through the live pool, node, tags, description and local journal.
async fn inspect(
    pve: &impl Pve,
    deployment: &Path,
    config: &DeployConfig,
    summary: &Summary,
    ctid: u32,
    locked: Option<&Storage>,
) -> Result<(Snapshot, Value), String> {
    let saved = summary
        .deployed
        .iter()
        .find(|d| d.ctid == ctid)
        .ok_or("Container is not a saved Site connector")?;
    let ct = node::container(pve, ctid)
        .await?
        .ok_or_else(|| format!("CT {ctid} no longer exists"))?;
    let resources = pve
        .get("/cluster/resources", &[("type", "vm")])
        .await
        .map_err(|_| "Cannot verify connector pool")?;
    let owned_pool = resources.as_array().into_iter().flatten().any(|r| {
        r["vmid"].as_u64() == Some(u64::from(ctid))
            && r["type"] == "lxc"
            && r["pool"].as_str() == Some(config.pool.as_str())
    });
    if ct.node != saved.node || !owned_pool {
        return Err(format!("CT {ctid} changed node or service pool; nothing is changed"));
    }
    let current = pve
        .get(&format!("/nodes/{}/lxc/{ctid}/config", saved.node), &[])
        .await
        .map_err(|_| format!("CT {ctid}: cannot read configuration"))?;
    let text = current["description"].as_str().unwrap_or_default();
    if current["hostname"].as_str() != Some(hostname(&saved.node).as_str())
        || !current["tags"]
            .as_str()
            .unwrap_or_default()
            .split(';')
            .any(|t| t == TAG)
        || !text
            .lines()
            .any(|l| l == format!("organization {}", summary.organization))
        || !text.lines().any(|l| l == format!("site {}", saved.site))
        || !text.lines().any(|l| l == "made by homestack")
    {
        return Err(format!("CT {ctid}: Site connector ownership does not match"));
    }
    let path = key_path(deployment, summary, &saved.node)?;
    let owned;
    let storage = if let Some(locked) = locked {
        locked
    } else {
        owned = Storage::open(&path)?;
        &owned
    };
    let record: SiteRecord = storage.load()?.ok_or("The Site credential journal is missing")?;
    let credential = read_credential(&path)?;
    if record.target != summary.target
        || record.organization != summary.organization
        || record.node != saved.node
        || record.site != saved.site
        || record.ctid != Some(ctid)
        || record.site_id != Some(saved.site_id)
        || record.stage != SiteStage::Created
        || credential.key_id().ok() != Some(record.newt_id.as_str())
    {
        return Err(format!("CT {ctid}: Site credential ownership does not match"));
    }
    image_of(&current)?;
    validate_config(&current)?;
    Ok((
        Snapshot {
            ctid,
            node: saved.node.clone(),
            site: saved.site.clone(),
            revision: revision(&current)?,
        },
        current,
    ))
}

fn number(config: &Value, key: &str) -> Result<u32, String> {
    config[key]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| config[key].as_str().and_then(|s| s.parse().ok()))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("Invalid connector {key}"))
}

fn disk(config: &Value) -> Result<(&str, u32), String> {
    let rootfs = config["rootfs"].as_str().ok_or("Connector root disk is missing")?;
    let (storage, _) = rootfs.split_once(':').ok_or("Invalid connector root disk")?;
    let options: Vec<_> = rootfs.split(',').skip(1).collect();
    if options.len() != 1 {
        return Err("Connector root disk has additional options; review them before replacement".to_owned());
    }
    let gib = options[0]
        .strip_prefix("size=")
        .and_then(|s| s.strip_suffix('G'))
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .ok_or("Connector disk size must be a whole number of GiB")?;
    Ok((storage, gib))
}

/// Refuse configurations whose storage or devices this replacement cannot preserve.
fn validate_config(config: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "arch",
        "cores",
        "cpuunits",
        "cpulimit",
        "description",
        "digest",
        "features",
        "hostname",
        "memory",
        "nameserver",
        "net0",
        "onboot",
        "ostype",
        "rootfs",
        "searchdomain",
        "startup",
        "swap",
        "tags",
        "timezone",
        "unprivileged",
        "cmode",
        "console",
        "tty",
        "env",
        "entrypoint",
        "lxc",
    ];
    if config
        .as_object()
        .ok_or("Invalid connector configuration")?
        .keys()
        .any(|k| !ALLOWED.contains(&k.as_str()))
    {
        return Err(
            "Connector has additional configuration, disks or protection; review it before image replacement"
                .to_owned(),
        );
    }
    if config["lxc"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            !matches!(
                (item[0].as_str(), item[1].as_str()),
                (Some("lxc.init.cwd"), Some("/")) | (Some("lxc.signal.halt"), Some("SIGTERM"))
            )
        })
    }) {
        return Err("Connector has custom LXC configuration; review it before image replacement".to_owned());
    }
    if config["entrypoint"]
        .as_str()
        .is_some_and(|entry| entry != "runsvdir -P /etc/service")
    {
        return Err("Connector has a custom entrypoint; review it before image replacement".to_owned());
    }
    if config["env"].as_str().is_some_and(|env| {
        env.split('\0')
            .any(|entry| !entry.is_empty() && !entry.starts_with("PATH="))
    }) {
        return Err("Connector has custom environment settings; review them before image replacement".to_owned());
    }
    disk(config)?;
    number(config, "memory")?;
    number(config, "cores")?;
    if config["net0"].as_str().is_none_or(|net| !net.contains("hwaddr=")) {
        return Err("Connector MAC address is missing; cannot preserve its network identity".to_owned());
    }
    if config["unprivileged"].as_u64() != Some(1) {
        return Err("Image replacement requires an unprivileged connector".to_owned());
    }
    Ok(())
}

pub async fn read(deployment: &Path, targets: &[u32], offset: i64) -> Result<Option<Images>, String> {
    let Some(summary) = Summary::load(&summary_path(deployment)?) else {
        return Ok(None);
    };
    if !targets.iter().any(|id| summary.deployed.iter().any(|d| d.ctid == *id)) {
        return Ok(None);
    }
    let config = DeployConfig::load(deployment)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let mut servers = Vec::new();
    let mut snapshots = Vec::new();
    for &id in targets {
        let (snapshot, ct) = inspect(&pve, deployment, &config, &summary, id, None).await?;
        servers.push(Server {
            vmid: id,
            name: snapshot.site.clone(),
            image: Some(Image {
                image: image_of(&ct)?,
                made: "—".to_owned(),
                created: None,
            }),
        });
        snapshots.push(snapshot);
    }
    let current: Vec<_> = servers
        .iter()
        .filter_map(|s| s.image.as_ref().map(|i| i.image.clone()))
        .collect();
    let published: Vec<_> = hs_core::registry::recent(IMAGE, 10, &current)
        .await?
        .into_iter()
        .map(|p| Image {
            made: p
                .created
                .as_deref()
                .and_then(|s| local_time(s, offset))
                .unwrap_or_else(|| "—".to_owned()),
            image: p.image,
            created: p.created,
        })
        .collect();
    for server in &mut servers {
        if let Some(image) = &mut server.image
            && let Some(found) = published.iter().find(|p| p.image == image.image)
        {
            *image = found.clone();
        }
    }
    Ok(Some(Images {
        servers,
        published,
        sites: snapshots,
        common: None,
        resume: None,
    }))
}

/// Replace only reviewed, still-owned connectors and restore their own credentials.
pub async fn apply(
    deployment: &Path,
    snapshots: &[Snapshot],
    image: &str,
    root: &mut dyn Root,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<String>, String> {
    if snapshots.is_empty() {
        return Err("No Site connectors selected".to_owned());
    }
    if hs_core::registry::split(image).map(|(h, r, _)| (h, r))
        != hs_core::registry::split(IMAGE).map(|(h, r, _)| (h, r))
        || hs_core::registry::split(image)
            .and_then(|(_, _, t)| hs_core::registry::version(t))
            .is_none()
    {
        return Err("Choose a versioned Pangolin Site image".to_owned());
    }
    let summary_file = summary_path(deployment)?;
    let mut summary = Summary::load(&summary_file).ok_or("Site deployment settings are missing")?;
    if summary.template_storage.is_empty() || !valid_url(&summary.url) {
        return Err("Site deployment settings are incomplete".to_owned());
    }
    let config = DeployConfig::load(deployment)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let access = hs_install::probe(root).map_err(|e| e.to_string())?;
    // Every selected connector is checked before any container is stopped.
    for snapshot in snapshots {
        let (found, _) = inspect(&pve, deployment, &config, &summary, snapshot.ctid, None).await?;
        if &found != snapshot {
            return Err("Site connector changed since review; review again".to_owned());
        }
        let path = key_path(deployment, &summary, &snapshot.node)?;
        let credential = read_credential(&path)?;
        if installed(root, &access, &snapshot.node, snapshot.ctid)?
            != digest(&configuration(&summary.url, &credential)?)
        {
            return Err(format!(
                "CT {}: installed Site credential differs; nothing is changed",
                snapshot.ctid
            ));
        }
    }
    let mut lines = Vec::new();
    for snapshot in snapshots {
        let path = key_path(deployment, &summary, &snapshot.node)?;
        let storage_lock = Storage::open(&path)?;
        let credential = read_credential(&path)?;
        let content = configuration(&summary.url, &credential)?;
        let expected = digest(&content);
        say(&format!(
            "{}: CT {}: preparing image replacement",
            snapshot.site, snapshot.ctid
        ));
        let ostemplate = node::ensure_image(&pve, &snapshot.node, &summary.template_storage, image, &|line| {
            say(line)
        })
        .await?;
        let (found, original) =
            inspect(&pve, deployment, &config, &summary, snapshot.ctid, Some(&storage_lock)).await?;
        if &found != snapshot {
            return Err("Site connector changed since review; review again".to_owned());
        }
        let ct = node::container(&pve, snapshot.ctid)
            .await?
            .ok_or("Connector disappeared")?;
        if installed(root, &access, &snapshot.node, snapshot.ctid)? != expected {
            return Err(format!(
                "CT {}: installed credential changed before replacement",
                snapshot.ctid
            ));
        }
        if image_of(&original)? == image {
            lines.push(format!(
                "{}: CT {} already uses {image}; kept",
                snapshot.site, snapshot.ctid
            ));
            continue;
        }
        let (disk_storage, disk_gib) = disk(&original)?;
        let spec = NewContainer {
            node: &snapshot.node,
            ctid: snapshot.ctid,
            hostname: &snapshot.site,
            ostemplate: &ostemplate,
            pool: &config.pool,
            storage: disk_storage,
            disk_gib,
            memory_mib: number(&original, "memory")?,
            cores: number(&original, "cores")?,
            net0: Some(
                original["net0"]
                    .as_str()
                    .ok_or("Connector network is missing")?
                    .to_owned(),
            ),
            onboot: true,
            nameserver: original["nameserver"].as_str().map(str::to_owned),
            description: original["description"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(|l| {
                    if l.starts_with("image ") {
                        format!("image {image}")
                    } else {
                        l.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            tag: Some(TAG),
            mp0: None,
        };
        node::destroy(&pve, &ct, snapshot.ctid, &|line| say(line)).await?;
        node::create(&pve, &spec, &|line| say(line)).await?;
        let retained: Vec<(String, String)> = original
            .as_object()
            .ok_or("Invalid configuration")?
            .iter()
            .filter(|(k, _)| {
                ![
                    "arch",
                    "ostype",
                    "digest",
                    "rootfs",
                    "description",
                    "entrypoint",
                    "env",
                    "lxc",
                ]
                .contains(&k.as_str())
            })
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()),
                )
            })
            .collect();
        let form: Vec<(&str, &str)> = retained.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pve.put(&format!("/nodes/{}/lxc/{}/config", snapshot.node, snapshot.ctid), &form)
            .await
            .map_err(|_| {
                format!(
                    "CT {}: cannot restore container settings; credential remains saved locally",
                    snapshot.ctid
                )
            })?;
        node::power(&pve, &snapshot.node, snapshot.ctid, "start").await?;
        let mut ready = false;
        for _ in 0..30 {
            if installed(root, &access, &snapshot.node, snapshot.ctid).is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !ready {
            return Err(format!(
                "CT {}: container did not become ready; credential remains saved locally",
                snapshot.ctid
            ));
        }
        let command = on_node(
            &access,
            &snapshot.node,
            &format!("pct exec {} -- {TOOL}", snapshot.ctid),
        )?;
        let output = root
            .run_input(&command, &content)
            .map_err(|_| "Cannot restore Site credential over root SSH")?;
        if output.code != 0 || installed(root, &access, &snapshot.node, snapshot.ctid)? != expected {
            return Err(format!(
                "CT {}: credential installation failed; saved credential retained",
                snapshot.ctid
            ));
        }
        let command = on_node(
            &access,
            &snapshot.node,
            &format!("pct exec {} -- sv status /etc/service/site", snapshot.ctid),
        )?;
        let output = root.run(&command).map_err(|_| "Cannot inspect connector service")?;
        if output.code != 0 || !output.stdout.starts_with("run:") {
            return Err(format!("CT {}: connector service is not running", snapshot.ctid));
        }
        summary.image = image.to_owned();
        summary.at = stamp();
        summary.save(&summary_file)?;
        let line = format!(
            "{}: CT {} updated; Site credential verified, connector running",
            snapshot.site, snapshot.ctid
        );
        say(&line);
        lines.push(line);
    }
    Ok(lines)
}

/// Root on one reviewed node can reach the other nodes over the cluster's SSH.
pub fn root_target(snapshots: &[Snapshot]) -> Result<String, String> {
    let node = &snapshots.first().ok_or("No connectors selected")?.node;
    crate::root_ssh::target(node).map_err(|_| "Invalid connector node".to_owned())
}
