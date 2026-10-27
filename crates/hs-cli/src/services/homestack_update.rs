//! `homestack update`: every HomeStack service whose containers run an older
//! image than its newest release, reviewed and replaced as the services
//! screen does. Manifests and publication status come from GitHub; only
//! images already published in the registry are offered for deployment.
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use hs_core::registry::version;
use hs_deploy::config::DeployConfig;

use super::deploy::Source;
use super::{github, instance, update};

const REPOSITORY: &str = "hipotures/homestack";

/// One service's reviewed update.
struct Pending {
    source: Source,
    version: String,
    plan: update::Plan,
}

fn tag(image: &str) -> &str {
    image.rsplit_once(':').map_or("", |(_, tag)| tag)
}

/// Review updating `only` (every service when empty) to their newest
/// releases, then apply the ones chosen: all with `yes`, otherwise as asked
/// on a terminal. `say` reports progress.
pub async fn run(
    deploy_toml: &Path,
    only: &[String],
    yes: bool,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), String> {
    let config = DeployConfig::load(deploy_toml)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let root = deploy_toml.parent().ok_or("Deployment profile directory is missing")?;
    say(&format!("Reading the service manifests of {REPOSITORY}…"));
    let found =
        tokio::task::spawn_blocking(|| github::discover(&crate::system::RealSystem::default(), REPOSITORY, None))
            .await
            .map_err(|error| error.to_string())??;
    let sources: Vec<Source> = found
        .services()
        .filter(|(_, manifest)| only.is_empty() || only.contains(&manifest.id))
        .map(|(file, manifest)| Source::discovered(&found, file, manifest))
        .collect();
    if let Some(unknown) = only.iter().find(|id| !sources.iter().any(|s| &s.manifest.id == *id)) {
        let known: Vec<&str> = found.services().map(|(_, m)| m.id.as_str()).collect();
        return Err(format!("No service {unknown}; {REPOSITORY} has {}", known.join(", ")));
    }
    let instances = instance::of_repository(&pve, REPOSITORY).await?;
    let mut pending = Vec::new();
    for source in sources {
        let id = &source.manifest.id;
        let deployed: Vec<&instance::Instance> = instances.iter().filter(|i| &i.service == id).collect();
        if deployed.is_empty() {
            continue;
        }
        let newest = match hs_core::registry::recent(&source.image("latest"), 1, &[]).await {
            Ok(releases) => releases.first().map(|release| tag(&release.image).to_owned()),
            Err(error) => {
                say(&format!("{id}: releases unavailable: {error}"));
                continue;
            }
        };
        let service = id.clone();
        let published = newest.clone();
        let publication = tokio::task::spawn_blocking(move || {
            github::image_publication(
                &crate::system::RealSystem::default(),
                REPOSITORY,
                &service,
                published.as_deref(),
            )
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        match publication {
            Ok(Some(status)) => say(&format!("{id}: {status}")),
            Ok(None) => {}
            Err(error) => say(&format!("{id}: publication status unavailable: {error}")),
        }
        let Some(newest) = newest else {
            say(&format!("{id}: no published release"));
            continue;
        };
        // Only older images; a container on a newer build stays as it is.
        let targets: Vec<u32> = deployed
            .iter()
            .filter(|i| match (version(tag(&i.image)), version(&newest)) {
                (Some(now), Some(offered)) => now < offered,
                _ => tag(&i.image) != newest,
            })
            .map(|i| i.ctid)
            .collect();
        if targets.is_empty() {
            say(&format!(
                "{id}: {} containers; no newer published image (latest: {newest})",
                deployed.len()
            ));
            continue;
        }
        let plan = update::plan(&pve, &config, &source, &targets, &newest, "", root, false).await;
        pending.push(Pending {
            source,
            version: newest,
            plan,
        });
    }
    if pending.is_empty() {
        say("No published image updates to apply.");
        return Ok(());
    }
    for (number, update) in pending.iter().enumerate() {
        say(&format!(
            "\n{}. {} → {}",
            number + 1,
            update.source.manifest.id,
            update.version
        ));
        for line in &update.plan.lines {
            say(&format!("   {line}"));
        }
        for problem in &update.plan.problems {
            say(&format!("   Problem: {problem}"));
        }
    }
    let ready: Vec<usize> = (0..pending.len())
        .filter(|&index| pending[index].plan.problems.is_empty())
        .collect();
    if ready.is_empty() {
        return Err("No update can be applied; resolve the problems above".to_owned());
    }
    let chosen = if yes {
        ready
    } else if std::io::stdin().is_terminal() {
        choose(&ready)?
    } else {
        say("\nRun again with -y to apply these updates.");
        return Ok(());
    };
    let mut failed = Vec::new();
    for index in chosen {
        let update = &pending[index];
        let id = &update.source.manifest.id;
        say(&format!("\nUpdating {id} to {}…", update.version));
        match update::apply(&pve, &update.source, &update.plan, say).await {
            Ok(_) => say(&format!("{id} updated to {}.", update.version)),
            Err(error) => {
                say(&format!("{id}: {error}"));
                failed.push(id.clone());
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("Not updated: {}", failed.join(", ")))
    }
}

/// The updates the operator picks by number from `ready`; Enter takes all.
fn choose(ready: &[usize]) -> Result<Vec<usize>, String> {
    let numbers: Vec<String> = ready.iter().map(|index| (index + 1).to_string()).collect();
    loop {
        print!("\nUpdate which? [all] {} or none: ", numbers.join(" "));
        std::io::stdout().flush().map_err(|error| error.to_string())?;
        let mut answer = String::new();
        if std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Ok(Vec::new());
        }
        match answer.trim() {
            "" | "all" => return Ok(ready.to_vec()),
            "none" | "n" => return Ok(Vec::new()),
            answer => {
                let picked: Option<Vec<usize>> = answer
                    .split([' ', ','])
                    .filter(|word| !word.is_empty())
                    .map(|word| {
                        let index = word.parse::<usize>().ok()?.checked_sub(1)?;
                        ready.contains(&index).then_some(index)
                    })
                    .collect();
                match picked {
                    Some(mut picked) => {
                        picked.sort_unstable();
                        picked.dedup();
                        return Ok(picked);
                    }
                    None => println!("Enter numbers from the list, all or none."),
                }
            }
        }
    }
}
