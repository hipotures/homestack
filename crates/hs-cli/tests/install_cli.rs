//! `homestack install` and `homestack uninstall` (phase 13c): the command
//! line, the plan and its confirmation, the exit codes and `--json`, with the
//! root access over SSH and through a Herdr pane, both answered by a local
//! shell that sees the fake Proxmox tools of `deploy/pve/tests/fake-pve`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{FakeSystem, Recorded, strings};
use hs_cli::app::{self, Context};
use hs_cli::args::{self, Command};
use hs_cli::servers::Servers;
use hs_cli::system::Output;
use hs_cli::view::Style;
use hs_deploy::config::DeployConfig;
use hs_deploy::dns::DnsHost;
use serde_json::Value;

/// A fake single-node or three-node cluster on disk.
struct Cluster {
    _work: tempfile::TempDir,
    dir: PathBuf,
    home: PathBuf,
    screen: Arc<Mutex<String>>,
}

impl Cluster {
    fn new(nodes: &[&str]) -> Self {
        let work = tempfile::tempdir().expect("temporary directory");
        let dir = work.path().join("cluster");
        let home = work.path().join("home");
        std::fs::create_dir_all(dir.join("bin")).expect("bin");
        std::fs::create_dir_all(dir.join("vz/snippets")).expect("vz");
        std::fs::create_dir_all(home.join(".config/homestack")).expect("home");
        let tool = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/pve/tests/fake-pve")
            .canonicalize()
            .expect("fake-pve");
        for name in ["pvesh", "pveum", "pvesm", "ssh", "openssl", "id", "hostname"] {
            std::os::unix::fs::symlink(&tool, dir.join("bin").join(name)).expect("symlink");
        }
        let cluster = Self {
            _work: work,
            dir,
            home,
            screen: Arc::new(Mutex::new("root@pve1:~# ".to_owned())),
        };
        for file in ["calls.log", "acl.tsv", "tokens.tsv", "pools.tsv"] {
            cluster.write(file, "");
        }
        cluster.write("users", "root@pam\n");
        cluster.write(
            "roles.tsv",
            "PVEAuditor\tDatastore.Audit,Pool.Audit,Sys.Audit,VM.Audit\t1\nPVESDNUser\tSDN.Audit,SDN.Use\t1\n",
        );
        cluster.write("options.json", "{}\n");
        cluster.write(
            "storage.json",
            r#"[{"storage":"local","type":"dir","content":"iso,vztmpl,backup,snippets"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir"}]
"#,
        );
        let mut table = String::new();
        let mut trusted = String::new();
        for (index, name) in nodes.iter().enumerate() {
            let digits: String = name.chars().filter(char::is_ascii_digit).collect();
            table.push_str(&format!("{name}\t192.0.2.{digits}\t1\t{}\n", u8::from(index == 0)));
            trusted.push_str(&format!("{name}.example.net\n"));
        }
        cluster.write("nodes.tsv", &table);
        cluster.write("trusted", &trusted);
        if nodes.len() > 1 {
            cluster.write("cluster", "");
        }
        cluster.write(
            "vms.tsv",
            &format!(
                "101\t{}\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\t\n",
                nodes[0]
            ),
        );
        cluster
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).expect("write state");
    }

    fn changes(&self) -> usize {
        self.read("calls.log")
            .lines()
            .filter(|l| l.starts_with("pveum ") || l.starts_with("pvesh set "))
            .count()
    }

    fn token_dir(&self) -> PathBuf {
        self.home.join(".config/homestack")
    }

    /// Run `command` on the fake node, as the root shell would.
    fn shell(&self, command: &str) -> io::Result<std::process::Output> {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Process::new("bash")
            .arg("-c")
            .arg(command)
            .env("PATH", path)
            .env("HS_FAKE", &self.dir)
            .stdin(std::process::Stdio::null())
            .output()
    }

    /// The local commands of the desktop: `ssh` to the node, and `herdr`.
    fn system(&self) -> FakeSystem {
        let me = Arc::new(Self {
            _work: tempfile::tempdir().expect("unused"),
            dir: self.dir.clone(),
            home: self.home.clone(),
            screen: Arc::clone(&self.screen),
        });
        FakeSystem::new().run(move |argv| {
            let words: Vec<&str> = argv.iter().map(String::as_str).collect();
            match words.as_slice() {
                ["ssh", .., "-O", _, _] => Ok(Output::default()),
                ["ssh", .., _target, command] => {
                    let ran = me.shell(command)?;
                    Ok(Output {
                        code: ran.status.code().unwrap_or(-1),
                        stdout: String::from_utf8_lossy(&ran.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
                    })
                }
                ["herdr", "workspace", "list"] => Ok(Output::stdout(
                    r#"{"result":{"workspaces":[{"label":"PVE","workspace_id":"w3"}]}}"#,
                )),
                ["herdr", "tab", "list"] => Ok(Output::stdout(
                    r#"{"result":{"tabs":[{"label":"pve1","tab_id":"w3:t1","workspace_id":"w3"}]}}"#,
                )),
                ["herdr", "pane", "list"] => Ok(Output::stdout(
                    r#"{"result":{"panes":[{"pane_id":"w3:p1","tab_id":"w3:t1","terminal_title_stripped":"root@pve1:~"}]}}"#,
                )),
                ["herdr", "pane", "read", _, "--source", ..] => {
                    Ok(Output::stdout(&me.screen.lock().expect("screen").clone()))
                }
                ["herdr", "pane", "run", _, text] => {
                    let mut screen = me.screen.lock().expect("screen");
                    if text.starts_with("printf '\\033[3J") {
                        *screen = "root@pve1:~# ".to_owned();
                    } else {
                        let ran = me.shell(text)?;
                        screen.push_str(&format!(
                            "{text}\n{}root@pve1:~# ",
                            String::from_utf8_lossy(&ran.stdout)
                        ));
                    }
                    Ok(Output::stdout("{}"))
                }
                ["herdr", "pane", "wait-output", _, "--match", marker, ..] => {
                    if me.screen.lock().expect("screen").contains(marker) {
                        Ok(Output::stdout("{}"))
                    } else {
                        Ok(Output::failed(1, "timeout"))
                    }
                }
                _ => Ok(Output::default()),
            }
        })
    }
}

struct Run {
    code: i32,
    console: Recorded,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.console.out()).unwrap_or(Value::Null)
    }
}

async fn run(cluster: &Cluster, argv: &[&str], answers: &[&str], tty: bool) -> Run {
    run_with(cluster, argv, answers, tty, Arc::new(FakeServers::default())).await
}

