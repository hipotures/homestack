//! The TUI's actions against an in-process server on the Proxmox simulator
//! (phase 13b, step 2): plan, the user's decision, the job's steps, the
//! desktop's part, as the CLI does them (`tests/cli.rs`).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_cli::tui::action::{Decision, Desktop, Kind, Progress, Start, StepState, work};
use hs_core::lifecycle::LifecycleTiming;
use hs_core::verify::VerifyTimeouts;
use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, AppOptions, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use serde_json::json;
use tokio::sync::mpsc;

type Error = Box<dyn std::error::Error>;

const LIFECYCLE: &str = include_str!("../../hs-core/tests/data/lifecycle-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const API_TOKEN: &str = "tui-test-api-token-0123456789abcdef01234";

const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
[root]
disk = "scsi0"
[home]
disk = "scsi1"
default_size = "20G"
[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]
[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "developer"
uid = 1000
gid = 1000
"#;

const SELECTED_KEYS: [(&str, &str); 4] = [
    (
        "blue",
        "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1 yubikey-blue",
    ),
    (
        "backup",
        "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBHRyZXpvcg== backup key",
    ),
    ("third", "ssh-ed25519 AAAATHIRD third-key"),
    ("fourth", "ssh-ed25519 AAAAFOURTH fourth-key"),
];

struct Desk {
    sim: PveSim,
    home: tempfile::TempDir,
    client: hs_client::Client,
    _state: tempfile::TempDir,
}

fn private_file(path: &Path, text: &str) -> Result<(), std::io::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

async fn desk() -> Result<Desk, Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(LIFECYCLE)?).await?;
    let state = tempfile::tempdir()?;
    let mut pve = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let timing = LifecycleTiming {
        poll: Duration::from_millis(5),
        verify: VerifyTimeouts {
            agent: Duration::from_secs(5),
            cloud_init: Duration::from_secs(5),
        },
    };
    let app = hs_server::app_with(
        ServerConfig {
            api_token: ApiToken::parse(API_TOKEN)?.into(),
            pve: pve.into(),
            config_file: state.path().join("server.toml"),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools::default(),
        },
        AppOptions {
            operations: hs_server::lifecycle::operations(timing),
            ..AppOptions::default()
        },
    )?;
    let certs = pve_sim::tls::generate()?;
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        certs.chain_pem.clone().into_bytes(),
        certs.key_pem.clone().into_bytes(),
    )
    .await?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    tokio::spawn(axum_server::from_tcp_rustls(listener, tls)?.serve(app.into_make_service()));
    let url = format!("https://localhost:{port}");
    let home = tempfile::tempdir()?;
    let dir = home.path().join(".config/homestack");
    private_file(&dir.join("api-token"), API_TOKEN)?;
    private_file(&dir.join("ca.pem"), &certs.ca_pem)?;
    private_file(
        &dir.join("client.toml"),
        &format!(
            "version = 1\n\n[workspace_ssh]\nuser = \"developer\"\nidentity_files = [\"~/.ssh/example-hardware-key\"]\n\
identities_only = true\nlog_level = \"FATAL\"\n\n[server]\nurls = [\"{url}\"]\n\
token_file = \"~/.config/homestack/api-token\"\nca_file = \"~/.config/homestack/ca.pem\"\n"
        ),
    )?;
    let mut config = hs_client::ClientConfig::new(vec![url.parse::<url::Url>()?.into()], API_TOKEN.to_owned());
    config.extra_ca_pem = Some(certs.ca_pem);
    let client = hs_client::Client::new(config)?;
    client.configure_on(0, SERVER_TOML).await?;
    Ok(Desk {
        sim,
        home,
        client,
        _state: state,
    })
}

impl Desk {
    fn desktop(&self) -> Desktop {
        let path = self.home.path().join(".config/homestack/client.toml");
        Desktop {
            home: self.home.path().to_owned(),
            loaded: hs_cli::config::load(&path).unwrap(),
            poll: Duration::from_millis(10),
        }
    }

