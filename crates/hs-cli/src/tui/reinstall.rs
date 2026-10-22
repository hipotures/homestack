//! Reinstalling the server containers (the containers group, Enter): the
//! image each was made from and when, the images published since, and
//! `hs_deploy::deploy::apply` for each chosen container with the image
//! chosen; `deploy.toml` names that image afterwards. No drawing.

use std::path::{Path, PathBuf};

use hs_deploy::config::DeployConfig;
use serde_json::Value;
use tokio::sync::mpsc;

use super::action::{Decision, Kind, Progress, Start, Step, StepState};

/// Root SSH access used only to restore an existing connector credential.
pub struct SiteAccess {
    pub system: std::sync::Arc<dyn crate::system::System>,
    pub home: PathBuf,
    pub prefix: Vec<String>,
    pub sounds: crate::touch::Sounds,
}

/// How many of the last published images are offered.
const OFFERED: usize = 10;

/// An image and when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// `REGISTRY/PATH:TAG`.
    pub image: String,
    /// Local time, `YYYY-MM-DD HH:MM`, or `—`.
    pub made: String,
    /// When it was made (RFC 3339), to compare.
    pub created: Option<String>,
}

impl Image {
    /// The tag alone: `0.2.0`.
    #[must_use]
    pub fn tag(&self) -> &str {
        self.image.rsplit_once(':').map_or(self.image.as_str(), |(_, tag)| tag)
    }
}

/// A container to reinstall and the image it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub vmid: u32,
    pub name: String,
    /// `None`: not made by `homestack-deploy`, or no longer there.
    pub image: Option<Image>,
}

/// What the reinstall form shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Images {
    pub servers: Vec<Server>,
    /// Newest first.
    pub published: Vec<Image>,
    pub sites: Vec<super::pangolin_sites::images::Snapshot>,
    pub common: Option<(String, String)>,
}

impl Images {
    /// Newest published image for an installed API container.
    #[must_use]
    pub fn update_for(&self, server: &Server) -> Option<&Image> {
        let version = |image: &Image| {
            hs_core::registry::split(&image.image).and_then(|(_, _, tag)| hs_core::registry::version(tag))
        };
        self.published
            .iter()
            .filter(|image| self.newer(image, server))
            .max_by(|a, b| version(a).cmp(&version(b)).then_with(|| a.created.cmp(&b.created)))
    }

    /// Compare publication versions first; timestamps order builds of the same version.
    #[must_use]
    pub fn newer(&self, image: &Image, server: &Server) -> bool {
        let Some(current) = &server.image else { return false };
        let (Some((host, path, tag)), Some((current_host, current_path, current_tag))) = (
            hs_core::registry::split(&image.image),
            hs_core::registry::split(&current.image),
        ) else {
            return false;
        };
        if (host, path) != (current_host, current_path) {
            return false;
        }
        if let (Some(offered), Some(installed)) =
            (hs_core::registry::version(tag), hs_core::registry::version(current_tag))
            && offered != installed
        {
            return offered > installed;
        }
        match (&image.created, &current.created) {
            (Some(offered), Some(installed)) => offered > installed,
            _ => false,
        }
    }
}

/// `2026-10-16T01:42:01.53Z` as local time, `offset` seconds from UTC.
#[must_use]
pub fn local_time(created: &str, offset: i64) -> Option<String> {
    let format = time::format_description::well_known::Rfc3339;
    let at = time::OffsetDateTime::parse(created, &format).ok()? + time::Duration::seconds(offset);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    ))
}