async fn run_with(cluster: &Cluster, argv: &[&str], answers: &[&str], tty: bool, servers: Arc<FakeServers>) -> Run {
    let console = Recorded::answering(answers);
    let system = Arc::new(cluster.system());
    let mut context = Context {
        home: cluster.home.clone(),
        config_home: None,
        stdin_tty: tty,
        style: Style {
            color: false,
            width: 120,
        },
        system: system as Arc<dyn hs_cli::system::System>,
        console: Box::new(console.clone()),
        poll: Duration::from_millis(10),
        servers: servers as Arc<dyn Servers>,
    };
    let code = app::run(&strings(argv), &mut context).await;
    Run { code, console }
}

const STORAGES: [&str; 8] = [
    "--storages",
    "local-lvm",
    "--snippet-storage",
    "local",
    "--service-storages",
    "local-lvm",
    "--template-storage",
    "local",
];

fn install_argv<'a>(access: &'a [&'a str], extra: &'a [&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["install", "--no-servers"];
    argv.extend(access);
    argv.extend(STORAGES);
    argv.extend(extra);
    argv
}

#[test]
fn the_commands_are_parsed() {
    let parsed = args::parse(&strings(&[
        "install",
        "--ssh",
        "root@pve1",
        "--identity",
        "~/.ssh/id_pve",
        "--test",
        "--storages",
        "a,b",
        "--snippet-storage",
        "local",
        "--service-storages",
        "c",
        "--template-storage",
        "local",
        "--gold",
        "101",
        "--token-dir",
        "/tmp/t",
    ]))
    .expect("parsed");
    let Command::Install(install) = parsed.command else {
        panic!("not install");
    };
    assert_eq!(install.ssh.as_deref(), Some("root@pve1"));
    assert_eq!(install.herdr, None);
    assert_eq!(install.identity.as_deref(), Some("~/.ssh/id_pve"));
    assert!(install.test);
    assert_eq!(install.storages, ["a", "b"]);
    assert_eq!(install.service_storages, ["c"]);
    assert_eq!(install.gold, Some(101));
    assert_eq!(install.token_dir, Some(PathBuf::from("/tmp/t")));

    let parsed = args::parse(&strings(&["uninstall", "--herdr", "pve1", "--force", "-y"])).expect("parsed");
    assert!(parsed.yes);
    let Command::Uninstall(install) = parsed.command else {
        panic!("not uninstall");
    };
    assert_eq!(install.herdr.as_deref(), Some("pve1"));
    assert!(install.force);

    // Neither way given is allowed (the installer asks); both is not.
    assert!(args::parse(&strings(&["install", "--no-servers"])).is_ok());
    // --force belongs to uninstall.
    for bad in [
        vec!["install", "--ssh", "a", "--herdr", "b"],
        vec!["install", "--ssh", "a", "--force"],
        vec!["uninstall", "--ssh", "a", "--gold", "x"],
    ] {
        assert!(args::parse(&strings(&bad)).is_err(), "{bad:?} was accepted");
    }
}

#[tokio::test]
async fn a_plan_without_confirmation_is_shown_and_changes_nothing() {
    let cluster = Cluster::new(&["pve1"]);
    let json = run(
        &cluster,
        &install_argv(&["--ssh", "root@pve1"], &["--json"]),
        &[],
        false,
    )
    .await;
    assert_eq!(json.code, 3, "{}", json.console.out());
    let out = json.json();
    assert_eq!(out["confirmation_required"], true, "{out}");
    assert_eq!(out["plan"]["topology"]["cluster"], false);
    assert_eq!(out["plan"]["host"], "pve1");
    assert!(
        out["plan"]["steps"].as_array().is_some_and(|steps| !steps.is_empty()),
        "{out}"
    );
    assert_eq!(cluster.changes(), 0);

    let text = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &[]), &[], false).await;
    assert_eq!(text.code, 1);
    let out = text.console.out();
    assert!(
        out.contains("single node") && out.contains("Plan (install, production part)"),
        "{out}"
    );
    assert!(out.contains("Interactive confirmation requires a TTY"), "{out}");
    assert_eq!(cluster.changes(), 0);
}

#[tokio::test]
async fn yes_installs_over_ssh_and_the_secrets_land_on_the_desktop() {
    let cluster = Cluster::new(&["pve1"]);
    let done = run(
        &cluster,
        &install_argv(&["--ssh", "root@pve1"], &["--yes", "--json"]),
        &[],
        false,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.json();
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["command"], "install");
    assert!(out["applied_steps"].as_u64().is_some_and(|n| n > 10), "{out}");
    let files: Vec<String> = out["token_files"]
        .as_array()
        .expect("token files")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    for (name, content) in [
        ("pve-token", "homestack@pve!server=fake-secret-server\n"),
        ("pve-token-deploy", "homestack@pve!deploy=fake-secret-deploy\n"),
    ] {
        let path = cluster.token_dir().join(name);
        assert_eq!(std::fs::read_to_string(&path).expect("token file"), content);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(!done.console.out().contains("fake-secret"), "the secret was printed");
    assert!(cluster.read("users").contains("homestack@pve"));

    // A second run has nothing to do.
    let again = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &["--yes"]), &[], false).await;
    assert_eq!(again.code, 0, "{}", again.console.out());
    assert!(
        again.console.out().contains("Nothing to change"),
        "{}",
        again.console.out()
    );
}

#[tokio::test]
async fn the_question_is_asked_on_a_terminal_and_no_changes_nothing() {
    let cluster = Cluster::new(&["pve1"]);
    let declined = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &[]), &["n"], true).await;
    assert_eq!(declined.code, 0);
    assert!(
        declined.console.out().contains("Cancelled. No changes were made."),
        "{}",
        declined.console.out()
    );
    assert_eq!(declined.console.questions().len(), 1);
    assert_eq!(cluster.changes(), 0);

    let accepted = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &[]), &["y"], true).await;
    assert_eq!(accepted.code, 0, "{}", accepted.console.out());
    assert!(cluster.read("users").contains("homestack@pve"));
}

