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
    /// Whether `image` is newer than what `server` runs.
    #[must_use]
    pub fn newer(&self, image: &Image, server: &Server) -> bool {
        match (
            &image.created,
            server.image.as_ref().and_then(|now| now.created.as_ref()),
        ) {
            (Some(offered), Some(now)) => offered > now,
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
    let published: Vec<Image> = hs_deploy::registry::recent(&config.image, OFFERED, &current)
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
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |e: std::io::Error| format!("{}: {}", path.display(), e.kind());
    let text = std::fs::read_to_string(path).map_err(fail)?;
    let mut document: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    match document.get_mut("image").and_then(toml_edit::Item::as_value_mut) {
        // The comment after it stays.
        Some(value) => {
            let decor = value.decor().clone();
            *value = image.into();
            *value.decor_mut() = decor;
        }
        None => document["image"] = toml_edit::value(image),
    }
    let fresh = path.with_extension("toml.new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&fresh)
        .map_err(fail)?;
    file.write_all(document.to_string().as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    std::fs::rename(&fresh, path).map_err(fail)
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
    let mut steps: Vec<Step> = Vec::new();
    for name in &servers {
        steps.push(Step {
            name: format!("{name}: reinstall from {tag}"),
            state: StepState::Running,
        });
        progress(Progress::Steps(steps.clone()));
        let said = |line: &str| say(line);
        let outcome = hs_deploy::deploy::apply(&config, std::slice::from_ref(name), &said).await;
        let state = if outcome.is_ok() {
            StepState::Done
        } else {
            StepState::Failed
        };
        if let Some(step) = steps.last_mut() {
            step.state = state;
        }
        progress(Progress::Steps(steps.clone()));
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