/// The containers `vmids` (instances of `deploy_toml`), the image each was
/// made from, and the images published lately, times shifted by `offset`.
pub async fn read(deploy_toml: &Path, vmids: &[u32], offset: i64) -> Result<Images, String> {
    if let Some(images) = common_images(deploy_toml, vmids, offset).await? {
        return Ok(images);
    }
    if let Some(images) = super::pangolin_sites::images::read(deploy_toml, vmids, offset).await? {
        return Ok(images);
    }
    let config = DeployConfig::load(deploy_toml)?;
    let made = hs_deploy::deploy::images(&config).await?;
    let mut chosen = Vec::new();
    for vmid in vmids {
        let instance = config
            .instances
            .iter()
            .find(|instance| instance.ctid == *vmid)
            .ok_or_else(|| format!("CT {vmid} is not an instance of {}", deploy_toml.display()))?;
        let image = made
            .iter()
            .find(|(name, _)| *name == instance.name)
            .and_then(|(_, image)| image.clone());
        chosen.push((*vmid, instance.name.clone(), image));
    }
    let current: Vec<String> = chosen.iter().filter_map(|(_, _, image)| image.clone()).collect();
    let published: Vec<Image> = hs_core::registry::recent(&config.image, OFFERED, &current)
        .await?
        .into_iter()
        .map(|published| Image {
            made: published
                .created
                .as_deref()
                .and_then(|created| local_time(created, offset))
                .unwrap_or_else(|| "—".to_owned()),
            image: published.image,
            created: published.created,
        })
        .collect();
    let servers = chosen
        .into_iter()
        .map(|(vmid, name, image)| Server {
            vmid,
            name,
            image: image.map(|image| {
                published
                    .iter()
                    .find(|known| known.image == image)
                    .cloned()
                    .unwrap_or(Image {
                        image,
                        made: "—".to_owned(),
                        created: None,
                    })
            }),
        })
        .collect();
    Ok(Images {
        servers,
        published,
        sites: Vec::new(),
        common: None,
    })
}