#[tokio::test]
async fn install_works_through_a_herdr_pane_and_clears_the_screen_after_a_secret() {
    let cluster = Cluster::new(&["pve1"]);
    let done = run(
        &cluster,
        &install_argv(&["--herdr", "pve1"], &["--yes", "--json"]),
        &[],
        false,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    assert_eq!(done.json()["ok"], true);
    assert!(cluster.read("users").contains("homestack@pve"));
    assert!(cluster.token_dir().join("pve-token").exists());
    // The last secret was followed by a clear; what is on the pane now holds no secret.
    let screen = cluster.screen.lock().expect("screen").clone();
    assert!(!screen.contains("fake-secret"), "{screen}");
}

#[tokio::test]
async fn uninstall_removes_homestack_and_keeps_the_desktop_files() {
    let cluster = Cluster::new(&["pve1", "pve2"]);
    let access = ["--ssh", "root@pve1"];
    assert_eq!(
        run(&cluster, &install_argv(&access, &["--yes"]), &[], false).await.code,
        0
    );
    let gone = run(
        &cluster,
        &[
            "uninstall",
            "--ssh",
            "root@pve1",
            "--snippet-storage",
            "local",
            "--yes",
            "--json",
        ],
        &[],
        false,
    )
    .await;
    assert_eq!(gone.code, 0, "{}", gone.console.out());
    assert!(!cluster.read("users").contains("homestack"));
    assert!(cluster.read("tokens.tsv").is_empty());
    assert!(
        cluster.token_dir().join("pve-token").exists(),
        "a desktop file was removed"
    );
    assert!(
        gone.json()["plan"]["notes"][0]
            .as_str()
            .is_some_and(|n| n.contains("pve-token")),
        "{}",
        gone.console.out()
    );
}

#[tokio::test]
async fn a_node_that_does_not_answer_makes_the_run_fail_after_the_rest() {
    let cluster = Cluster::new(&["pve1", "pve2"]);
    cluster.write("offline-ssh", "pve2\n");
    let done = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &["--yes"]), &[], false).await;
    assert_eq!(done.code, 1, "{}", done.console.out());
    assert!(done.console.out().contains("pve2"), "{}", done.console.out());
    assert!(cluster.read("users").contains("homestack@pve"));
}

#[tokio::test]
async fn an_access_that_is_not_root_or_not_proxmox_stops_before_the_plan() {
    let cluster = Cluster::new(&["pve1"]);
    cluster.write("uid", "1000\n");
    let refused = run(&cluster, &install_argv(&["--ssh", "user@pve1"], &["--yes"]), &[], false).await;
    assert_eq!(refused.code, 1);
    assert!(refused.console.out().contains("not root"), "{}", refused.console.out());
    assert_eq!(cluster.changes(), 0);

    let cluster = Cluster::new(&["pve1"]);
    std::fs::remove_file(cluster.dir.join("bin/pvesh")).expect("remove pvesh");
    let refused = run(
        &cluster,
        &install_argv(&["--ssh", "root@pve1"], &["--yes", "--json"]),
        &[],
        false,
    )
    .await;
    assert_eq!(refused.code, 1);
    assert_eq!(refused.json()["ok"], false);
    assert!(
        refused.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("not a Proxmox VE node")),
        "{}",
        refused.console.out()
    );
}

#[tokio::test]
async fn the_checks_of_the_cluster_owner_are_listed() {
    let cluster = Cluster::new(&["pve1"]);
    cluster.write(
        "storage.json",
        &cluster
            .read("storage.json")
            .replace("iso,vztmpl,backup,snippets", "iso"),
    );
    let refused = run(&cluster, &install_argv(&["--ssh", "root@pve1"], &["--yes"]), &[], false).await;
    assert_eq!(refused.code, 1);
    let out = refused.console.out();
    assert!(out.contains("does not allow content type snippets"), "{out}");
    assert!(out.contains("nothing changed"), "{out}");
}

#[tokio::test]
async fn the_help_names_both_commands() {
    let cluster = Cluster::new(&["pve1"]);
    let help = run(&cluster, &["--help"], &[], false).await;
    let out = help.console.out();
    assert!(
        out.contains("homestack install") && out.contains("homestack uninstall"),
        "{out}"
    );
    assert!(!out.contains("still in the Python client (phase 13c)"), "{out}");
}

// --- the interactive installer ---------------------------------------------

/// Two storages can hold disk images, so that one is asked.
fn two_disk_storages(cluster: &Cluster) {
    cluster.write(
        "storage.json",
        r#"[{"storage":"local","type":"dir","content":"iso,vztmpl,backup,snippets"},
 {"storage":"backup-a","type":"dir","content":"images,backup"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir"}]
"#,
    );
}

#[tokio::test]
async fn without_a_terminal_the_missing_options_are_listed_with_their_defaults() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    let text = run(&cluster, &["install", "--no-servers", "--ssh", "root@pve1"], &[], false).await;
    assert_eq!(text.code, 1);
    let out = text.console.out();
    assert!(!out.contains("ERROR"), "{out}");
    assert!(out.contains("--storages is required"), "{out}");
    assert!(
        out.lines().any(|l| l.contains("local-lvm") && l.contains("(default)")),
        "{out}"
    );
    assert!(
        out.lines().any(|l| l.contains("backup-a") && !l.contains("(default)")),
        "{out}"
    );
    assert!(out.lines().all(|l| l.chars().count() <= 100), "{out}");
    assert_eq!(cluster.changes(), 0);

    let json = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1", "--json"],
        &[],
        false,
    )
    .await;
    assert_eq!(json.code, 1);
    let out = json.json();
    assert_eq!(out["missing"][0]["option"], "--storages", "{out}");
    assert_eq!(out["missing"][0]["defaults"][0], "local-lvm", "{out}");
}

#[tokio::test]
async fn on_a_terminal_every_setting_is_shown_and_nothing_happens_before_install() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    // Enter installs from the form; the confirmation is declined.
    let declined = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["", "n"],
        true,
    )
    .await;
    assert_eq!(declined.code, 0, "{}", declined.console.out());
    assert!(declined.console.out().contains("Cancelled. No changes were made."));
    assert_eq!(cluster.changes(), 0, "something changed before Install");

    let done = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["", "y"],
        true,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    let form = out.split("Confirm the installation").next().unwrap_or_default();
    for row in [
        "Root access",
        "Workspace disks      local-lvm",
        "Cloud-Init snippets  local",
        "Server disks         local-lvm",
        "Server image         local",
        "Gold VM              101 gold",
    ] {
        assert!(form.contains(row), "the form lacks {row:?}:\n{form}");
    }
    assert!(
        !form.contains("--"),
        "a command-line option is shown in the form:\n{form}"
    );
    for text in [
        "Account and tokens",
        "new user homestack@pve",
        "Same as: homestack install --ssh root@pve1 --storages local-lvm --snippet-storage local --service-storages local-lvm --template-storage local --gold 101 --no-servers",
    ] {
        assert!(out.contains(text), "{text:?} missing:\n{out}");
    }
    assert!(cluster.read("users").contains("homestack@pve"), "not installed");
}

