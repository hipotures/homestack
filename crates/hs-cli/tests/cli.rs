//! The `homestack` command against in-process servers on the Proxmox
//! simulator, behind HTTPS (decision 8). Ports of `python/tests/test_cli.py`,
//! `test_resize_cli.py`, the local steps of `test_lifecycle.py` and
//! `test_workspace_ssh.py`, and `test_addkey.py`'s local failure.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{FakeSystem, Recorded, TestResult, strings};
use hs_cli::app::{self, Context};
use hs_cli::args::{self, Command};
use hs_cli::system::Output;
use hs_cli::view::Style;
use hs_core::lifecycle::LifecycleTiming;
use hs_core::verify::VerifyTimeouts;
use hs_pve::{PveClientConfig, PveToken};
use hs_server::{ApiToken, AppOptions, ServerConfig};
use pve_sim::{ClusterSpec, PveSim};
use serde_json::Value;

type Error = Box<dyn std::error::Error>;

const LIFECYCLE: &str = include_str!("../../hs-core/tests/data/lifecycle-cluster.toml");
const TRANSFER: &str = include_str!("../../hs-core/tests/data/create-transfer-cluster.toml");
const RESIZE: &str = include_str!("../../hs-core/tests/data/resize-cluster.toml");
const ADDKEY: &str = include_str!("../../hs-core/tests/data/addkey-cluster.toml");
const MIGRATE: &str = include_str!("../../hs-core/tests/data/migrate-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const API_TOKEN: &str = "cli-test-api-token-0123456789abcdef0123";
const TREZOR: &str = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBHRyZXpvcg== trezor-vm";

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

/// A simulated cluster, one server instance behind TLS, and a desktop
/// `HOME` whose `client.toml` points at it.
struct Desk {
    sim: PveSim,
    home: tempfile::TempDir,
    _state: tempfile::TempDir,
}

fn private_file(path: &Path, text: &str) -> Result<(), std::io::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

async fn desk(cluster: &str, server_toml: &str) -> Result<Desk, Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(cluster)?).await?;
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
    let server = axum_server::from_tcp_rustls(listener, tls)?.serve(app.into_make_service());
    tokio::spawn(server);
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
    hs_client::Client::new(config)?.configure_on(0, server_toml).await?;
    Ok(Desk {
        sim,
        home,
        _state: state,
    })
}

/// `ssh-keygen -F` finds every host; every other command exits 0.
fn known_hosts_system() -> FakeSystem {
    FakeSystem::new().run(|argv| {
        Ok(if argv.get(1).is_some_and(|a| a == "-F") {
            Output::stdout("# found\n")
        } else {
            Output::default()
        })
    })
}

struct Run {
    code: i32,
    console: Recorded,
    system: Arc<FakeSystem>,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.console.out()).unwrap_or(Value::Null)
    }
}

impl Desk {
    fn context(&self, console: &Recorded, system: &Arc<FakeSystem>, tty: bool) -> Context {
        Context {
            home: self.home.path().to_owned(),
            config_home: None,
            state_home: None,
            stdin_tty: tty,
            style: Style {
                color: false,
                width: 120,
            },
            system: Arc::clone(system) as Arc<dyn hs_cli::system::System>,
            console: Box::new(console.clone()),
            poll: Duration::from_millis(10),
            servers: Arc::new(hs_cli::servers::RealServers),
        }
    }

    async fn run_with(&self, argv: &[&str], console: Recorded, system: FakeSystem, tty: bool) -> Run {
        let system = Arc::new(system);
        let mut context = self.context(&console, &system, tty);
        let code = app::run(&strings(argv), &mut context).await;
        Run { code, console, system }
    }

    async fn run(&self, argv: &[&str]) -> Run {
        self.run_with(argv, Recorded::default(), known_hosts_system(), false)
            .await
    }

    fn ssh_dir(&self) -> PathBuf {
        self.home.path().join(".ssh/config.d/homestack")
    }

