//! `homestack update-api`: the API containers' image update from the command
//! line, with the review and replacement the services screen uses.
use std::path::Path;

use hs_deploy::config::DeployConfig;

use super::{deploy, instance, update};

const REPOSITORY: &str = "hipotures/homestack";
const SERVICE: &str = "homestack-api";

/// Review updating every API container not yet on `version` (the newest
/// release when omitted); apply it with `yes`. `say` reports progress.
pub async fn run(
    deploy_toml: &Path,
    version: Option<&str>,
    yes: bool,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), String> {
    let config = DeployConfig::load(deploy_toml)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let source = crate::tui::reinstall::common_source(REPOSITORY, SERVICE).await?;
    let version = match version {
        Some(version) => version.to_owned(),
        None => {
            let cluster = deploy::survey(&config, &pve, &source).await?;
            let newest = cluster.releases.first().ok_or("No published API release")?;
            newest
                .image
                .rsplit_once(':')
                .map(|(_, tag)| tag.to_owned())
                .ok_or("Invalid published API release")?
        }
    };
    let instances = instance::of_service(&pve, REPOSITORY, SERVICE).await?;
    let targets: Vec<u32> = instances
        .iter()
        .filter(|instance| instance.image.rsplit_once(':').map(|(_, tag)| tag) != Some(version.as_str()))
        .map(|instance| instance.ctid)
        .collect();
    if targets.is_empty() {
        say(&format!(
            "All {} API containers already run {version}.",
            instances.len()
        ));
        return Ok(());
    }
    let root = deploy_toml.parent().ok_or("Deployment profile directory is missing")?;
    let plan = update::plan(&pve, &config, &source, &targets, &version, "", root, false).await;
    for line in &plan.lines {
        say(line);
    }
    if !plan.problems.is_empty() {
        return Err(plan.problems.join("\n"));
    }
    if !yes {
        say("Run again with -y to apply this update.");
        return Ok(());
    }
    update::apply(&pve, &source, &plan, say).await?;
    say(&format!("API containers updated to {version}."));
    Ok(())
}
