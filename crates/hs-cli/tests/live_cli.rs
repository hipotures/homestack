//! L4, the gate of phase 13 (`docs/migration/phase-13.md`): the `homestack`
//! client against the three test instances (`hs-deploy`'s
//! `live-deploy.toml`, deployed for the gate and removed after), on VM 240
//! in pool `homestack-test` with the test Gold 248: the plan without
//! `--yes` (exit 3), `create`, `status`, `refresh` and `destroy` with
//! `--json --yes` (exit 0), and the desktop files in a temporary `HOME`.
//!
//! `scripts/check.sh --live` runs it with `HOMESTACK_LIVE_IMAGE`, after the
//! deployment gate. It refuses anything but test instances and VM 240, and
//! does nothing when VM 240 already exists.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_cli::app::{self, Context};
use hs_cli::system::{Console, RealSystem};
use hs_cli::view::Style;
use hs_deploy::config::DeployConfig;
use hs_deploy::{deploy, node};
use hs_pve::PveToken;
use serde_json::Value;

type Error = Box<dyn std::error::Error>;

const VMID: &str = "240";
const NAME: &str = "hst-cli13";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl Captured {
    fn take(&self) -> String {
        self.0
            .lock()
            .map(|mut out| std::mem::take(&mut *out))
            .unwrap_or_default()
    }
}

impl Console for Captured {
    fn out(&mut self, text: &str) {
        if let Ok(mut out) = self.0.lock() {
            out.push_str(text);
        }
    }

    fn err(&mut self, text: &str) {
        eprint!("{text}");
    }

    fn ask(&mut self, _question: &str) -> Option<String> {
        None
    }

    fn secret(&mut self, _prompt: &str) -> Option<String> {
        None
    }
}

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
    // The installed servers (homestack install writes deploy.toml) must keep
    // their containers, addresses and names: the gate stops before it starts.
    let installed = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".config/homestack/deploy.toml"))
        .filter(|path| path.exists());
    if let Some(path) = installed {
        let clashes = config.clashes(&DeployConfig::load(&path)?);
        if !clashes.is_empty() {
            return Err(format!(
                "the live gate's test instances share {} with the installed servers of {}; refusing",
                clashes.join(", "),
                path.display()
            )
            .into());
        }
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
    console: Captured,
}

impl Desk {
    async fn run(&self, argv: &[&str]) -> (i32, Value) {
        let mut context = Context {
            home: self.home.path().to_owned(),
            config_home: None,
            stdin_tty: false,
            style: Style {
                color: false,
                width: 120,
            },
            system: Arc::new(RealSystem::default()),
            console: Box::new(self.console.clone()),
            poll: Duration::from_secs(2),
            servers: std::sync::Arc::new(hs_cli::servers::RealServers),
        };
        let argv: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
        let code = app::run(&argv, &mut context).await;
        let out = self.console.take();
        let value = serde_json::from_str(&out).unwrap_or(Value::String(out));
        eprintln!("live cli: homestack {} -> {code}", argv.join(" "));
        (code, value)
    }

    async fn expect(&self, argv: &[&str], code: i32) -> Result<Value, Error> {
        let (got, out) = self.run(argv).await;
        if got != code {
            return Err(format!("homestack {}: exit {got}, expected {code}: {out}", argv.join(" ")).into());
        }
        Ok(out)
    }
}

#[tokio::test]
#[ignore = "live cluster, creates containers 997-999 and VM 240: run through scripts/check.sh --live"]
async fn the_client_runs_a_workspace_through_the_test_instances() -> Result<(), Error> {
    let config = load()?;
    let say = |line: &str| eprintln!("live cli: {line}");
    deploy::apply(&config, &[], &say).await?;
    let desk = Desk {
        home: desktop(&config)?,
        console: Captured::default(),
    };
    let outcome = gate(&desk).await;
    if outcome.is_err() {
        // Leave nothing behind: the VM is ours only if the gate made it.
        let (code, out) = desk.run(&["status", VMID, "--json"]).await;
        if code == 0 && out["name"] == NAME {
            let (code, out) = desk.run(&["destroy", VMID, "--json", "--yes"]).await;
            eprintln!("live cli: cleanup destroy -> {code}: {out}");
        }
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

async fn gate(desk: &Desk) -> Result<(), Error> {
    let absent = desk.expect(&["status", VMID, "--json"], 0).await?;
    if absent["status"] != "absent" {
        return Err(format!("VM {VMID} exists already; the gate touches only a VM it made: {absent}").into());
    }

    let plan = desk.expect(&["create", VMID, NAME, "--json"], 3).await?;
    if plan["confirmation_required"] != true || plan["plan"]["vmid"] != 240 {
        return Err(format!("unexpected create plan: {plan}").into());
    }
    let created = desk.expect(&["create", VMID, NAME, "--json", "--yes"], 0).await?;
    let conf = desk.home.path().join(".ssh/config.d/homestack/vm240-hst-cli13.conf");
    if created["ssh"]["root"] != "ssh root@hst-cli13" || created["ssh_config_path"] != conf.display().to_string() {
        return Err(format!("unexpected create result: {created}").into());
    }
    if !std::fs::read_to_string(&conf)?.contains("HostName 192.168.100.240") {
        return Err("the SSH config does not name the VM's address".into());
    }

    let status = desk.expect(&["status", NAME, "--json"], 0).await?;
    if status["vmid"] != 240 || status["status"] != "running" || status["ok"] != true {
        return Err(format!("unexpected status: {status}").into());
    }
    desk.expect(&["status", "--json"], 0).await?;
    desk.expect(&["transport", "--json"], 0).await?;

    let plan = desk.expect(&["refresh", NAME, "--json"], 3).await?;
    if plan["plan"]["mode"] == "recover" {
        return Err(format!("refresh plans a recovery: {plan}").into());
    }
    let refreshed = desk.expect(&["refresh", NAME, "--json", "--yes"], 0).await?;
    if refreshed["ok"] != true || refreshed["ssh"]["user"] != "ssh user@192.168.100.240" {
        return Err(format!("unexpected refresh result: {refreshed}").into());
    }

    let destroyed = desk.expect(&["destroy", VMID, "--json", "--yes"], 0).await?;
    if destroyed["status"] != "absent"
        || destroyed["ssh_config_removed"] != serde_json::json!([conf.display().to_string()])
    {
        return Err(format!("unexpected destroy result: {destroyed}").into());
    }
    if conf.exists() {
        return Err("the SSH config is left after destroy".into());
    }
    let absent = desk.expect(&["status", VMID, "--json"], 0).await?;
    if absent["status"] != "absent" {
        return Err(format!("VM {VMID} is left: {absent}").into());
    }
    Ok(())
}