    fn ops(&self) -> Vec<String> {
        self.sim.ops().iter().map(ToString::to_string).collect()
    }
}

fn parsed(argv: &[&str]) -> Result<Command, String> {
    args::parse(&strings(argv))
        .map(|parsed| parsed.command)
        .map_err(|e| e.to_string())
}

/// Port of `ParserTests::test_package_cli_main_is_callable`: help needs no
/// configuration and exits 0.
#[tokio::test]
async fn package_cli_main_is_callable() -> TestResult {
    let home = tempfile::tempdir()?;
    let console = Recorded::default();
    let mut context = Context {
        home: home.path().to_owned(),
        config_home: None,
        state_home: None,
        stdin_tty: false,
        style: Style {
            color: false,
            width: 100,
        },
        system: Arc::new(FakeSystem::new()),
        console: Box::new(console.clone()),
        poll: Duration::from_millis(10),
        servers: Arc::new(hs_cli::servers::RealServers),
    };
    assert_eq!(app::run(&strings(&["--help"]), &mut context).await, 0);
    assert!(console.out().contains("homestack create VMID NAME"));
    assert_eq!(app::run(&strings(&["status", "-h"]), &mut context).await, 0);
    Ok(())
}

/// Port of `ParserTests::test_create_refresh_destroy_status_and_migrate`.
#[test]
fn create_refresh_destroy_status_and_migrate() {
    assert_eq!(
        parsed(&["create", "200", "test1", "--home-size", "40G"]),
        Ok(Command::Create {
            vmid: 200,
            name: "test1".to_owned(),
            home_size: Some("40G".to_owned()),
            storage: None,
            node: None,
        })
    );
    for (argv, target) in [
        (["refresh", "200"], "200"),
        (["refresh", "test1"], "test1"),
        (["destroy", "200"], "200"),
        (["destroy", "test1"], "test1"),
    ] {
        let command = parsed(&argv);
        let target = target.to_owned();
        let expected = if argv[0] == "refresh" {
            Command::Refresh { target }
        } else {
            Command::Destroy { target }
        };
        assert_eq!(command, Ok(expected));
    }
    assert_eq!(
        parsed(&[
            "migrate",
            "test1",
            "example-node-2",
            "--target-storage",
            "example-storage-b"
        ]),
        Ok(Command::Migrate {
            target: "test1".to_owned(),
            target_node: "example-node-2".to_owned(),
            target_storage: "example-storage-b".to_owned(),
        })
    );
    assert_eq!(
        parsed(&["repo", "test1"]),
        Ok(Command::Repo {
            target: "test1".to_owned(),
            repository: None
        })
    );
    assert_eq!(
        parsed(&["repo", "test1", "hipotures/tklivetracker"]),
        Ok(Command::Repo {
            target: "test1".to_owned(),
            repository: Some("hipotures/tklivetracker".to_owned())
        })
    );
    assert_eq!(parsed(&["status"]), Ok(Command::Status { target: None }));
    assert_eq!(
        parsed(&["status", "200"]),
        Ok(Command::Status {
            target: Some("200".to_owned())
        })
    );
    assert_eq!(
        parsed(&["status", "test1"]),
        Ok(Command::Status {
            target: Some("test1".to_owned())
        })
    );
}

/// Port of `ParserTests::test_sync_command_is_rejected`.
#[tokio::test]
async fn sync_command_is_rejected() -> TestResult {
    assert!(parsed(&["sync", "200"]).is_err());
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk.run(&["sync", "200"]).await;
    assert_eq!(run.code, 2);
    assert!(run.console.err().contains("usage"), "{}", run.console.err());
    Ok(())
}

/// Port of `ParserTests::test_create_accepts_target_node_with_large_home`.
#[test]
fn create_accepts_target_node_with_large_home() {
    assert_eq!(
        parsed(&[
            "create",
            "200",
            "test1",
            "--node",
            "example-node-2",
            "--storage",
            "example-storage-b",
            "--home-size",
            "500G"
        ]),
        Ok(Command::Create {
            vmid: 200,
            name: "test1".to_owned(),
            home_size: Some("500G".to_owned()),
            storage: Some("example-storage-b".to_owned()),
            node: Some("example-node-2".to_owned()),
        })
    );
}