#[tokio::test]
async fn a_setting_is_changed_from_its_list_and_gold_can_be_chosen() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    cluster.write(
        "vms.tsv",
        "101\tpve1\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\t\n\
         150\tpve1\tother\t\tvirtio=BC:24:11:00:00:05,bridge=vmbr0\t\n",
    );
    // 1 = workspace disks: tick both; 5 = Gold: the second VM; then install.
    let done = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["1", "1,2", "5", "2", "", "y"],
        true,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    assert!(out.contains("Workspace disks:     backup-a, local-lvm"), "{out}");
    assert!(out.contains("Gold:                150 other"), "{out}");
    assert!(
        cluster.read("pools.tsv").contains("homestack-gold\t150"),
        "{}",
        cluster.read("pools.tsv")
    );
}

#[tokio::test]
async fn options_given_on_the_command_line_are_shown_but_not_asked() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    let done = run(
        &cluster,
        &[
            "install",
            "--no-servers",
            "--ssh",
            "root@pve1",
            "--storages",
            "backup-a",
        ],
        &["", "y"],
        true,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    assert!(
        out.contains("Workspace disks      backup-a  (given on the command line)"),
        "{out}"
    );
    assert!(out.contains("Workspace disks:     backup-a"), "{out}");
}

#[tokio::test]
async fn giving_up_changes_nothing() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    let gave_up = run(&cluster, &["install", "--no-servers", "--ssh", "root@pve1"], &[], true).await;
    assert_eq!(gave_up.code, 0, "{}", gave_up.console.out());
    assert!(gave_up.console.out().contains("Cancelled"), "{}", gave_up.console.out());
    assert_eq!(cluster.changes(), 0);
}

// --- the default installer asks how to reach root --------------------------

#[tokio::test]
async fn without_an_access_option_the_installer_asks_for_one_and_goes_on() {
    let cluster = Cluster::new(&["pve1"]);
    // 1 = SSH, the node, then the form and the confirmation.
    let done = run(
        &cluster,
        &["install", "--no-servers"],
        &["1", "root@pve1", "", "y"],
        true,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    assert!(
        out.contains("SSH") && out.contains("Herdr"),
        "the ways to reach root are not offered:\n{out}"
    );
    assert!(
        out.contains("Same as: homestack install --ssh root@pve1 --storages local-lvm"),
        "{out}"
    );
    assert!(cluster.read("users").contains("homestack@pve"), "not installed");
}

#[tokio::test]
async fn the_herdr_tabs_are_offered_to_choose_from() {
    let cluster = Cluster::new(&["pve1"]);
    // 2 = Herdr, the only tab, then the form and the confirmation.
    let done = run(&cluster, &["install", "--no-servers"], &["2", "1", "", "y"], true).await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    assert!(
        out.contains("PVE/pve1") && out.contains("root@pve1:~"),
        "the tab is not described:\n{out}"
    );
    assert!(out.contains("Same as: homestack install --herdr PVE/pve1"), "{out}");
    assert!(cluster.read("users").contains("homestack@pve"));
}

#[tokio::test]
async fn without_a_terminal_an_access_option_is_required() {
    let cluster = Cluster::new(&["pve1"]);
    let refused = run(
        &cluster,
        &["install", "--no-servers", "--storages", "local-lvm"],
        &[],
        false,
    )
    .await;
    assert_eq!(refused.code, 1);
    let out = refused.console.out();
    assert!(
        out.contains("--ssh") && out.contains("--herdr") && out.contains("terminal"),
        "{out}"
    );
    assert_eq!(cluster.changes(), 0);

    let gave_up = run(&cluster, &["install", "--no-servers"], &[], true).await;
    assert_eq!(gave_up.code, 0, "{}", gave_up.console.out());
    assert!(gave_up.console.out().contains("Cancelled"), "{}", gave_up.console.out());
}

#[tokio::test]
async fn the_choices_are_kept_for_the_next_run() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    // Workspace disks: only backup-a; then the user leaves without installing.
    let left = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["1", "1"],
        true,
    )
    .await;
    assert_eq!(left.code, 0, "{}", left.console.out());
    assert_eq!(cluster.changes(), 0);
    let draft = cluster.token_dir().join("install-draft.toml");
    assert!(draft.exists(), "the choices were not kept");
    assert_eq!(
        std::fs::metadata(&draft).expect("draft").permissions().mode() & 0o777,
        0o600
    );

    // The next run starts from them.
    let again = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["", "n"],
        true,
    )
    .await;
    let out = again.console.out();
    assert!(out.contains("Workspace disks      backup-a"), "{out}");

    // A draft that cannot be read is ignored.
    std::fs::write(&draft, "not = [toml").expect("write");
    let again = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["", "n"],
        true,
    )
    .await;
    assert!(
        again.console.out().contains("Workspace disks      local-lvm"),
        "{}",
        again.console.out()
    );
}

#[tokio::test]
async fn the_confirmation_groups_the_changes_in_short_lines() {
    let cluster = Cluster::new(&["pve1"]);
    std::fs::write(cluster.token_dir().join("pve-token"), "old\n").expect("old token file");
    let done = run(
        &cluster,
        &["install", "--no-servers", "--ssh", "root@pve1"],
        &["", "n"],
        true,
    )
    .await;
    let out = done.console.out();
    let confirm = out.split("Confirm the installation").nth(1).unwrap_or_default();
    for heading in [
        "Account and tokens",
        "Roles (6 new)",
        "Pools (3 new)",
        "Gold",
        "Permissions",
        "Cloud-Init snippets",
    ] {
        assert!(confirm.contains(heading), "no {heading:?}:\n{confirm}");
    }
    assert!(confirm.contains("old file kept as pve-token.replaced-"), "{confirm}");
    assert!(confirm.contains("~/.config/homestack/pve-token"), "{confirm}");
    assert!(
        !confirm.contains("give homestack@pve"),
        "raw permission steps are listed:\n{confirm}"
    );
    let changes = confirm.split("Account and tokens").nth(1).unwrap_or_default();
    assert!(
        changes.lines().all(|l| l.chars().count() <= 100),
        "a line is too long:\n{changes}"
    );
}

// --- the servers and the final status (section 8) --------------------------

const API_TOKEN: &str = "install-test-api-token-0123456789abcdef";
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const LIFECYCLE: &str = include_str!("../../hs-core/tests/data/lifecycle-cluster.toml");
/// The configuration of the status server, for the lifecycle cluster.
const STATUS_SERVER_TOML: &str = r#"version = 1
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

