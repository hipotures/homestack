//! `homestack install` and `homestack uninstall` (phase 13c): the command
//! line, the plan and its confirmation, the exit codes and `--json`, with the
//! root access over SSH and through a Herdr pane, both answered by a local
//! shell that sees the fake Proxmox tools of `deploy/pve/tests/fake-pve`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{FakeSystem, Recorded, strings};
use hs_cli::app::{self, Context};
use hs_cli::args::{self, Command};
use hs_cli::system::Output;
use hs_cli::view::Style;
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
    let mut argv = vec!["install"];
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
    assert!(args::parse(&strings(&["install"])).is_ok());
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
    let text = run(&cluster, &["install", "--ssh", "root@pve1"], &[], false).await;
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

    let json = run(&cluster, &["install", "--ssh", "root@pve1", "--json"], &[], false).await;
    assert_eq!(json.code, 1);
    let out = json.json();
    assert_eq!(out["missing"][0]["option"], "--storages", "{out}");
    assert_eq!(out["missing"][0]["defaults"][0], "local-lvm", "{out}");
}

#[tokio::test]
async fn on_a_terminal_the_choices_end_in_a_summary_and_nothing_happens_before_install() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    // Enter keeps the ticked storage; the summary is declined.
    let declined = run(&cluster, &["install", "--ssh", "root@pve1"], &["", "n"], true).await;
    assert_eq!(declined.code, 0, "{}", declined.console.out());
    assert!(declined.console.out().contains("Cancelled. No changes were made."));
    assert_eq!(cluster.changes(), 0, "something changed before Install");

    let done = run(&cluster, &["install", "--ssh", "root@pve1"], &["", "y"], true).await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    let out = done.console.out();
    for text in [
        "Storages for the workspace disks",
        "Summary",
        "Your choices",
        "Workspace disks:     local-lvm",
        "Chosen without asking",
        "Changes in Proxmox",
        "create user homestack@pve",
        "Same as: homestack install --ssh root@pve1 --storages local-lvm --snippet-storage local --service-storages local-lvm --template-storage local",
    ] {
        assert!(out.contains(text), "{text:?} missing:\n{out}");
    }
    let lists = out.split("Summary").next().unwrap_or_default();
    assert!(
        !lists.contains("--"),
        "a command-line option is shown in the lists:\n{lists}"
    );
    assert!(cluster.read("users").contains("homestack@pve"), "not installed");
}

#[tokio::test]
async fn a_single_possibility_is_not_asked_and_options_given_are_not_asked_again() {
    // Every option has one possible storage: only the summary is asked.
    let cluster = Cluster::new(&["pve1"]);
    let done = run(&cluster, &["install", "--ssh", "root@pve1"], &["y"], true).await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    assert_eq!(done.console.questions(), ["Install? [y/n] (n): "]);

    // A given option is not asked even when there is a choice.
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    let done = run(
        &cluster,
        &["install", "--ssh", "root@pve1", "--storages", "backup-a"],
        &["y"],
        true,
    )
    .await;
    assert_eq!(done.code, 0, "{}", done.console.out());
    assert_eq!(done.console.questions(), ["Install? [y/n] (n): "]);
    assert!(
        done.console.out().contains("Workspace disks:     backup-a"),
        "{}",
        done.console.out()
    );
}

#[tokio::test]
async fn giving_up_changes_nothing() {
    let cluster = Cluster::new(&["pve1"]);
    two_disk_storages(&cluster);
    let gave_up = run(&cluster, &["install", "--ssh", "root@pve1"], &[], true).await;
    assert_eq!(gave_up.code, 0, "{}", gave_up.console.out());
    assert!(gave_up.console.out().contains("Cancelled"), "{}", gave_up.console.out());
    assert_eq!(cluster.changes(), 0);
}

// --- the default installer asks how to reach root --------------------------

#[tokio::test]
async fn without_an_access_option_the_installer_asks_for_one_and_goes_on() {
    let cluster = Cluster::new(&["pve1"]);
    // 1 = SSH, the node, then the summary.
    let done = run(&cluster, &["install"], &["1", "root@pve1", "y"], true).await;
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
    // 2 = Herdr, the only tab, then the summary.
    let done = run(&cluster, &["install"], &["2", "1", "y"], true).await;
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
    let refused = run(&cluster, &["install", "--storages", "local-lvm"], &[], false).await;
    assert_eq!(refused.code, 1);
    let out = refused.console.out();
    assert!(
        out.contains("--ssh") && out.contains("--herdr") && out.contains("terminal"),
        "{out}"
    );
    assert_eq!(cluster.changes(), 0);

    let gave_up = run(&cluster, &["install"], &[], true).await;
    assert_eq!(gave_up.code, 0, "{}", gave_up.console.out());
    assert!(gave_up.console.out().contains("Cancelled"), "{}", gave_up.console.out());
}