/// Port of `ResizeCommandTests::test_parser_requires_exactly_one_disk_size`.
#[test]
fn parser_requires_exactly_one_disk_size() {
    assert_eq!(
        parsed(&["resize", "gpu", "--root-size", "32G"]),
        Ok(Command::Resize {
            target: "gpu".to_owned(),
            root_size: Some("32G".to_owned()),
            home_size: None,
        })
    );
    assert!(parsed(&["resize", "gpu"]).is_err());
    assert!(parsed(&["resize", "gpu", "--root-size", "32G", "--home-size", "500G"]).is_err());
}

/// Port of `CreateCommandTests::test_json_plan_forwards_destination_without_creating_vm`.
#[tokio::test]
async fn json_plan_forwards_destination_without_creating_vm() -> TestResult {
    let server_toml = SERVER_TOML.replace(
        "homestack-storage-1 = [\"local-lvm\"]",
        "homestack-storage-1 = [\"local-lvm\"]\nhomestack-storage-2 = [\"fast\"]",
    );
    let desk = desk(TRANSFER, &server_toml).await?;
    let run = desk
        .run(&[
            "create",
            "240",
            "test1",
            "--node",
            "pve-example-2",
            "--home-size",
            "500G",
            "--storage",
            "fast",
            "--json",
        ])
        .await;
    assert_eq!(run.code, 3, "{}", run.console.out());
    let out = run.json();
    assert_eq!(out["ok"], false);
    assert_eq!(out["confirmation_required"], true);
    assert_eq!(out["plan"]["node"], "pve-example-2");
    assert_eq!(out["plan"]["source_node"], "pve-example-1");
    assert_eq!(out["plan"]["home_size"], "500G");
    assert_eq!(out["plan"]["root_storage"], "fast");
    assert!(!desk.ops().iter().any(|op| op.contains("/clone")), "{:?}", desk.ops());
    Ok(())
}

/// Port of `ResizeCommandTests::test_json_plan_and_execution_require_confirmation`.
#[tokio::test]
async fn json_plan_and_execution_require_confirmation() -> TestResult {
    let desk = desk(RESIZE, SERVER_TOML).await?;
    let run = desk.run(&["resize", "200", "--home-size", "21G", "--json"]).await;
    assert_eq!(run.code, 3, "{}", run.console.out());
    assert_eq!(run.json()["confirmation_required"], true);
    assert_eq!(run.json()["plan"]["vmid"], 200);
    assert!(!desk.ops().iter().any(|op| op.contains("/resize")));
    let run = desk
        .run(&["resize", "200", "--home-size", "21G", "--json", "--yes"])
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    assert_eq!(run.json()["ok"], true);
    assert!(desk.ops().iter().any(|op| op.contains("/resize")));
    Ok(())
}

/// A plan that needs the LUKS passphrase asks for it only interactively
/// (`resize_auth.crypt_resize_access`) and sends it with the apply
/// (decision 6).
#[tokio::test]
async fn the_luks_passphrase_is_asked_only_interactively() -> TestResult {
    let desk = desk(RESIZE, SERVER_TOML).await?;
    let run = desk
        .run(&["resize", "202", "--root-size", "32G", "--json", "--yes"])
        .await;
    assert_eq!(run.code, 1);
    assert_eq!(
        run.json()["error"],
        "LUKS requires a passphrase. Run resize interactively without --json; no disk changes made"
    );
    let console = Recorded::answering(&["y"]).with_secret("correct horse battery staple");
    let run = desk
        .run_with(
            &["resize", "202", "--root-size", "32G"],
            console,
            known_hosts_system(),
            true,
        )
        .await;
    assert_eq!(run.code, 0, "{}{}", run.console.out(), run.console.err());
    assert!(
        run.console
            .questions()
            .iter()
            .any(|q| q.contains("LUKS passphrase for VM 202"))
    );
    assert!(run.console.out().contains("resized to 32G"), "{}", run.console.out());
    Ok(())
}