/// Set `image` in `deploy.toml`, keeping the rest of the file as it is.
fn keep_image(path: &Path, image: &str) -> Result<(), String> {
    hs_deploy::config::save_image(path, image)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// Do `start` (`Kind::Reinstall`): show its plan, wait for the answer,
/// then reinstall the servers one at a time with the image; each line of
/// `hs-deploy` goes to `say`.
pub async fn work(
    deploy_toml: PathBuf,
    start: Start,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    say: &(dyn Fn(&str) + Send + Sync),
    site_access: Option<SiteAccess>,
) {
    let parameters = &start.parameters;
    if parameters.get("service").is_some() {
        common_work(&deploy_toml, &start, decisions, progress, say).await;
        return;
    }
    let image = parameters["image"].as_str().unwrap_or_default().to_owned();
    let servers = strings(&parameters["servers"]);
    let sites = parameters["sites"] == true;
    progress(Progress::Plan {
        lines: strings(&parameters["plan"]),
        question: Kind::Reinstall.question(false).to_owned(),
        blocked: None,
        secret: None,
    });
    match decisions.recv().await {
        Some(Decision::Apply(_)) => {}
        Some(_) => {
            return progress(Progress::Done {
                ok: true,
                lines: vec!["Cancelled. No changes were made.".to_owned()],
            });
        }
        None => return,
    }
    let failed = |lines: Vec<String>| progress(Progress::Done { ok: false, lines });
    if sites {
        let snapshots: Vec<super::pangolin_sites::images::Snapshot> =
            match serde_json::from_value(parameters["servers"].clone()) {
                Ok(snapshots) => snapshots,
                Err(_) => return failed(vec!["Invalid Site image replacement plan".to_owned()]),
            };
        let Some(access) = site_access else {
            return failed(vec!["Root SSH access is unavailable".to_owned()]);
        };
        let report = |line: &str| {
            say(line);
            progress(Progress::Steps(vec![Step {
                name: line.to_owned(),
                state: StepState::Running,
            }]));
        };
        report("Connecting to Proxmox and checking Site credentials");
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::block_in_place(|| {
            let node = super::pangolin_sites::images::root_target(&snapshots)?;
            let mut root = crate::root_ssh::SshRoot::open_asking(
                access.system,
                &access.home,
                &node,
                None,
                &access.sounds,
                access.prefix,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .map_err(|_| "Cannot open root SSH access to restore Site credentials".to_owned())?;
            handle.block_on(super::pangolin_sites::images::apply(
                &deploy_toml,
                &snapshots,
                &image,
                &mut root,
                &report,
            ))
        });
        return match result {
            Ok(lines) => progress(Progress::Done { ok: true, lines }),
            Err(error) => failed(vec![error]),
        };
    }
    let mut config = match DeployConfig::load(&deploy_toml) {
        Ok(config) => config,
        Err(error) => return failed(vec![error]),
    };
    config.image.clone_from(&image);
    let tag = image.rsplit_once(':').map_or(image.as_str(), |(_, tag)| tag).to_owned();
    // Every server from the start; the one under way shows what hs-deploy
    // does now.
    let steps = std::sync::Mutex::new(
        servers
            .iter()
            .map(|name| Step {
                name: format!("{name}: waiting"),
                state: StepState::Waiting,
            })
            .collect::<Vec<Step>>(),
    );
    let set = |index: usize, state: StepState, doing: &str| {
        if let Ok(mut steps) = steps.lock() {
            if let Some(step) = steps.get_mut(index) {
                step.state = state;
                step.name = format!("{}: {doing}", servers[index]);
            }
            progress(Progress::Steps(steps.clone()));
        }
    };
    for (index, name) in servers.iter().enumerate() {
        set(index, StepState::Running, &format!("reinstall from {tag}"));
        let said = |line: &str| {
            say(line);
            let doing = line.trim();
            // The instance's own first line names it again.
            if !doing.starts_with(&format!("{name}:")) {
                set(index, StepState::Running, doing);
            }
        };
        let outcome = hs_deploy::deploy::apply(&config, std::slice::from_ref(name), &said).await;
        match &outcome {
            Ok(()) => set(index, StepState::Done, &format!("reinstalled from {tag}")),
            Err(_) => set(index, StepState::Failed, "failed"),
        }
        if let Err(error) = outcome {
            let done: Vec<&String> = servers.iter().take_while(|server| *server != name).collect();
            let mut lines = vec![format!("{name}: {error}")];
            if !done.is_empty() {
                let done: Vec<&str> = done.iter().map(|server| server.as_str()).collect();
                lines.push(format!("Reinstalled before it: {}.", done.join(", ")));
            }
            lines.push("deploy.toml is unchanged.".to_owned());
            return failed(lines);
        }
    }
    if servers.len() > 1
        && let Err(error) = hs_deploy::deploy::check_selected_versions(&config, &servers, &|line| say(line)).await
    {
        return failed(vec![
            error,
            format!("Reinstalled before check: {}.", servers.join(", ")),
            "deploy.toml is unchanged.".to_owned(),
        ]);
    }
    let mut lines = vec![format!("Reinstalled from {tag}: {}.", servers.join(", "))];
    match keep_image(&deploy_toml, &image) {
        Ok(()) => lines.push(format!("{} names this image now.", deploy_toml.display())),
        Err(error) => lines.push(format!("deploy.toml could not be changed: {error}")),
    }
    progress(Progress::Done { ok: true, lines });
}

/// Read service identity from the profile registry before considering legacy
/// handlers. A CT tag/pool never selects an image repository.
async fn common_images(path: &Path, ids: &[u32], offset: i64) -> Result<Option<Images>, String> {
    use crate::services::{deploy, registry};
    let registry = registry::load(&path.with_file_name("services.toml"))?;
    let Some((record, _)) = ids.iter().find_map(|id| registry.instance(*id)) else {
        return Ok(None);
    };
    let source = common_source(record).await?;
    let config = DeployConfig::load(path)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let mut servers = Vec::new();
    for id in ids {
        let instance = record
            .instances
            .iter()
            .find(|i| i.ctid == *id)
            .ok_or("Select instances of one registered service at a time")?;
        let (found, current) = deploy::config_of(&pve, *id)
            .await?
            .ok_or("Registered CT is missing; use Recovery")?;
        if found.node != instance.node {
            return Err("Registered CT is on a different node".to_owned());
        }
        crate::services::lifecycle::verify(&source, record, instance, &current)?;
        let version = instance
            .release
            .as_ref()
            .map_or(record.image_version.as_str(), |r| r.version.as_str());
        servers.push(Server {
            vmid: *id,
            name: instance.hostname.clone(),
            image: Some(Image {
                image: format!("{}:{version}", record.image_repository),
                made: "—".to_owned(),
                created: None,
            }),
        });
    }
    let installed: Vec<String> = servers
        .iter()
        .filter_map(|s| s.image.as_ref().map(|i| i.image.clone()))
        .collect();
    let published: Vec<Image> = hs_core::registry::recent(&source.image("latest"), OFFERED, &installed)
        .await?
        .into_iter()
        .map(|r| Image {
            made: r
                .created
                .as_deref()
                .and_then(|t| local_time(t, offset))
                .unwrap_or_else(|| "—".to_owned()),
            image: r.image,
            created: r.created,
        })
        .collect();
    for server in &mut servers {
        if let Some(image) = &mut server.image
            && let Some(known) = published.iter().find(|p| p.image == image.image)
        {
            *image = known.clone();
        }
    }
    Ok(Some(Images {
        servers,
        published,
        sites: Vec::new(),
        common: Some((record.repository.clone(), record.service.clone())),
    }))
}

async fn common_source(
    record: &crate::services::registry::Deployment,
) -> Result<crate::services::deploy::Source, String> {
    let repository = record.repository.clone();
    let id = record.service.clone();
    tokio::task::spawn_blocking(move || {
        let found = crate::services::github::discover(&crate::system::RealSystem::default(), &repository, None)?;
        let (file, manifest) = found
            .services()
            .find(|(_, m)| m.id == id)
            .ok_or("Registered service has no valid manifest on the default branch")?;
        Ok(crate::services::deploy::Source {
            repository,
            branch: found.branch.clone(),
            commit: found.commit.clone(),
            path: file.path.clone(),
            blob: file.blob.clone(),
            manifest: manifest.clone(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn common_work(
    path: &Path,
    start: &Start,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    say: &(dyn Fn(&str) + Send + Sync),
) {
    use crate::services::{lifecycle, registry};
    let result = async {
        let (repository, service): (String, String) =
            serde_json::from_value(start.parameters["service"].clone()).map_err(|_| "Invalid service identity")?;
        let ids: Vec<u32> =
            serde_json::from_value(start.parameters["instances"].clone()).map_err(|_| "Invalid instance selection")?;
        let state_path = path.with_file_name("services.toml");
        let registry = registry::load(&state_path)?;
        let record = registry
            .deployment(&repository, &service)
            .ok_or("Service is no longer registered")?;
        let source = common_source(record).await?;
        let image = start.parameters["image"].as_str().ok_or("Image is missing")?;
        let version = image.rsplit_once(':').map_or("", |(_, v)| v);
        if source.image(version) != image {
            return Err("Selected image does not belong to this service".to_owned());
        }
        let config = DeployConfig::load(path)?;
        let pve = hs_deploy::deploy::deploy_client(&config)?;
        let choice = lifecycle::Choice {
            action: Some(lifecycle::Action::Update),
            ctids: ids,
            version: version.to_owned(),
            ..lifecycle::Choice::default()
        };
        let plan = lifecycle::plan(&pve, &config, &source, record, &choice).await;
        progress(Progress::Plan {
            lines: plan.lines.clone(),
            question: "Apply this service image update?".to_owned(),
            blocked: (!plan.problems.is_empty()).then(|| plan.problems.join("\n")),
            secret: None,
        });
        if !plan.problems.is_empty() {
            return Ok::<_, String>(None);
        }
        match decisions.recv().await {
            Some(Decision::Apply(_)) => {}
            _ => return Ok(Some(vec!["Cancelled. No changes were made.".to_owned()])),
        }
        let replacement = plan.replacement.as_ref().ok_or("API replacement plan is missing")?;
        let lines = crate::services::replacement::apply(&pve, &state_path, &source, replacement, say).await?;
        Ok(Some(lines))
    }
    .await;
    match result {
        Ok(Some(lines)) => progress(Progress::Done { ok: true, lines }),
        Ok(None) => {}
        Err(error) => progress(Progress::Done {
            ok: false,
            lines: vec![error],
        }),
    }
}