    /// Run `start`, answering with `decisions` in order; every progress.
    async fn act(&self, start: Start, decisions: &[Decision]) -> Vec<Progress> {
        let (send, mut receive) = mpsc::unbounded_channel();
        for decision in decisions {
            send.send(decision.clone()).unwrap();
        }
        drop(send);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        work(&self.client, &self.desktop(), start, &mut receive, &move |progress| {
            record.lock().unwrap().push(progress);
        })
        .await;
        seen.lock().unwrap().clone()
    }

    fn changes(&self) -> Vec<String> {
        self.sim
            .ops()
            .iter()
            .map(ToString::to_string)
            .filter(|op| !op.starts_with("get{") && !op.starts_with("guest_ping{"))
            .collect()
    }
}

fn create(vmid: u32, name: &str) -> Start {
    Start {
        kind: Kind::New,
        vmid: None,
        parameters: json!({"vmid": vmid, "name": name}),
        key_files: Vec::new(),
    }
}

fn destroy(vmid: u32) -> Start {
    Start {
        kind: Kind::Destroy,
        vmid: Some(vmid),
        parameters: json!({"vmid": vmid}),
        key_files: Vec::new(),
    }
}

fn done(progress: &[Progress]) -> (bool, String) {
    match progress.last() {
        Some(Progress::Done { ok, lines }) => (*ok, lines.join("\n")),
        other => panic!("not done: {other:?}"),
    }
}

#[tokio::test]
async fn the_plan_comes_first_in_the_clis_words_and_cancel_changes_nothing() -> Result<(), Error> {
    let desk = desk().await?;
    let progress = desk.act(create(240, "hst-tui"), &[Decision::Cancel]).await;
    let Some(Progress::Plan { lines, question, .. }) = progress.first() else {
        panic!("{progress:?}");
    };
    let text = lines.join("\n");
    assert!(text.contains("CREATE PLAN"), "{text}");
    assert!(text.contains("hst-tui") && text.contains("192.0.2.240"), "{text}");
    assert!(
        !text.contains('│') && !text.contains('╭'),
        "no box inside the box: {text}"
    );
    assert_eq!(question, "Create this workspace?");
    let (ok, text) = done(&progress);
    assert!(ok);
    assert_eq!(text, "Cancelled. No changes were made.");
    assert!(desk.changes().is_empty(), "{:?}", desk.changes());
    Ok(())
}

#[tokio::test]
async fn closing_the_box_before_the_decision_changes_nothing() -> Result<(), Error> {
    let desk = desk().await?;
    let progress = desk.act(create(240, "hst-tui"), &[]).await;
    assert!(matches!(progress.first(), Some(Progress::Plan { .. })), "{progress:?}");
    assert!(desk.changes().is_empty(), "{:?}", desk.changes());
    Ok(())
}

#[tokio::test]
async fn create_runs_with_its_steps_and_writes_the_ssh_config_as_the_cli() -> Result<(), Error> {
    let desk = desk().await?;
    let progress = desk.act(create(240, "hst-tui"), &[Decision::Apply(None)]).await;
    let steps: Vec<_> = progress
        .iter()
        .filter_map(|item| match item {
            Progress::Steps(steps) => Some(steps.clone()),
            _ => None,
        })
        .collect();
    let last = steps.last().expect("steps were shown");
    assert!(!last.is_empty());
    assert!(last.iter().all(|step| step.state == StepState::Done), "{last:?}");
    let (ok, text) = done(&progress);
    assert!(ok, "{text}");
    assert!(text.contains("ssh hst-tui"), "{text}");
    let conf = desk.home.path().join(".ssh/config.d/homestack/vm240-hst-tui.conf");
    assert!(std::fs::read_to_string(&conf)?.contains("HostName 192.0.2.240"));

    let progress = desk.act(destroy(240), &[Decision::Apply(None)]).await;
    let Some(Progress::Plan { question, .. }) = progress.first() else {
        panic!("{progress:?}");
    };
    assert_eq!(
        question,
        "Permanently destroy this workspace INCLUDING its persistent home?"
    );
    let (ok, text) = done(&progress);
    assert!(ok, "{text}");
    assert!(!conf.exists(), "the SSH config goes with the VM");
    Ok(())
}