/// Ports of `CreateWorkspaceCompletionTests::test_create_result_uses_local_ssh_alias`
/// and the local steps of `destroy_workspace`
/// (`DestroyPlanPresentationTests::test_destroy_removes_local_ssh_artifacts_only_after_vm_is_absent`).
#[tokio::test]
async fn create_result_uses_local_ssh_alias() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk.run(&["create", "240", "hst-cli", "--json", "--yes"]).await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let out = run.json();
    assert_eq!(out["ok"], true);
    assert_eq!(out["ssh"]["root"], "ssh root@hst-cli");
    assert_eq!(out["ssh"]["user"], "ssh hst-cli");
    // A first connection has not made the workspace's known_hosts file yet.
    assert_eq!(out["ssh_known_hosts_removed"], serde_json::json!([]));
    let conf = desk.ssh_dir().join("vm240-hst-cli.conf");
    assert_eq!(out["ssh_config_path"], conf.display().to_string());
    assert!(std::fs::read_to_string(&conf)?.contains("HostName 192.0.2.240"));
    let known = desk.ssh_dir().join("known_hosts/vm240");
    assert!(
        known.parent().is_some_and(Path::is_dir),
        "the directory the config names"
    );
    assert!(std::fs::read_to_string(&conf)?.contains("UserKnownHostsFile ~/.ssh/config.d/homestack/known_hosts/vm240"));
    // ssh makes the file on the first connection.
    std::fs::write(&known, "192.0.2.240 ssh-ed25519 AAAA\n")?;

    let status = desk.run(&["status", "hst-cli", "--json"]).await;
    assert_eq!(status.code, 0);
    assert_eq!(status.json()["vmid"], 240);
    assert_eq!(status.json()["ok"], true);
    assert_eq!(status.json()["command"], "status");

    desk.sim.clear_ops();
    let run = desk.run(&["destroy", "hst-cli", "--json", "--yes"]).await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let out = run.json();
    assert_eq!(out["status"], "absent");
    assert_eq!(
        out["ssh_config_removed"],
        serde_json::json!([conf.display().to_string()])
    );
    assert_eq!(
        out["ssh_known_hosts_removed"],
        serde_json::json!([known.display().to_string()])
    );
    assert!(!known.exists());
    assert_eq!(out["backup_keys_left"], serde_json::json!([]));
    assert!(!conf.exists());
    // No ssh-keygen: the user's own known_hosts is never touched.
    assert!(run.system.runs().is_empty(), "{:?}", run.system.runs());
    Ok(())
}

/// Port of `CreateWorkspaceCompletionTests::test_local_ssh_config_failure_reports_verified_vm_without_rollback`.
#[tokio::test]
async fn local_ssh_config_failure_reports_verified_vm_without_rollback() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    std::fs::create_dir_all(desk.home.path().join(".ssh/config.d"))?;
    std::fs::write(desk.ssh_dir(), "a file where the directory should be")?;
    let run = desk.run(&["create", "240", "hst-cli", "--json", "--yes"]).await;
    assert_eq!(run.code, 1);
    let error = run.json()["error"].as_str().unwrap_or_default().to_owned();
    assert!(
        error.starts_with(
            "Workspace VM 240 (hst-cli) was created and verified successfully, but local SSH config generation failed: "
        ),
        "{error}"
    );
    assert!(
        !desk
            .ops()
            .iter()
            .any(|op| op.starts_with("delete{path=/nodes/pve-example-1/qemu/240"))
    );
    Ok(())
}