/// The deployment and the desktop's DNS, recorded instead of done.
#[derive(Default)]
struct FakeServers {
    /// What `apiN.<domain>` resolves to; a name not here does not resolve.
    names: BTreeMap<String, Ipv4Addr>,
    deployed: Mutex<Vec<DeployConfig>>,
    /// The URL and CA file the client reaches the servers at; none: nothing answers.
    reach: Option<(String, String)>,
    fail: Option<String>,
    /// The public zone of a server domain and who serves its DNS; none:
    /// the domain's parent at OVH.
    zone: Option<(String, DnsHost)>,
}

impl FakeServers {
    fn resolving(names: &[(&str, &str)]) -> Self {
        Self {
            names: names
                .iter()
                .map(|(name, ip)| ((*name).to_owned(), ip.parse().expect("address")))
                .collect(),
            ..Self::default()
        }
    }

    fn deployed(&self) -> Vec<DeployConfig> {
        self.deployed.lock().expect("deployed").clone()
    }
}

impl Servers for FakeServers {
    fn public_zone(&self, domain: &str) -> Result<(String, DnsHost), String> {
        Ok(self.zone.clone().unwrap_or_else(|| {
            let parent = domain.split_once('.').map_or(domain, |(_, rest)| rest);
            (parent.to_owned(), DnsHost::Ovh)
        }))
    }

    fn resolve(&self, name: &str) -> Option<Ipv4Addr> {
        self.names.get(name).copied()
    }

    fn deploy<'a>(
        &'a self,
        config: &'a DeployConfig,
        say: &'a (dyn Fn(&str) + Send + Sync),
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.deployed.lock().expect("deployed").push(config.clone());
            say("fake deployment");
            self.fail.clone().map_or(Ok(()), Err)
        })
    }

    fn client_server(&self, _config: &DeployConfig) -> (Vec<String>, Option<String>) {
        match &self.reach {
            Some((url, ca)) => (vec![url.clone()], Some(ca.clone())),
            None => (vec!["https://127.0.0.1:9".to_owned()], None),
        }
    }
}

/// The desktop files an install with servers needs: the old configuration,
/// the client token, the OVH key.
fn desktop_files(cluster: &Cluster) {
    let dir = cluster.token_dir();
    let example =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python/config.example.toml"))
            .expect("example configuration");
    std::fs::write(dir.join("config.toml"), example).expect("config.toml");
    for (name, text) in [("api-token", API_TOKEN), ("ovh-dns", "OVH_AK=a\nOVH_AS=b\nOVH_CK=c\n")] {
        std::fs::write(dir.join(name), text).expect("file");
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
}

/// A server on the simulated lifecycle cluster, behind TLS: its URL and CA.
async fn status_server() -> (String, String, pve_sim::PveSim) {
    let sim = pve_sim::PveSim::start(pve_sim::ClusterSpec::from_scenario(LIFECYCLE).expect("scenario"))
        .await
        .expect("simulator");
    let mut pve = hs_pve::PveClientConfig::new(
        vec![sim.base_url().parse().expect("url")],
        hs_pve::PveToken::parse(PVE_TOKEN).expect("token"),
    );
    pve.extra_ca_pem = Some(sim.ca_pem().to_owned());
    let state = tempfile::tempdir().expect("state").keep();
    let app = hs_server::app_with(
        hs_server::ServerConfig {
            api_token: hs_server::ApiToken::parse(API_TOKEN).expect("api token").into(),
            pve: pve.into(),
            config_file: state.join("server.toml"),
            instance: hs_server::Instance::default(),
            pools: hs_server::Pools::default(),
        },
        hs_server::AppOptions::default(),
    )
    .expect("server");
    let certs = pve_sim::tls::generate().expect("certificate");
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        certs.chain_pem.clone().into_bytes(),
        certs.key_pem.clone().into_bytes(),
    )
    .await
    .expect("tls");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(
        axum_server::from_tcp_rustls(listener, tls)
            .expect("serve")
            .serve(app.into_make_service()),
    );
    let url = format!("https://localhost:{port}");
    let mut client =
        hs_client::ClientConfig::new(vec![url.parse::<url::Url>().expect("url").into()], API_TOKEN.to_owned());
    client.extra_ca_pem = Some(certs.ca_pem.clone());
    hs_client::Client::new(client)
        .expect("client")
        .configure_on(0, STATUS_SERVER_TOML)
        .await
        .expect("configured");
    (url, certs.ca_pem, sim)
}

fn server_argv<'a>(extra: &'a [&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["install", "--ssh", "root@pve1"];
    argv.extend(STORAGES);
    argv.extend(extra);
    argv
}

