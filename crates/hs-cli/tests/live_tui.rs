//! L4, the gate of phase 13b (`docs/migration/phase-13b.md`): the TUI's
//! actions (`tui::action::work`, the code behind the box of the workspaces
//! panel) against the three test instances (`hs-deploy`'s
//! `live-deploy.toml`, deployed for the gate and removed after), on VM 240 in
//! pool `homestack-test`: the plan, Apply, the job's steps as the box shows
//! them, and the desktop files, for create, refresh and destroy; a Cancel
//! changes nothing.
//!
//! `scripts/check.sh --live` runs it with `HOMESTACK_LIVE_IMAGE`. It refuses
//! anything but test instances and VM 240, and does nothing when VM 240
//! already exists.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_cli::tui::action::{Decision, Desktop, Kind, Progress, Start, StepState, work};
use hs_deploy::config::DeployConfig;
use hs_deploy::{deploy, node};
use hs_pve::PveToken;
use serde_json::json;
use tokio::sync::mpsc;

type Error = Box<dyn std::error::Error>;

const VMID: u32 = 240;
const NAME: &str = "hst-tui13b";

fn load() -> Result<DeployConfig, Error> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hs-deploy/tests/data/live-deploy.toml");
    let mut config = DeployConfig::load(&path)?;
    config.image = std::env::var("HOMESTACK_LIVE_IMAGE").map_err(|_| "HOMESTACK_LIVE_IMAGE is not set")?;
    if config.server.pool != "homestack-test" {
        return Err("the live gate uses only test instances (pool homestack-test)".into());
    }
    if !PveToken::from_file(&config.server.pve_token_file)?
        .id()
        .ends_with("!test")
    {
        return Err("the instances' token is not the test token; refusing".into());
    }
    if config
        .instances
        .iter()
        .any(|instance| !(997..=999).contains(&instance.ctid))
    {
        return Err("the live gate uses CTIDs 997-999 only".into());
    }
    Ok(config)
}

/// A desktop `HOME` whose `client.toml` names the three instances.
fn desktop(config: &DeployConfig) -> Result<tempfile::TempDir, Error> {
    let home = tempfile::tempdir()?;
    let dir = home.path().join(".config/homestack");
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(home.path().join(".ssh"))?;
    std::fs::write(home.path().join(".ssh/known_hosts"), "")?;
    let token = dir.join("api-token");
    std::fs::write(&token, std::fs::read_to_string(&config.server.api_token_file)?.trim())?;
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600))?;
    let urls = config
        .instances
        .iter()
        .map(|instance| {
            config
                .url(instance)
                .map(|url| format!("{:?}", url.as_str().trim_end_matches('/')))
        })
        .collect::<Result<Vec<_>, String>>()?;
    std::fs::write(
        dir.join("client.toml"),
        format!(
            "version = 1\n\n[workspace_ssh]\nuser = \"user\"\nidentity_files = [\"~/.ssh/id_live_gate\"]\n\
identities_only = true\nlog_level = \"FATAL\"\n\n[server]\nurls = [{}]\n",
            urls.join(", ")
        ),
    )?;
    Ok(home)
}

struct Desk {
    home: tempfile::TempDir,
    client: hs_client::Client,
    desktop: Desktop,
}

impl Desk {
    fn new(home: tempfile::TempDir) -> Result<Self, Error> {
        let path = home.path().join(".config/homestack/client.toml");
        let loaded = hs_cli::config::load(&path)?;
        let client = hs_client::Client::new(loaded.client_config(home.path())?)?;
        let desktop = Desktop {
            home: home.path().to_owned(),
            loaded,
            poll: Duration::from_secs(2),
        };
        Ok(Self { home, client, desktop })
    }

    /// Run `start` answering `decisions` in order; every progress the box saw.
    async fn act(&self, start: Start, decisions: &[Decision]) -> Vec<Progress> {
        let (send, mut receive) = mpsc::unbounded_channel();
        for decision in decisions {
            let _ = send.send(decision.clone());
        }
        drop(send);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        work(
            &self.client,
            &self.desktop,
            start,
            &mut receive,
            &move |progress: Progress| {
                if let Progress::Steps(steps) = &progress
                    && let Some(step) = steps.last()
                {
                    eprintln!("live tui:   {:?} {}", step.state, step.name);
                }
                if let Ok(mut seen) = record.lock() {
                    seen.push(progress);
                }
            },
        )
        .await;
        seen.lock().map(|seen| seen.clone()).unwrap_or_default()
    }