/// Port of `LocalSSHConfigTests::test_create_failure_does_not_generate_ssh_config`.
#[tokio::test]
async fn create_failure_does_not_generate_ssh_config() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    desk.sim.inject_fault(toml::from_str(
        "op = \"post{path=/nodes/pve-example-1/qemu/101/clone}\"\noccurrence = 1\nmessage = \"clone failed\"\n",
    )?)?;
    let run = desk.run(&["create", "240", "hst-cli", "--json", "--yes"]).await;
    assert_eq!(run.code, 1);
    assert!(
        run.json()["error"].as_str().is_some_and(|e| e.contains("clone failed")),
        "{}",
        run.console.out()
    );
    assert!(!desk.ssh_dir().exists());
    assert!(run.system.runs().is_empty());
    Ok(())
}

/// Ports of `DestroyPlanPresentationTests::test_destroy_reports_local_cleanup_failure_after_verified_deletion`
/// and `test_destroy_does_not_remove_backup_keys_when_vm_still_exists`.
#[tokio::test]
async fn destroy_reports_local_cleanup_failure_after_verified_deletion() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    assert_eq!(desk.run(&["create", "240", "hst-cli", "--json", "--yes"]).await.code, 0);
    let known = desk.ssh_dir().join("known_hosts/vm240");
    std::fs::write(&known, "192.0.2.240 ssh-ed25519 AAAA\n")?;

    desk.sim.inject_fault(toml::from_str(
        "op = \"delete{path=/nodes/pve-example-1/qemu/240}\"\noccurrence = 1\nmessage = \"destroy failed\"\n",
    )?)?;
    let run = desk.run(&["destroy", "240", "--json", "--yes"]).await;
    assert_eq!(run.code, 1, "{}", run.console.out());
    assert!(desk.ssh_dir().join("vm240-hst-cli.conf").exists());
    assert!(known.exists(), "nothing local is touched while the VM exists");

    // A file the desktop cannot remove (a directory in its place).
    std::fs::remove_file(&known)?;
    std::fs::create_dir_all(&known)?;
    std::fs::write(known.join("inside"), "x")?;
    let run = desk.run(&["destroy", "240", "--json", "--yes"]).await;
    assert_eq!(run.code, 1);
    let error = run.json()["error"].as_str().unwrap_or_default().to_owned();
    assert!(
        error.starts_with("Workspace VM 240 (hst-cli) was destroyed successfully, but local SSH cleanup failed: "),
        "{error}"
    );
    Ok(())
}

/// Port of `LocalTests::test_local_failure_is_reported_without_failing_the_key_addition`
/// (`test_addkey.py`), with the key added through the server.
#[tokio::test]
async fn local_failure_is_reported_without_failing_the_key_addition() -> TestResult {
    let desk = desk(ADDKEY, SERVER_TOML).await?;
    let ssh = desk.home.path().join(".ssh");
    std::fs::create_dir_all(&ssh)?;
    std::fs::write(ssh.join("id_ecdsa_sk_trezor_vm.pub"), format!("{TREZOR}\n"))?;
    std::fs::write(ssh.join("id_ecdsa_sk_trezor_vm"), "private")?;
    let client_toml = desk.home.path().join(".config/homestack/client.toml");
    let text = std::fs::read_to_string(&client_toml)?.replace(
        "identity_files = [\"~/.ssh/example-hardware-key\"]",
        "identity_files = [\n  \"~/.ssh/example-hardware-key\",\n]",
    );
    std::fs::write(&client_toml, &text)?;
    let key = ssh.join("id_ecdsa_sk_trezor_vm.pub").display().to_string();
    let run = desk.run(&["addkey", "alpha", "--key", &key, "--json", "--yes"]).await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let out = run.json();
    assert_eq!(out["ok"], true);
    assert!(
        out["local_error"]
            .as_str()
            .is_some_and(|e| e.contains("one-line array")),
        "{out}"
    );
    assert!(out.get("local").is_none());
    assert_eq!(std::fs::read_to_string(&client_toml)?, text);
    Ok(())
}

