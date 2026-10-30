//! `homestack update`: every HomeStack service whose containers run an older
//! image than its newest release, reviewed and replaced as the services
//! screen does. Manifests and publication status come from GitHub; only
//! images already published in the registry are offered for deployment.
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use hs_core::pve::Pve;
use hs_core::registry::version;
use hs_deploy::config::DeployConfig;

use super::deploy::Source;
use super::{github, instance, update};

const REPOSITORY: &str = "hipotures/homestack";

/// How long `--auto` waits between publication checks.
const POLL: Duration = Duration::from_secs(60);

/// One service's reviewed update.
struct Pending<'a> {
    source: &'a Source,
    version: String,
    plan: update::Plan,
}

/// The updates found in one pass and the services whose newest release is
/// still on its way to the registry.
struct Review<'a> {
    pending: Vec<Pending<'a>>,
    building: Vec<String>,
}

fn tag(image: &str) -> &str {
    image.rsplit_once(':').map_or("", |(_, tag)| tag)
}

/// Review updating `only` (every service when empty) to their newest
/// releases, then apply the ones chosen: all with `yes`, otherwise as asked
/// on a terminal. With `auto` every ready update is applied and the command
/// keeps checking until no newer release is still being published; a service
/// whose newest release is building waits for it instead of taking an older
/// one. `say` reports progress.
pub async fn run(
    deploy_toml: &Path,
    only: &[String],
    yes: bool,
    auto: bool,
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
    if auto {
        return run_auto(&pve, &config, &sources, root, say).await;
    }
    let pending = review(&pve, &config, &sources, root, false, true, say).await?.pending;
    if pending.is_empty() {
        say("No published image updates to apply.");
        return Ok(());
    }
    show(&pending, say);
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
        if !apply(&pve, &pending[index], say).await {
            failed.push(pending[index].source.manifest.id.clone());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("Not updated: {}", failed.join(", ")))
    }
}

/// `--auto`: apply every ready update, then wait for releases still being
/// published until none is left. A release that cannot be applied is not
/// retried.
async fn run_auto(
    pve: &impl Pve,
    config: &DeployConfig,
    sources: &[Source],
    root: &Path,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), String> {
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut first = true;
    loop {
        let round = review(pve, config, sources, root, true, first, say).await?;
        first = false;
        let pending: Vec<Pending> = round
            .pending
            .into_iter()
            .filter(|p| !failed.iter().any(|(id, version)| *id == p.source.manifest.id && *version == p.version))
            .collect();
        show(&pending, say);
        for update in &pending {
            if !update.plan.problems.is_empty() || !apply(pve, update, say).await {
                failed.push((update.source.manifest.id.clone(), update.version.clone()));
            }
        }
        if round.building.is_empty() {
            break;
        }
        say(&format!(
            "\nWaiting for {} to be published; checking again in {} s…",
            round.building.join(", "),
            POLL.as_secs()
        ));
        tokio::time::sleep(POLL).await;
    }
    if failed.is_empty() {
        say("No image is being published; all updates applied.");
        Ok(())
    } else {
        let failed: Vec<String> = failed.iter().map(|(id, version)| format!("{id} {version}")).collect();
        Err(format!("Not updated: {}", failed.join(", ")))
    }
}

/// One pass over `sources`: the deployed services with a newer published
/// image, and those whose newest release is still being published. With
/// `wait`, a service whose newest release is building is not offered an older
/// one. `verbose` also reports services that are already current.
async fn review<'a>(
    pve: &impl Pve,
    config: &DeployConfig,
    sources: &'a [Source],
    root: &Path,
    wait: bool,
    verbose: bool,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<Review<'a>, String> {
    let instances = instance::of_repository(pve, REPOSITORY).await?;
    let mut pending = Vec::new();
    let mut building = Vec::new();
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
            Ok(Some(publication)) => {
                say(&format!("{id}: {}", publication.text));
                if publication.building {
                    building.push(id.clone());
                    if wait {
                        continue;
                    }
                }
            }
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
            if verbose {
                say(&format!(
                    "{id}: {} containers; no newer published image (latest: {newest})",
                    deployed.len()
                ));
            }
            continue;
        }
        let plan = update::plan(pve, config, source, &targets, &newest, "", root).await;
        pending.push(Pending {
            source,
            version: newest,
            plan,
        });
    }
    Ok(Review { pending, building })
}

fn show(pending: &[Pending], say: &(dyn Fn(&str) + Send + Sync)) {
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
}

/// Apply one reviewed update; false when it failed.
async fn apply(pve: &impl Pve, update: &Pending<'_>, say: &(dyn Fn(&str) + Send + Sync)) -> bool {
    let id = &update.source.manifest.id;
    say(&format!("\nUpdating {id} to {}…", update.version));
    match update::apply(pve, update.source, &update.plan, say).await {
        Ok(_) => {
            say(&format!("{id} updated to {}.", update.version));
            true
        }
        Err(error) => {
            say(&format!("{id}: {error}"));
            false
        }
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