    async fn exists(&self) -> Result<bool, Error> {
        match self.client.get(&format!("/v1/workspaces/{VMID}")).await {
            Ok(found) => Ok(found["status"] != "absent"),
            Err(hs_client::ClientError::Api { code, .. }) if code == "not_found" => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

fn start(kind: Kind, parameters: serde_json::Value) -> Start {
    Start {
        kind,
        vmid: (kind != Kind::New).then_some(VMID),
        parameters,
        key_file: None,
    }
}

/// The box's last word: ok and its lines, after a plan and at least one step.
fn finished(what: &str, progress: &[Progress]) -> Result<Vec<String>, Error> {
    if !matches!(progress.first(), Some(Progress::Plan { .. })) {
        return Err(format!("{what}: no plan first: {progress:?}").into());
    }
    let ran = progress.iter().any(|item| {
        matches!(item, Progress::Steps(steps) if !steps.is_empty()
            && steps.iter().all(|step| step.state == StepState::Done))
    });
    match progress.last() {
        Some(Progress::Done { ok: true, lines }) if ran => {
            eprintln!("live tui: {what}: {}", lines.first().cloned().unwrap_or_default());
            Ok(lines.clone())
        }
        other => Err(format!("{what}: {other:?}").into()),
    }
}

#[tokio::test]
#[ignore = "live cluster, creates containers 997-999 and VM 240: run through scripts/check.sh --live"]
async fn the_tui_runs_a_workspace_through_the_test_instances() -> Result<(), Error> {
    let config = load()?;
    let say = |line: &str| eprintln!("live tui: {line}");
    deploy::apply(&config, &[], &say).await?;
    let desk = Desk::new(desktop(&config)?)?;
    let made = Arc::new(Mutex::new(false));
    let outcome = gate(&desk, &made).await;
    if outcome.is_err() && made.lock().map(|made| *made).unwrap_or(false) {
        let progress = desk
            .act(start(Kind::Destroy, json!({"vmid": VMID})), &[Decision::Apply(None)])
            .await;
        eprintln!("live tui: cleanup destroy -> {:?}", progress.last());
    }
    let removed = deploy::remove(&config, &[], &say).await;
    let pve = deploy::deploy_client(&config)?;
    for instance in &config.instances {
        if node::container(&pve, instance.ctid).await?.is_some() {
            return Err(format!("CT {} is left", instance.ctid).into());
        }
    }
    removed?;
    outcome
}

async fn gate(desk: &Desk, made: &Arc<Mutex<bool>>) -> Result<(), Error> {
    if desk.exists().await? {
        return Err(format!("VM {VMID} exists already; the gate touches only a VM it made").into());
    }
    let create = || start(Kind::New, json!({"vmid": VMID, "name": NAME}));

    let cancelled = desk.act(create(), &[Decision::Cancel]).await;
    match cancelled.last() {
        Some(Progress::Done { ok: true, lines }) if lines == &["Cancelled. No changes were made."] => {}
        other => return Err(format!("cancel: {other:?}").into()),
    }
    if desk.exists().await? {
        return Err("a cancelled create made the VM".into());
    }

    let created = desk.act(create(), &[Decision::Apply(None)]).await;
    if let Ok(mut made) = made.lock() {
        *made = true;
    }
    let lines = finished("create", &created)?.join("\n");
    if !lines.contains(&format!("ssh {NAME}")) {
        return Err(format!("create result: {lines}").into());
    }
    let conf = desk
        .home
        .path()
        .join(format!(".ssh/config.d/homestack/vm{VMID}-{NAME}.conf"));
    if !std::fs::read_to_string(&conf)?.contains("HostName 192.168.100.240") {
        return Err("the SSH config does not name the VM's address".into());
    }

    let refreshed = desk
        .act(start(Kind::Refresh, json!({"vmid": VMID})), &[Decision::Apply(None)])
        .await;
    finished("refresh", &refreshed)?;

    let destroyed = desk
        .act(start(Kind::Destroy, json!({"vmid": VMID})), &[Decision::Apply(None)])
        .await;
    finished("destroy", &destroyed)?;
    if let Ok(mut made) = made.lock() {
        *made = false;
    }
    if conf.exists() {
        return Err("the SSH config is left after destroy".into());
    }
    if desk.exists().await? {
        return Err(format!("VM {VMID} is left").into());
    }
    Ok(())
}