/// The plan without `--yes` changes nothing and exits 3 for every
/// operation (`cli.main`); without a TTY and without `--json` it refuses.
#[tokio::test]
async fn every_operation_asks_before_it_changes_anything() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    assert_eq!(desk.run(&["create", "240", "hst-cli", "--json", "--yes"]).await.code, 0);
    desk.sim.clear_ops();
    for argv in [
        vec!["create", "241", "hst-two", "--json"],
        vec!["refresh", "240", "--json"],
        vec!["refresh", "hst-cli", "--json"],
        vec!["destroy", "hst-cli", "--json"],
    ] {
        let run = desk.run(&argv).await;
        let message = run.json()["message"].as_str().unwrap_or_default().to_owned();
        assert!(
            run.code == 3 && message.starts_with("No changes made."),
            "{argv:?}: {}",
            run.console.out()
        );
    }
    let run = desk.run(&["destroy", "240"]).await;
    assert_eq!(run.code, 1);
    assert!(run.console.out().contains("DESTROY PLAN"));
    assert!(
        run.console
            .out()
            .contains("Interactive confirmation requires a TTY; use --yes for automation")
    );
    let declined = desk
        .run_with(
            &["destroy", "240"],
            Recorded::answering(&["maybe", "n"]),
            known_hosts_system(),
            true,
        )
        .await;
    assert_eq!(declined.code, 0);
    assert!(declined.console.out().contains("Please enter Y or N"));
    assert!(declined.console.out().contains("Cancelled. No changes were made."));
    assert_eq!(
        declined.console.questions(),
        [
            "Permanently destroy this workspace INCLUDING its persistent home? [y/n] (n): ",
            "Permanently destroy this workspace INCLUDING its persistent home? [y/n] (n): "
        ]
    );
    let changes: Vec<String> = desk
        .ops()
        .into_iter()
        .filter(|op| !op.starts_with("get{") && !op.starts_with("guest_ping{"))
        .collect();
    assert!(changes.is_empty(), "{changes:?}");
    Ok(())
}

/// `status` of a VMID that does not exist, and errors as `--json`
/// (`status.workspace_status`, `cli.main`).
#[tokio::test]
async fn status_of_an_absent_vm_and_errors_keep_their_shape() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk.run(&["status", "239", "--json"]).await;
    assert_eq!(run.code, 0);
    assert_eq!(
        run.json(),
        serde_json::json!({"ok": false, "command": "status", "vmid": 239, "status": "absent"})
    );
    let run = desk.run(&["refresh", "9bad", "--json"]).await;
    assert_eq!(run.code, 1);
    assert_eq!(run.json()["ok"], false);
    assert_eq!(run.json()["command"], "refresh");
    assert!(
        run.json()["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("Invalid workspace target"))
    );
    let run = desk.run(&["status", "--json"]).await;
    assert_eq!(run.code, 0);
    let out = run.json();
    assert_eq!(
        (out["ok"].clone(), out["command"].clone(), out["scope"].clone()),
        (true.into(), "status".into(), "global".into())
    );
    assert_eq!(out["summary"]["storage_unit"], "GiB");
    assert_eq!(out["execution"]["type"], "server");
    Ok(())
}

/// The token goes only over HTTPS (decision 8): an `http://` URL is refused
/// before any request.
#[tokio::test]
async fn an_http_url_is_refused() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let client_toml = desk.home.path().join(".config/homestack/client.toml");
    let text = std::fs::read_to_string(&client_toml)?.replace("https://", "http://");
    std::fs::write(&client_toml, text)?;
    let run = desk.run(&["status", "--json"]).await;
    assert_eq!(run.code, 1);
    assert!(
        run.json()["error"].as_str().is_some_and(|e| e.contains("https://")),
        "{}",
        run.console.out()
    );
    Ok(())
}

