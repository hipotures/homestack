//! `homestack update`: every HomeStack service whose containers run an older
//! image than its newest release, reviewed and replaced as the services
//! screen does. Manifests and publication status come from GitHub; only
//! images already published in the registry are offered for deployment.
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// One service's line in the status table.
struct Row {
    id: String,
    text: String,
}

/// One pass: the status of every deployed service, its reviewed updates and
/// the releases still on their way to the registry ("ai-usage 0.2.8 …").
struct Review<'a> {
    rows: Vec<Row>,
    pending: Vec<Pending<'a>>,
    building: Vec<String>,
    problems: Vec<String>,
}

/// ANSI colors, only on a terminal and without NO_COLOR.
#[derive(Clone, Copy)]
struct Paint(bool);

impl Paint {
    fn detect() -> Self {
        Self(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn code(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    fn green(self, text: &str) -> String {
        self.code("32", text)
    }
    fn yellow(self, text: &str) -> String {
        self.code("33", text)
    }
    fn red(self, text: &str) -> String {
        self.code("31", text)
    }
    fn cyan(self, text: &str) -> String {
        self.code("36", text)
    }
    fn dim(self, text: &str) -> String {
        self.code("2", text)
    }
    fn bold(self, text: &str) -> String {
        self.code("1", text)
    }
    /// A GitHub Actions link: on a terminal a short OSC 8 hyperlink
    /// ("actions run 123"), otherwise the full URL.
    fn link(self, url: &str) -> String {
        if !self.0 {
            return url.to_owned();
        }
        let label = match url.rsplit_once("/runs/") {
            Some((_, run)) => format!("actions run {run}"),
            None => "actions".to_owned(),
        };
        format!("\x1b]8;;{url}\x1b\\{}\x1b]8;;\x1b\\", self.code("2;4", &label))
    }
}

fn tag(image: &str) -> &str {
    image.rsplit_once(':').map_or("", |(_, tag)| tag)
}

fn containers(count: usize) -> String {
    format!("{count} container{}", if count == 1 { "" } else { "s" })
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
    let paint = Paint::detect();
    let config = DeployConfig::load(deploy_toml)?;
    let pve = hs_deploy::deploy::deploy_client(&config)?;
    let root = deploy_toml.parent().ok_or("Deployment profile directory is missing")?;
    say(&paint.dim(&format!("Reading the service manifests of {REPOSITORY}…")));
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
        return run_auto(&pve, &config, &sources, root, paint, say).await;
    }
    let review = review(&pve, &config, &sources, root, false, paint).await?;
    table(&review.rows, say);
    let pending = review.pending;
    if pending.is_empty() {
        say("\nNo published image updates to apply.");
        return Ok(());
    }
    show(&pending, paint, say);
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
        if !apply(&pve, &pending[index], paint, say).await {
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
/// retried. After the first table only changed rows are printed again.
async fn run_auto(
    pve: &impl Pve,
    config: &DeployConfig,
    sources: &[Source],
    root: &Path,
    paint: Paint,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), String> {
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut shown: Vec<Row> = Vec::new();
    let problems = loop {
        let round = review(pve, config, sources, root, true, paint).await?;
        let changed: Vec<&Row> = round
            .rows
            .iter()
            .filter(|row| !shown.iter().any(|old| old.id == row.id && old.text == row.text))
            .collect();
        if !changed.is_empty() {
            let width = round.rows.iter().map(|row| row.id.len()).max().unwrap_or(0);
            for row in changed {
                say(&line(row, width, paint));
            }
        }
        shown = round.rows;
        let pending: Vec<Pending> = round
            .pending
            .into_iter()
            .filter(|p| {
                !failed
                    .iter()
                    .any(|(id, version)| *id == p.source.manifest.id && *version == p.version)
            })
            .collect();
        show(&pending, paint, say);
        let mut applied = false;
        for update in &pending {
            if !update.plan.problems.is_empty() || !apply(pve, update, paint, say).await {
                failed.push((update.source.manifest.id.clone(), update.version.clone()));
            }
            applied = true;
        }
        if round.building.is_empty() {
            break round.problems;
        }
        if applied {
            // Updated rows are shown again in full on the next pass.
            shown.clear();
        }
        wait(&round.building, paint, say).await;
    };
    let mut failed: Vec<String> = failed
        .into_iter()
        .map(|(id, version)| format!("{id} {version}"))
        .collect();
    failed.extend(problems);
    if failed.is_empty() {
        say(&format!(
            "\n{}",
            paint.green("Done: no image is being published and every update is applied.")
        ));
        Ok(())
    } else {
        Err(format!("Updates incomplete: {}", failed.join(", ")))
    }
}

/// Wait one poll interval; a terminal shows a spinner and a countdown.
async fn wait(building: &[String], paint: Paint, say: &(dyn Fn(&str) + Send + Sync)) {
    let what = building.join(", ");
    if !paint.0 {
        say(&format!("Waiting for {what}; checking again in {} s", POLL.as_secs()));
        tokio::time::sleep(POLL).await;
        return;
    }
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let start = Instant::now();
    let mut stdout = std::io::stdout();
    say("");
    for frame in FRAMES.iter().cycle() {
        let elapsed = start.elapsed();
        if elapsed >= POLL {
            break;
        }
        let left = (POLL - elapsed).as_secs() + 1;
        let _ = write!(
            stdout,
            "\r\x1b[2K{} Waiting for {what} {}",
            paint.yellow(&frame.to_string()),
            paint.dim(&format!("· next check in {left} s"))
        );
        let _ = stdout.flush();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = write!(stdout, "\r\x1b[2K\x1b[1A");
    let _ = stdout.flush();
}

fn line(row: &Row, width: usize, paint: Paint) -> String {
    let pad = " ".repeat(width - row.id.len());
    let indent = " ".repeat(width + 4);
    format!(
        "  {}{pad}  {}",
        paint.bold(&row.id),
        row.text.replace('\n', &format!("\n{indent}"))
    )
}

fn table(rows: &[Row], say: &(dyn Fn(&str) + Send + Sync)) {
    let width = rows.iter().map(|row| row.id.len()).max().unwrap_or(0);
    let paint = Paint::detect();
    say("");
    for row in rows {
        say(&line(row, width, paint));
    }
}

/// One pass over `sources`: the deployed services with a newer published
/// image, and those whose newest release is still being published. With
/// `wait`, a service whose newest release is building is not offered an older
/// one.
async fn review<'a>(
    pve: &impl Pve,
    config: &DeployConfig,
    sources: &'a [Source],
    root: &Path,
    wait: bool,
    paint: Paint,
) -> Result<Review<'a>, String> {
    let instances = instance::of_repository(pve, REPOSITORY).await?;
    let mut rows = Vec::new();
    let mut pending = Vec::new();
    let mut building = Vec::new();
    let mut problems = Vec::new();
    for source in sources {
        let id = &source.manifest.id;
        let mut row = |text: String| rows.push(Row { id: id.clone(), text });
        let deployed: Vec<&instance::Instance> = instances.iter().filter(|i| &i.service == id).collect();
        if deployed.is_empty() {
            continue;
        }
        let newest = match hs_core::registry::recent(&source.image("latest"), 1, &[]).await {
            Ok(releases) => releases.first().map(|release| tag(&release.image).to_owned()),
            Err(error) => {
                problems.push(format!("{id}: releases unavailable: {error}"));
                row(paint.red(&format!("releases unavailable: {error}")));
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
        // The newest release on its way, or failed, is noted before the published one.
        let mut note = String::new();
        match publication {
            Ok(Some(publication)) => {
                let detail = if publication.detail.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", publication.detail)
                };
                let state = format!("{} {}{detail}", publication.release, publication.state);
                note = format!(
                    "{}\n{}",
                    if publication.building {
                        paint.yellow(&state)
                    } else {
                        paint.red(&state)
                    },
                    paint.link(&publication.url)
                );
                if !publication.building {
                    problems.push(format!("{id} {state}"));
                }
                if publication.building {
                    building.push(format!("{id} {}", publication.release));
                    if wait {
                        row(note);
                        continue;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                problems.push(format!("{id}: publication status unavailable: {error}"));
                note = paint.red(&format!("publication status unavailable: {error}"));
            }
        }
        let note = |text: String| {
            if note.is_empty() {
                text
            } else {
                format!("{text}\n{note}")
            }
        };
        let Some(newest) = newest else {
            problems.push(format!("{id}: no published release"));
            row(note(paint.red("no published release")));
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
            row(note(format!(
                "{} {}",
                paint.green(&format!("{newest} up to date")),
                paint.dim(&format!("· {}", containers(deployed.len())))
            )));
            continue;
        }
        row(note(paint.cyan(&format!(
            "→ {newest} for {} of {}",
            targets.len(),
            containers(deployed.len())
        ))));
        let plan = update::plan(pve, config, source, &targets, &newest, "", root).await;
        pending.push(Pending {
            source,
            version: newest,
            plan,
        });
    }
    Ok(Review {
        rows,
        pending,
        building,
        problems,
    })
}

fn show(pending: &[Pending], paint: Paint, say: &(dyn Fn(&str) + Send + Sync)) {
    for (number, update) in pending.iter().enumerate() {
        say(&format!(
            "\n{}",
            paint.bold(&format!(
                "{}. {} → {}",
                number + 1,
                update.source.manifest.id,
                update.version
            ))
        ));
        for line in &update.plan.lines {
            say(&format!("   {line}"));
        }
        for problem in &update.plan.problems {
            say(&format!("   {}", paint.red(&format!("Problem: {problem}"))));
        }
    }
}

/// Apply one reviewed update; false when it failed.
async fn apply(pve: &impl Pve, update: &Pending<'_>, paint: Paint, say: &(dyn Fn(&str) + Send + Sync)) -> bool {
    let id = &update.source.manifest.id;
    say(&format!(
        "\n{}",
        paint.bold(&format!("Updating {id} to {}…", update.version))
    ));
    match update::apply(pve, update.source, &update.plan, say).await {
        Ok(_) => {
            say(&paint.green(&format!("{id} updated to {}.", update.version)));
            true
        }
        Err(error) => {
            say(&paint.red(&format!("{id}: {error}")));
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
