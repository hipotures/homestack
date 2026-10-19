//! Reinstalling the server containers (the containers group, Enter): the
//! image each was made from and when, the images published since, and
//! `hs_deploy::deploy::apply` for each chosen container with the image
//! chosen; `deploy.toml` names that image afterwards. No drawing.

use std::path::{Path, PathBuf};

use hs_deploy::config::DeployConfig;
use serde_json::Value;
use tokio::sync::mpsc;

use super::action::{Decision, Kind, Progress, Start, Step, StepState};

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
    /// The tag alone: `0.1.0-5b8ded498d24`.
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
    Ok(Images { servers, published })
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
) {
    let parameters = &start.parameters;
    let image = parameters["image"].as_str().unwrap_or_default().to_owned();
    let servers = strings(&parameters["servers"]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_time_is_shown_as_local_minutes() {
        assert_eq!(
            local_time("2026-10-16T01:42:01.535937043Z", 7200).as_deref(),
            Some("2026-10-16 03:42")
        );
        assert_eq!(local_time("not a time", 0), None);
    }

    #[test]
    fn deploy_toml_keeps_everything_but_the_image() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("deploy.toml");
        std::fs::write(&path, "# the servers\nimage = \"r/p:old\" # kept\ndomain = \"x\"\n")?;
        keep_image(&path, "r/p:new")?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "# the servers\nimage = \"r/p:new\" # kept\ndomain = \"x\"\n"
        );
        Ok(())
    }
}