/// Port of `EnvironmentDiscoveryTests::test_environment_discovery_reports_progress_without_changing_read_only_queries`
/// (`test_discovery.py`): `discover` reads only, now through the server.
#[tokio::test]
async fn environment_discovery_reports_progress_without_changing_read_only_queries() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    desk.sim.clear_ops();
    let run = desk.run(&["discover", "--json"]).await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let out = run.json();
    assert_eq!(out["ok"], true);
    assert_eq!(out["read_only"], true);
    assert!(out["server"]["discovery"].is_object(), "{out}");
    assert!(desk.ops().iter().all(|op| op.starts_with("get{")), "{:?}", desk.ops());
    let run = desk.run(&["transport", "--json"]).await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    assert_eq!(run.json()["pve"]["node"], "pve-example-1");
    assert_eq!(run.json()["transport"]["instances"][0]["ok"], true);
    Ok(())
}

/// Commands still in Python say so (owner's decision for phase 13).
#[tokio::test]
async fn python_only_commands_say_where_they_are() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    for (argv, phase) in [(vec!["repo", "x"], "14f")] {
        let run = desk.run(&argv).await;
        assert_eq!(run.code, 1, "{argv:?}");
        let text = run.console.out();
        assert!(
            text.contains("still provided by the Python client") && text.contains(phase),
            "{text}"
        );
    }
    Ok(())
}

/// `migrate` through the server: the plan exits 3 without `--yes`, the job
/// moves the VM, and the human output names where it went.
#[tokio::test]
async fn migrate_moves_the_workspace_through_the_server() -> TestResult {
    let server_toml = SERVER_TOML.replace(
        "homestack-storage-1 = [\"local-lvm\"]",
        "homestack-storage-1 = [\"local-lvm\"]\nhomestack-storage-2 = [\"local-lvm\", \"fast\"]\nhomestack-storage-3 = [\"local-lvm\"]",
    );
    let desk = desk(MIGRATE, &server_toml).await?;
    let run = desk
        .run(&["migrate", "200", "pve-example-2", "--target-storage", "fast", "--json"])
        .await;
    assert_eq!(run.code, 3, "{}", run.console.out());
    assert_eq!(run.json()["plan"]["target_node"], "pve-example-2");
    let run = desk
        .run(&["migrate", "200", "pve-example-2", "--target-storage", "fast", "--yes"])
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    assert!(
        run.console.out().contains("migrated to pve-example-2 on fast"),
        "{}",
        run.console.out()
    );
    let status = desk.run(&["status", "200", "--json"]).await;
    assert_eq!(status.json()["node"], "pve-example-2");
    Ok(())
}

/// A desktop whose workspace answers every guest call of the setup with
/// `ok` and nothing to change.
fn setup_system() -> FakeSystem {
    FakeSystem::new().run(|argv| {
        Ok(if argv[0] == "ssh" && argv.last().is_some_and(|last| last.contains("python3 -")) {
            Output::stdout(
                r#"{"ok": true, "changed": [], "paths": [], "items": [], "registry": {}, "created": false, "state_path": "~/.local/state/homestack/setup.json"}"#,
            )
        } else {
            Output::default()
        })
    })
}

/// Port of `test_setup.py::CLITests::test_list_json_without_herdr_or_guest`.
#[tokio::test]
async fn list_json_without_herdr_or_guest() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk
        .run_with(&["setup", "list", "--json"], Recorded::default(), setup_system(), false)
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let result = run.json();
    assert!(result["catalog"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(result["items"].as_array().is_some_and(|items| !items.is_empty()));
    assert_eq!(result["guest_state"], "unknown");
    assert!(run.system.masters().is_empty());
    Ok(())
}

/// Port of `CLITests::test_missing_target_selectors_and_invalid_input_no_transport`.
#[tokio::test]
async fn missing_target_selectors_and_invalid_input_no_transport() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    for argv in [
        &["setup", "--json"][..],
        &["setup", "workspace", "--yes", "--json"],
        &["setup", "workspace", "app=missing", "--yes", "--json"],
        &["setup", "workspace", "f=1", "files=2", "--json"],
    ] {
        let run = desk.run_with(argv, Recorded::default(), setup_system(), false).await;
        assert_eq!(run.code, 1, "{argv:?}");
        assert_eq!(run.json()["ok"], false, "{argv:?}");
        assert!(run.system.masters().is_empty());
    }
    Ok(())
}