#[tokio::test]
async fn a_refused_plan_ends_with_the_servers_message() -> Result<(), Error> {
    let desk = desk().await?;
    let progress = desk.act(destroy(239), &[Decision::Apply(None)]).await;
    let (ok, text) = done(&progress);
    assert!(!ok);
    assert!(!text.is_empty(), "{progress:?}");
    assert!(desk.changes().is_empty(), "{:?}", desk.changes());
    Ok(())
}

#[tokio::test]
async fn four_selected_keys_use_one_confirmation_with_the_single_key_api() -> Result<(), Error> {
    let desk = desk().await?;
    let (ok, text) = done(&desk.act(create(240, "hst-keys"), &[Decision::Apply(None)]).await);
    assert!(ok, "{text}");
    let keys = SELECTED_KEYS;
    for (name, public) in keys {
        private_file(
            &desk.home.path().join(format!(".ssh/{name}")),
            "test private key placeholder",
        )?;
        private_file(&desk.home.path().join(format!(".ssh/{name}.pub")), public)?;
    }
    let start = Start {
        kind: Kind::Addkey,
        vmid: Some(240),
        parameters: json!({"vmid":240, "restart":false}),
        key_files: keys.iter().map(|(name, _)| format!("~/.ssh/{name}.pub")).collect(),
    };
    desk.sim.clear_ops();
    let cancelled = desk.act(start.clone(), &[Decision::Cancel]).await;
    assert!(done(&cancelled).0);
    assert!(desk.changes().is_empty(), "{:?}", desk.changes());
    let progress = desk.act(start, &[Decision::Apply(None)]).await;
    let (ok, text) = done(&progress);
    assert!(ok, "{text}");
    assert_eq!(
        progress
            .iter()
            .filter(|item| matches!(item, Progress::Plan { .. }))
            .count(),
        1
    );
    let Some(Progress::Plan { lines, .. }) = progress.first() else {
        panic!("{progress:?}")
    };
    let plan = lines.join("\n");
    assert!(plan.contains("yubikey-blue") && plan.contains("backup key"), "{plan}");
    let desktop = desk.desktop();
    let config = std::fs::read_to_string(desk.home.path().join(".ssh/config.d/homestack/vm240-hst-keys.conf"))?;
    for (name, _) in keys {
        assert!(
            desktop
                .loaded
                .client
                .workspace_ssh
                .identity_files
                .contains(&format!("~/.ssh/{name}"))
        );
        assert!(config.contains(&format!("/.ssh/{name}")), "{config}");
    }
    assert!(
        !desk
            .changes()
            .iter()
            .any(|op| op.contains("/status/reboot") || op.contains("/status/shutdown"))
    );
    let mut config = PveClientConfig::new(vec![desk.sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    config.extra_ca_pem = Some(desk.sim.ca_pem().to_owned());
    let pve = hs_pve::PveClient::new(config)?;
    for path in ["/root/.ssh/authorized_keys", "/home/developer/.ssh/authorized_keys"] {
        let text = hs_core::guest::read_text(&pve, "pve-example-1", 240, path)
            .await?
            .unwrap_or_default();
        for (_, key) in keys {
            assert_eq!(text.lines().filter(|line| *line == key).count(), 1, "{text}");
        }
    }

    Ok(())
}

#[tokio::test]
async fn missing_private_key_in_a_selection_fails_before_any_remote_change() -> Result<(), Error> {
    let desk = desk().await?;
    private_file(
        &desk.home.path().join(".ssh/only-public.pub"),
        "ssh-ed25519 AAAA public-only",
    )?;
    let progress = desk
        .act(
            Start {
                kind: Kind::Addkey,
                vmid: Some(240),
                parameters: json!({"vmid":240}),
                key_files: vec!["~/.ssh/only-public.pub".to_owned()],
            },
            &[Decision::Apply(None)],
        )
        .await;
    let (ok, text) = done(&progress);
    assert!(!ok && text.contains("private key file"), "{text}");
    assert!(!progress.iter().any(|item| matches!(item, Progress::Plan { .. })));
    assert!(desk.changes().is_empty());
    Ok(())
}

async fn prepared_selection(desk: &Desk) -> Result<Start, Error> {
    let (ok, text) = done(&desk.act(create(240, "hst-selection"), &[Decision::Apply(None)]).await);
    assert!(ok, "{text}");
    for (name, public) in SELECTED_KEYS {
        private_file(
            &desk.home.path().join(format!(".ssh/{name}")),
            "test identity placeholder",
        )?;
        private_file(&desk.home.path().join(format!(".ssh/{name}.pub")), public)?;
    }
    desk.sim.clear_ops();
    Ok(Start {
        kind: Kind::Addkey,
        vmid: Some(240),
        parameters: json!({"vmid":240,"restart":false}),
        key_files: SELECTED_KEYS
            .iter()
            .map(|(name, _)| format!("~/.ssh/{name}.pub"))
            .collect(),
    })
}

#[tokio::test]
async fn an_invalid_fourth_key_stops_the_selection_before_any_key_is_added() -> Result<(), Error> {
    let desk = desk().await?;
    let start = prepared_selection(&desk).await?;
    private_file(&desk.home.path().join(".ssh/fourth.pub"), "not a public key")?;
    let progress = desk.act(start, &[Decision::Apply(None)]).await;
    assert!(!done(&progress).0);
    assert!(!progress.iter().any(|item| matches!(item, Progress::Plan { .. })));
    assert!(desk.changes().is_empty(), "{:?}", desk.changes());
    Ok(())
}

#[tokio::test]
async fn failure_on_the_second_key_keeps_the_first_usable_and_stops_the_remaining_keys() -> Result<(), Error> {
    let desk = desk().await?;
    let start = prepared_selection(&desk).await?;
    desk.sim.inject_fault(pve_sim::scenario::FaultSpec {
        op: "post{path=/nodes/pve-example-1/qemu/240/config}".to_owned(),
        occurrence: 2,
        status: 500,
        message: "second key write failed".to_owned(),
        applied: false,
        hang: false,
        task: None,
    })?;
    let progress = desk.act(start, &[Decision::Apply(None)]).await;
    let (ok, text) = done(&progress);
    assert!(!ok, "{text}");
    assert!(text.contains("Completed keys: yubikey-blue"), "{text}");
    assert!(text.contains("Remaining keys were not attempted"), "{text}");
    let desktop = desk.desktop();
    assert!(
        desktop
            .loaded
            .client
            .workspace_ssh
            .identity_files
            .contains(&"~/.ssh/blue".to_owned())
    );
    assert!(
        !desktop
            .loaded
            .client
            .workspace_ssh
            .identity_files
            .contains(&"~/.ssh/third".to_owned())
    );
    let mut config = PveClientConfig::new(vec![desk.sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    config.extra_ca_pem = Some(desk.sim.ca_pem().to_owned());
    let pve = hs_pve::PveClient::new(config)?;
    let text = hs_core::guest::read_text(&pve, "pve-example-1", 240, "/home/developer/.ssh/authorized_keys")
        .await?
        .unwrap_or_default();
    assert!(text.contains(SELECTED_KEYS[0].1), "{text}");
    assert!(
        !text.contains(SELECTED_KEYS[2].1) && !text.contains(SELECTED_KEYS[3].1),
        "{text}"
    );
    Ok(())
}