#[tokio::test]
async fn the_servers_are_deployed_after_proxmox_and_the_install_ends_with_status() {
    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    let (url, ca_pem, _sim) = status_server().await;
    let ca = cluster.token_dir().join("server-ca.pem");
    std::fs::write(&ca, ca_pem).expect("ca");
    let servers = Arc::new(FakeServers {
        reach: Some((url.clone(), ca.display().to_string())),
        ..FakeServers::resolving(&[
            ("api1.hs.example.net", "192.0.2.97"),
            ("api2.hs.example.net", "192.0.2.98"),
            ("api3.hs.example.net", "192.0.2.99"),
        ])
    });
    let done = run_with(
        &cluster,
        &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    let out = done.console.out();
    assert_eq!(done.code, 0, "{out}\n{}", done.console.err());
    assert!(
        cluster.read("users").contains("homestack@pve"),
        "the Proxmox part is missing"
    );

    let deployed = servers.deployed();
    assert_eq!(deployed.len(), 1, "deployed {} times", deployed.len());
    let config = &deployed[0];
    let instances: Vec<(String, String, String, u32, String)> = config
        .instances
        .iter()
        .map(|i| {
            (
                i.name.clone(),
                i.node.clone(),
                i.address.to_string(),
                i.ctid,
                i.storage.clone(),
            )
        })
        .collect();
    assert_eq!(
        instances,
        [
            (
                "api1".into(),
                "pve1".into(),
                "192.0.2.97".into(),
                2097,
                "local-lvm".into()
            ),
            (
                "api2".into(),
                "pve2".into(),
                "192.0.2.98".into(),
                2098,
                "local-lvm".into()
            ),
            (
                "api3".into(),
                "pve3".into(),
                "192.0.2.99".into(),
                2099,
                "local-lvm".into()
            ),
        ]
    );
    assert_eq!(config.domain, "hs.example.net");
    assert!(
        config.image.starts_with("ghcr.io/hipotures/homestack-server:0.1.0-"),
        "{}",
        config.image
    );
    assert_eq!(config.template_storage, "local");
    assert_eq!(config.pool, "homestack-service");
    assert_eq!(
        (config.bridge.as_str(), config.cidr, config.gateway.to_string()),
        ("vmbr0", 24, "192.0.2.1".to_owned())
    );
    assert_eq!(
        config.nameserver.map(|ip| ip.to_string()).as_deref(),
        Some("192.0.2.53")
    );
    let endpoints: Vec<String> = config.pve.endpoints.iter().map(ToString::to_string).collect();
    assert_eq!(
        endpoints,
        [
            "https://pve1.example.net:8006/",
            "https://pve2.example.net:8006/",
            "https://pve3.example.net:8006/"
        ]
    );
    let dir = cluster.token_dir();
    assert_eq!(config.pve.deploy_token_file, dir.join("pve-token-deploy"));
    assert_eq!(config.server.pve_token_file, dir.join("pve-token"));
    assert_eq!(config.server.api_token_file, dir.join("api-token"));
    assert_eq!(config.server.pool, "homestack");
    assert_eq!(config.acme.zone, "example.net");
    assert_eq!(config.acme.ovh_file, Some(dir.join("ovh-dns")));
    assert_eq!(config.acme.spaceship_file, None);
    assert_eq!(config.acme.dir, dir.join("acme"));

    // The server's configuration is the old file's, with the Gold installed.
    let server_toml = std::fs::read_to_string(&config.server.config_file).expect("server.toml");
    assert!(server_toml.contains("gold_vmid = 101"), "{server_toml}");
    // The client's: the old file's, with the servers.
    let client_toml = std::fs::read_to_string(dir.join("client.toml")).expect("client.toml");
    assert!(
        client_toml.contains(&url) && client_toml.contains("[workspace_ssh]"),
        "{client_toml}"
    );
    for name in ["client.toml", "deploy.toml", "server.toml"] {
        let mode = std::fs::metadata(dir.join(name)).expect(name).permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    let written = DeployConfig::load(&dir.join("deploy.toml")).expect("deploy.toml reads");
    assert_eq!(written.instances.len(), 3);

    // The last thing shown is the status the new client gets.
    assert!(out.contains("homestack status"), "{out}");
    assert!(out.contains("pve-example-1"), "no status:\n{out}");
}

#[tokio::test]
async fn a_server_name_that_does_not_resolve_stops_before_any_change() {
    let cluster = Cluster::new(&["pve1", "pve2"]);
    desktop_files(&cluster);
    let servers = Arc::new(FakeServers::resolving(&[("api1.hs.example.net", "192.0.2.97")]));
    let refused = run_with(
        &cluster,
        &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    assert_eq!(refused.code, 1);
    let out = format!("{}{}", refused.console.out(), refused.console.err());
    assert!(out.contains("api2.hs.example.net does not resolve"), "{out}");
    assert_eq!(cluster.changes(), 0, "something changed");
    assert!(servers.deployed().is_empty());
}

/// A server domain that is itself the zone, served by Spaceship.
fn at_spaceship() -> FakeServers {
    FakeServers {
        zone: Some(("homestack.example".to_owned(), DnsHost::Spaceship)),
        ..FakeServers::resolving(&[
            ("api1.homestack.example", "192.0.2.97"),
            ("api2.homestack.example", "192.0.2.98"),
            ("api3.homestack.example", "192.0.2.99"),
        ])
    }
}

#[tokio::test]
#[ignore = "slow (owner, 2026-10-15): run with cargo test -- --ignored"]
async fn a_domain_at_spaceship_gets_its_certificate_through_spaceship() {
    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    let dir = cluster.token_dir();
    std::fs::write(
        dir.join("spaceship-dns"),
        "SPACESHIP_API_KEY=k\nSPACESHIP_API_SECRET=s\n",
    )
    .expect("key");
    std::fs::set_permissions(dir.join("spaceship-dns"), std::fs::Permissions::from_mode(0o600)).expect("mode");
    let servers = Arc::new(at_spaceship());
    let done = run_with(
        &cluster,
        &server_argv(&["--server-domain", "homestack.example", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    let out = format!("{}{}", done.console.out(), done.console.err());
    assert!(out.contains("DNS at Spaceship, zone homestack.example"), "{out}");
    let deployed = servers.deployed();
    assert_eq!(deployed.len(), 1, "{out}");
    let config = &deployed[0];
    assert_eq!(config.domain, "homestack.example");
    assert_eq!(config.acme.zone, "homestack.example");
    assert_eq!(config.acme.spaceship_file, Some(dir.join("spaceship-dns")));
    assert_eq!(config.acme.ovh_file, None);
    assert_eq!(
        config.url(&config.instances[0]).expect("url").as_str(),
        "https://api1.homestack.example/"
    );
}

#[tokio::test]
async fn a_domain_at_spaceship_without_its_key_stops_before_any_change() {
    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    let servers = Arc::new(at_spaceship());
    let refused = run_with(
        &cluster,
        &server_argv(&["--server-domain", "homestack.example", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    assert_eq!(refused.code, 1);
    let out = format!("{}{}", refused.console.out(), refused.console.err());
    assert!(
        out.contains("spaceship-dns (SPACESHIP_API_KEY, SPACESHIP_API_SECRET)"),
        "{out}"
    );
    assert_eq!(cluster.changes(), 0, "something changed");
    assert!(servers.deployed().is_empty());
}

#[tokio::test]
async fn a_domain_whose_dns_is_elsewhere_stops_before_any_change() {
    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    let servers = Arc::new(FakeServers {
        zone: Some((
            "homestack.example".to_owned(),
            DnsHost::Other("ns1.elsewhere.example".to_owned()),
        )),
        ..at_spaceship()
    });
    let refused = run_with(
        &cluster,
        &server_argv(&["--server-domain", "homestack.example", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    assert_eq!(refused.code, 1);
    let out = format!("{}{}", refused.console.out(), refused.console.err());
    assert!(
        out.contains("the DNS of homestack.example is at ns1.elsewhere.example"),
        "{out}"
    );
    assert_eq!(cluster.changes(), 0, "something changed");
    assert!(servers.deployed().is_empty());
}

#[tokio::test]
async fn without_the_old_configuration_the_install_stops_before_any_change() {
    let cluster = Cluster::new(&["pve1"]);
    let servers = Arc::new(FakeServers::resolving(&[("api1.hs.example.net", "192.0.2.97")]));
    let refused = run_with(
        &cluster,
        &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    assert_eq!(refused.code, 1);
    let out = format!("{}{}", refused.console.out(), refused.console.err());
    assert!(out.contains("config.toml"), "{out}");
    assert_eq!(cluster.changes(), 0);
}

#[tokio::test]
async fn the_manual_way_needs_a_server_domain_or_no_servers() {
    let cluster = Cluster::new(&["pve1"]);
    let refused = run(&cluster, &server_argv(&["--yes"]), &[], false).await;
    assert_eq!(refused.code, 1);
    let out = refused.console.out();
    assert!(out.contains("--server-domain") && out.contains("--no-servers"), "{out}");
    assert_eq!(cluster.changes(), 0);
}

#[tokio::test]
async fn the_server_on_its_number_is_replaced_and_a_status_that_fails_fails_the_install() {
    let cluster = Cluster::new(&["pve1"]);
    desktop_files(&cluster);
    cluster.write(
        "vms.tsv",
        "100\tpve1\tdesktop\t\tvirtio=BC:24:11:00:00:02,bridge=vmbr0\t\n\
         101\tpve1\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\t\n\
         2097\tpve1\tapi1\t\t\t\tlxc\n",
    );
    cluster.write("pools.tsv", "homestack-service\t2097\tHomeStack servers\n");
    let servers = Arc::new(FakeServers::resolving(&[("api1.hs.example.net", "192.0.2.97")]));
    let done = run_with(
        &cluster,
        &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    let deployed = servers.deployed();
    assert_eq!(deployed.len(), 1, "{}", done.console.out());
    assert_eq!(deployed[0].instances[0].ctid, 2097);
    assert!(
        done.console.out().contains("CT 2097  local-lvm  (replaced)"),
        "{}",
        done.console.out()
    );
    // Nothing answers at the servers' address: the install is not done.
    assert_eq!(done.code, 1, "{}", done.console.out());
    let out = format!("{}{}", done.console.out(), done.console.err());
    assert!(out.contains("homestack status"), "{out}");
    assert!(cluster.token_dir().join("client.toml").exists());
}

#[tokio::test]
async fn a_server_number_that_is_taken_left_over_or_too_low_stops_before_any_change() {
    // The number is the last two octets of the address (owner's decision of
    // 2026-10-14): 192.0.2.97 is CT 2097.
    let cases = [
        (
            "2097\tpve1\tdesktop\t\tvirtio=BC:24:11:00:00:03,bridge=vmbr0\t\n",
            "",
            "192.0.2.97",
            "CT 2097 for api1.hs.example.net is taken by desktop",
        ),
        (
            "120\tpve1\tapi1\t\t\t\tlxc\n",
            "homestack-service\t120\tHomeStack servers\n",
            "192.0.2.97",
            "api1 is CT 120, but its address gives CT 2097: remove CT 120 first",
        ),
        (
            "",
            "",
            "10.0.0.5",
            "api1.hs.example.net is 10.0.0.5, which gives CT 5; Proxmox numbers start at 100",
        ),
        (
            "",
            "",
            "198.51.100.7",
            "api1.hs.example.net is 198.51.100.7, outside the network 192.0.2.0/24 of the gateway 192.0.2.1",
        ),
    ];
    for (vms, pools, address, expected) in cases {
        let cluster = Cluster::new(&["pve1"]);
        desktop_files(&cluster);
        cluster.write(
            "vms.tsv",
            &format!("101\tpve1\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\t\n{vms}"),
        );
        if !pools.is_empty() {
            cluster.write("pools.tsv", pools);
        }
        let servers = Arc::new(FakeServers::resolving(&[("api1.hs.example.net", address)]));
        let refused = run_with(
            &cluster,
            &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
            &[],
            false,
            Arc::clone(&servers),
        )
        .await;
        let out = format!("{}{}", refused.console.out(), refused.console.err());
        assert_eq!(refused.code, 1, "{expected}: {out}");
        assert!(out.contains(expected), "{expected}: {out}");
        assert_eq!(cluster.changes(), 0, "{expected}: something changed");
        assert!(servers.deployed().is_empty(), "{expected}");
    }
}

#[tokio::test]
async fn a_failed_deployment_writes_no_client_configuration() {
    let cluster = Cluster::new(&["pve1"]);
    desktop_files(&cluster);
    let servers = Arc::new(FakeServers {
        fail: Some("api1: no answer over TLS within 180 s".to_owned()),
        ..FakeServers::resolving(&[("api1.hs.example.net", "192.0.2.97")])
    });
    let done = run_with(
        &cluster,
        &server_argv(&["--server-domain", "hs.example.net", "--yes"]),
        &[],
        false,
        servers,
    )
    .await;
    assert_eq!(done.code, 1);
    let out = format!("{}{}", done.console.out(), done.console.err());
    assert!(out.contains("no answer over TLS"), "{out}");
    assert!(!cluster.token_dir().join("client.toml").exists());
}

#[tokio::test]
async fn the_form_asks_the_server_domain_and_the_confirmation_lists_the_servers() {
    let cluster = Cluster::new(&["pve1"]);
    desktop_files(&cluster);
    let servers = Arc::new(FakeServers::resolving(&[("api1.hs.example.net", "192.0.2.97")]));
    // 6 = Server domain; then install from the form and decline.
    let declined = run_with(
        &cluster,
        &["install", "--ssh", "root@pve1"],
        &["6", "hs.example.net", "", "n"],
        true,
        Arc::clone(&servers),
    )
    .await;
    let out = declined.console.out();
    let form = out.split("Confirm the installation").next().unwrap_or_default();
    assert!(form.contains("Server domain"), "{form}");
    let confirm = out.split("Confirm the installation").nth(1).unwrap_or_default();
    assert!(confirm.contains("Servers"), "{confirm}");
    let line = confirm
        .lines()
        .find(|l| l.contains("api1.hs.example.net"))
        .unwrap_or_default();
    for part in ["192.0.2.97", "pve1", "CT 2097", "local-lvm"] {
        assert!(line.contains(part), "{part:?} missing in {line:?}:\n{confirm}");
    }
    assert!(confirm.contains("Then: homestack status"), "{confirm}");
    assert_eq!(cluster.changes(), 0);
    assert!(servers.deployed().is_empty());
    // The domain is kept for the next run.
    let draft = std::fs::read_to_string(cluster.token_dir().join("install-draft.toml")).expect("draft");
    assert!(draft.contains("hs.example.net"), "{draft}");
}

#[tokio::test]
async fn the_servers_put_workspace_disks_on_the_storages_chosen_in_the_form() {
    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    cluster.write(
        "storage.json",
        r#"[{"storage":"local","type":"dir","content":"iso,vztmpl,backup,snippets"},
 {"storage":"fast-roots","type":"lvmthin","content":"images,rootdir","nodes":"pve1"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir","nodes":"pve2,pve3"}]
"#,
    );
    let servers = Arc::new(FakeServers::resolving(&[
        ("api1.hs.example.net", "192.0.2.97"),
        ("api2.hs.example.net", "192.0.2.98"),
        ("api3.hs.example.net", "192.0.2.99"),
    ]));
    let done = run_with(
        &cluster,
        &[
            "install",
            "--ssh",
            "root@pve1",
            "--storages",
            "fast-roots,local-lvm",
            "--snippet-storage",
            "local",
            "--service-storages",
            "fast-roots,local-lvm",
            "--template-storage",
            "local",
            "--server-domain",
            "hs.example.net",
            "--yes",
        ],
        &[],
        false,
        Arc::clone(&servers),
    )
    .await;
    let deployed = servers.deployed();
    assert_eq!(deployed.len(), 1, "{}{}", done.console.out(), done.console.err());
    let server_toml = std::fs::read_to_string(&deployed[0].server.config_file).expect("server.toml");
    for line in [
        "homestack-storage-1 = [\"fast-roots\"]",
        "homestack-storage-2 = [\"local-lvm\"]",
        "homestack-storage-3 = [\"local-lvm\"]",
    ] {
        assert!(server_toml.contains(line), "{line:?} missing:\n{server_toml}");
    }
    assert!(
        !server_toml.contains("example-storage"),
        "the old file's layouts are kept:\n{server_toml}"
    );
}

/// The next event of the TUI's install worker, waited for; what it is busy
/// with is skipped.
fn next_event(events: &std::sync::mpsc::Receiver<hs_cli::tui::install::Event>) -> hs_cli::tui::install::Event {
    loop {
        match events
            .recv_timeout(Duration::from_secs(30))
            .expect("an event from the worker")
        {
            hs_cli::tui::install::Event::Busy(_) => {}
            event => return event,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow (owner, 2026-10-15): run with cargo test -- --ignored"]
async fn the_tui_worker_installs_through_the_same_session_as_the_cli() {
    use hs_cli::tui::install::{Event, Method, Mode};
    use hs_cli::tui::installing::{self, Command, Setup};

    let cluster = Cluster::new(&["pve1", "pve2", "pve3"]);
    desktop_files(&cluster);
    let (url, ca_pem, _sim) = status_server().await;
    let ca = cluster.token_dir().join("server-ca.pem");
    std::fs::write(&ca, ca_pem).expect("ca");
    let servers = Arc::new(FakeServers {
        reach: Some((url.clone(), ca.display().to_string())),
        ..FakeServers::resolving(&[
            ("api1.hs.example.net", "192.0.2.97"),
            ("api2.hs.example.net", "192.0.2.98"),
            ("api3.hs.example.net", "192.0.2.99"),
        ])
    });
    let client_path = cluster.token_dir().join("client.toml");
    let setup = Setup {
        mode: Mode::Install,
        system: Arc::new(cluster.system()),
        home: cluster.home.clone(),
        token_dir: cluster.token_dir(),
        client_path: client_path.clone(),
        servers: Arc::clone(&servers) as Arc<dyn Servers>,
        sounds: hs_cli::touch::Sounds::disabled(),
        ssh_prefix: Vec::new(),
        server_image: None,
        handle: tokio::runtime::Handle::current(),
    };
    let (commands, receiver) = std::sync::mpsc::channel();
    let (sender, events) = std::sync::mpsc::channel();
    installing::spawn(setup, receiver, move |event| {
        let _ = sender.send(event);
    });

    commands
        .send(Command::Connect {
            method: Method::Ssh,
            target: "root@pve1".to_owned(),
        })
        .unwrap();
    let found = match next_event(&events) {
        Event::Connected(found) => found,
        other => panic!("{other:?}"),
    };
    assert!(found.root.starts_with("ssh root@pve1 · PVE "), "{}", found.root);
    assert!(
        found.check.iter().any(|(_, label, _)| label == "Proxmox"),
        "{:?}",
        found.check
    );
    assert!(!found.settings.is_empty(), "nothing to choose: {found:?}");
    assert_eq!(cluster.changes(), 0, "connecting changed something");

    let answers: Vec<hs_cli::wizard::Answer> = found
        .settings
        .iter()
        .map(|setting| hs_cli::wizard::Answer::Picked(setting.picked.clone()))
        .collect();
    commands
        .send(Command::Review {
            answers,
            domain: Some("hs.example.net".to_owned()),
        })
        .unwrap();
    match next_event(&events) {
        Event::Reviewed { sections, problems } => {
            assert!(problems.is_empty(), "{problems:?}");
            assert!(sections.iter().any(|(heading, _)| heading == "Servers"), "{sections:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(cluster.changes(), 0, "the review changed something");

    commands.send(Command::Apply).unwrap();
    let mut lines = Vec::new();
    let (ok, result) = loop {
        match next_event(&events) {
            Event::Line(line) => lines.push(line),
            Event::Done { ok, lines } => break (ok, lines),
            other => panic!("{other:?}"),
        }
    };
    assert!(ok, "{result:?}\n{lines:?}");
    assert!(lines.iter().any(|line| line.contains("homestack@pve")), "{lines:?}");
    assert!(cluster.read("users").contains("homestack@pve"));
    assert_eq!(servers.deployed().len(), 1);
    assert!(client_path.exists(), "client.toml was not written");
    assert!(
        result
            .iter()
            .any(|line| line.starts_with("Check: the client reaches the servers")),
        "{result:?}"
    );
    commands.send(Command::Cancel).unwrap();
}

#[tokio::test]
async fn a_proxmox_template_is_no_gold_to_choose() {
    let cluster = Cluster::new(&["pve1"]);
    cluster.write(
        "vms.tsv",
        "101\tpve1\tvm-a\t\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\t\n\
         102\tpve1\tvm-b\t\tvirtio=BC:24:11:00:00:02,bridge=vmbr0\t\n\
         105\tpve1\tbase-template\t\tvirtio=BC:24:11:00:00:05,bridge=vmbr0\t\ttemplate\n",
    );
    let json = run(
        &cluster,
        &install_argv(&["--ssh", "root@pve1", "--json"], &[]),
        &[],
        false,
    )
    .await;
    let out = json.json();
    let gold = out["missing"]
        .as_array()
        .and_then(|missing| missing.iter().find(|question| question["option"] == "--gold"))
        .cloned()
        .unwrap_or_else(|| panic!("no --gold question: {out}"));
    let values: Vec<&str> = gold["choices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|choice| choice["value"].as_str())
        .collect();
    assert_eq!(values, ["101", "102"], "{out}");
    assert_eq!(cluster.changes(), 0);
}