/// Port of `CLITests::test_dry_run_confirmation_and_unattended_json`.
#[tokio::test]
async fn dry_run_confirmation_and_unattended_json() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk
        .run_with(
            &[
                "setup",
                "alien-workspace",
                "env=bash",
                "app=codex",
                "--dry-run",
                "--json",
            ],
            Recorded::default(),
            setup_system(),
            false,
        )
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    assert_eq!(run.json()["dry_run"], true);
    assert!(run.system.masters().is_empty());
    let run = desk
        .run_with(
            &["setup", "160", "env=bash", "--json"],
            Recorded::default(),
            setup_system(),
            false,
        )
        .await;
    assert_eq!(run.code, 3, "{}", run.console.out());
    assert_eq!(run.json()["confirmation_required"], true);
    assert!(run.system.masters().is_empty());
    let run = desk
        .run_with(
            &[
                "setup",
                "alien-workspace",
                "app=codex",
                "--yes",
                "--non-interactive",
                "--json",
            ],
            Recorded::default(),
            setup_system(),
            false,
        )
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let result = run.json();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["plan"]["non_interactive"], true);
    assert_eq!(run.system.masters().len(), 1);
    Ok(())
}

/// Port of `CLITests::test_explicit_selectors_never_enter_tui_and_tty_review_can_cancel`.
#[tokio::test]
async fn explicit_selectors_never_enter_tui_and_tty_review_can_cancel() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk
        .run_with(
            &["setup", "alien-workspace", "env=bash"],
            Recorded::answering(&["n"]),
            setup_system(),
            true,
        )
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    assert!(run.console.out().contains("Cancelled"));
    assert!(run.system.masters().is_empty());
    Ok(())
}

/// Port of `test_setup_state.py::SetupStatusCLITests::test_setup_status_is_read_only_and_uses_one_workspace_connection`.
#[tokio::test]
async fn setup_status_is_read_only_and_uses_one_workspace_connection() -> TestResult {
    let desk = desk(LIFECYCLE, SERVER_TOML).await?;
    let run = desk
        .run_with(
            &["setup", "status", "alien-workspace", "--json"],
            Recorded::default(),
            setup_system(),
            false,
        )
        .await;
    assert_eq!(run.code, 0, "{}", run.console.out());
    let result = run.json();
    assert_eq!(result["command"], "setup status");
    assert_eq!(result["items"]["codex"]["state"], "installed");
    assert_eq!(run.system.masters().len(), 1);
    let exits = run
        .system
        .runs()
        .iter()
        .filter(|argv| argv.contains(&"exit".to_owned()))
        .count();
    assert_eq!(exits, 1);
    let guests: Vec<String> = run
        .system
        .inputs
        .lock()
        .map(|inputs| {
            inputs
                .iter()
                .map(|(_, input)| input.lines().last().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        guests
            .iter()
            .all(|call| !call.contains("\\\"apply\\\":true") && !call.contains("state-record"))
    );
    Ok(())
}

/// `homestack setup` takes Python's arguments, its positionals in one run.
#[test]
fn setup_arguments_are_python_s() {
    assert_eq!(
        parsed(&[
            "setup",
            "--json",
            "alien-workspace",
            "env=bash",
            "app=codex",
            "--catalog",
            "abc",
            "--dry-run"
        ]),
        Ok(Command::Setup(args::Setup {
            target: Some("alien-workspace".to_owned()),
            selectors: vec!["env=bash".to_owned(), "app=codex".to_owned()],
            local: false,
            catalog: Some("abc".to_owned()),
            dry_run: true,
            non_interactive: false,
        }))
    );
    let scattered = parsed(&["setup", "alien-workspace", "--json", "env=bash"])
        .err()
        .unwrap_or_default();
    assert!(scattered.contains("unrecognized arguments: env=bash"), "{scattered}");
}
