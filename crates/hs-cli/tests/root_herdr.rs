//! The Herdr root access (`homestack install --herdr NAME`, phase 13c) against
//! a fake `herdr`: it knows workspaces, tabs and panes, and a pane whose
//! shell really runs what is typed into it, so the wrapping that brings the
//! output and the exit code back is exercised for real.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io;
use std::sync::{Arc, Mutex};

use hs_cli::root_herdr::{self, HerdrRoot};
use hs_cli::system::{Output, System};
use hs_install::Root;
use serde_json::{Value, json};

struct Fake {
    workspaces: Value,
    tabs: Value,
    panes: Value,
    prompt: String,
    screen: Mutex<String>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl Fake {
    fn new(tabs: &[(&str, &str, &str)], panes: &[(&str, &str, Option<&str>)]) -> Arc<Self> {
        // tabs: (workspace id, tab id, label); panes: (tab id, pane id, label)
        let prompt = "root@pve1:~# ".to_owned();
        Arc::new(Self {
            workspaces: json!({"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[
                {"label":"PVE","workspace_id":"w3"},
                {"label":"ROOT","workspace_id":"w1"},
                {"label":"OTHER","workspace_id":"w5"}]}}),
            tabs: json!({"id":"cli:tab:list","result":{"type":"tab_list","tabs": tabs.iter().map(|(workspace, tab, label)|
                json!({"workspace_id":workspace,"tab_id":tab,"label":label})).collect::<Vec<_>>()}}),
            panes: json!({"id":"cli:pane:list","result":{"type":"pane_list","panes": panes.iter().map(|(tab, pane, label)| {
                let mut value = json!({"tab_id":tab,"pane_id":pane});
                if let Some(label) = label { value["label"] = json!(label); }
                value}).collect::<Vec<_>>()}}),
            screen: Mutex::new(prompt.clone()),
            prompt,
            calls: Mutex::default(),
        })
    }

    fn standard() -> Arc<Self> {
        Self::new(
            &[("w3", "w3:t1", "pve1"), ("w3", "w3:t2", "pve2"), ("w1", "w1:t1", "1")],
            &[
                ("w3:t1", "w3:p1", None),
                ("w3:t2", "w3:p2", None),
                ("w1:t1", "w1:p1", None),
            ],
        )
    }

    fn typed(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|argv| {
                argv.get(1).map(String::as_str) == Some("pane") && argv.get(2).map(String::as_str) == Some("run")
            })
            .filter_map(|argv| argv.get(4).cloned())
            .collect()
    }
}

impl System for Fake {
    fn which(&self, name: &str) -> Option<String> {
        (name == "herdr").then(|| "/usr/bin/herdr".to_owned())
    }

    fn run(&self, argv: &[String]) -> io::Result<Output> {
        self.calls.lock().expect("calls").push(argv.to_vec());
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["herdr", "workspace", "list"] => Ok(Output::stdout(&self.workspaces.to_string())),
            ["herdr", "tab", "list"] => Ok(Output::stdout(&self.tabs.to_string())),
            ["herdr", "pane", "list"] => Ok(Output::stdout(&self.panes.to_string())),
            ["herdr", "pane", "read", _, "--source", "detection"] => {
                Ok(Output::stdout(&self.screen.lock().expect("screen").clone()))
            }
            ["herdr", "pane", "read", _, "--source", "recent-unwrapped", "--lines", _] => {
                Ok(Output::stdout(&self.screen.lock().expect("screen").clone()))
            }
            ["herdr", "pane", "run", _, text] => {
                let mut screen = self.screen.lock().expect("screen");
                if text.starts_with("printf '\\033[3J") {
                    *screen = self.prompt.clone();
                    return Ok(Output::stdout("{}"));
                }
                let ran = std::process::Command::new("bash").arg("-c").arg(text).output()?;
                screen.push_str(text);
                screen.push('\n');
                screen.push_str(&String::from_utf8_lossy(&ran.stdout));
                screen.push_str(&self.prompt);
                Ok(Output::stdout("{}"))
            }
            ["herdr", "pane", "wait-output", _, "--match", marker, ..] => {
                if self.screen.lock().expect("screen").contains(marker) {
                    Ok(Output::stdout("{}"))
                } else {
                    Ok(Output::failed(1, "timeout"))
                }
            }
            other => Err(io::Error::other(format!("unexpected herdr call {other:?}"))),
        }
    }

    fn run_master(&self, _argv: &[String]) -> io::Result<Output> {
        Err(io::Error::other("no master"))
    }

    fn monotonic(&self) -> f64 {
        0.0
    }
}

#[test]
fn a_command_comes_back_with_its_output_error_and_exit_code() {
    let fake = Fake::standard();
    let mut root = HerdrRoot::open(fake.clone(), "pve1").expect("open");
    assert_eq!(root.pane(), "w3:p1");
    let output = root.run("echo out; echo err >&2; exit 3").expect("run");
    assert_eq!(
        (output.code, output.stdout.as_str(), output.stderr.as_str()),
        (3, "out\n", "err\n")
    );
    let output = root.run("printf 'a b\\tc\\n\\nd'").expect("run");
    assert_eq!((output.code, output.stdout.as_str()), (0, "a b\tc\n\nd"));
}

#[test]
fn a_long_answer_with_odd_bytes_comes_back_whole() {
    let fake = Fake::standard();
    let mut root = HerdrRoot::open(fake, "pve1").expect("open");
    let output = root
        .run("i=0; while [ $i -lt 4000 ]; do printf '%05d \\033[31m{\"x\":\"é\"}\\n' $i; i=$((i+1)); done")
        .expect("run");
    assert_eq!(output.code, 0);
    assert_eq!(output.stdout.lines().count(), 4000);
    assert!(output.stdout.starts_with("00000 \u{1b}[31m{\"x\":\"é\"}\n"));
    assert!(output.stdout.ends_with("03999 \u{1b}[31m{\"x\":\"é\"}\n"));
}

#[test]
fn what_is_typed_is_one_line_that_never_holds_its_own_markers() {
    let fake = Fake::standard();
    let mut root = HerdrRoot::open(fake.clone(), "pve1").expect("open");
    root.run("true").expect("run");
    let typed = fake.typed();
    assert_eq!(typed.len(), 1);
    let line = &typed[0];
    assert!(!line.contains('\n'));
    let token = line
        .strip_prefix("hs_t=")
        .and_then(|rest| rest.split(';').next())
        .expect("a token");
    assert!(token.len() >= 8);
    for marker in ["__HS_BEGIN__", "__HS_ERR__", "__HS_END__"] {
        assert!(
            !line.contains(&format!("{marker}{token}")),
            "{marker} is spelled out in the typed line"
        );
    }
}

#[test]
fn a_command_too_long_for_a_pane_is_refused() {
    let fake = Fake::standard();
    let mut root = HerdrRoot::open(fake.clone(), "pve1").expect("open");
    let error = root.run(&format!("echo {}", "x".repeat(4000))).expect_err("refused");
    assert!(error.to_string().contains("too long"), "{error}");
    assert!(fake.typed().is_empty(), "something was typed anyway");
    let error = root.run("echo a\necho b").expect_err("refused");
    assert!(error.to_string().contains("one line"), "{error}");
}

#[test]
fn the_screen_is_cleared_after_a_secret() {
    let fake = Fake::standard();
    let mut root = HerdrRoot::open(fake.clone(), "pve1").expect("open");
    root.run("echo fake-secret-value").expect("run");
    assert!(!fake.screen.lock().expect("screen").is_empty());
    root.forget_screen();
    assert_eq!(*fake.screen.lock().expect("screen"), "root@pve1:~# ");
}

#[test]
fn no_tab_or_pane_of_that_name_is_an_error() {
    let fake = Fake::standard();
    let error = HerdrRoot::open(fake, "pve9").err().expect("an error");
    assert!(error.0.contains("no tab or pane named"), "{error}");
}

#[test]
fn several_of_the_same_name_are_listed_and_refused() {
    let fake = Fake::new(
        &[("w3", "w3:t1", "pve1"), ("w5", "w5:t1", "pve1")],
        &[("w3:t1", "w3:p1", None), ("w5:t1", "w5:p1", None)],
    );
    let error = HerdrRoot::open(fake.clone(), "pve1").err().expect("an error");
    assert!(error.0.contains("More than one"), "{error}");
    assert!(
        error.0.contains("PVE/pve1") && error.0.contains("OTHER/pve1"),
        "{error}"
    );
    assert!(error.0.contains("WORKSPACE/NAME"), "{error}");
    // Narrowed to a workspace it is one.
    let root = HerdrRoot::open(fake, "OTHER/pve1").expect("open");
    assert_eq!(root.pane(), "w5:p1");
}

#[test]
fn a_tab_with_several_panes_is_refused_but_a_named_pane_is_found() {
    let fake = Fake::new(
        &[("w3", "w3:t1", "pve1")],
        &[("w3:t1", "w3:p1", None), ("w3:t1", "w3:p9", Some("root-pve1"))],
    );
    let error = HerdrRoot::open(fake.clone(), "pve1").err().expect("an error");
    assert!(error.0.contains("2 panes"), "{error}");
    let root = HerdrRoot::open(fake, "root-pve1").expect("open");
    assert_eq!(root.pane(), "w3:p9");
}

#[test]
fn a_pane_that_is_not_at_a_root_prompt_is_refused_before_anything_is_typed() {
    let fake = Fake::standard();
    *fake.screen.lock().expect("screen") = "user@cachyos:~$ ".to_owned();
    let error = HerdrRoot::open(fake.clone(), "pve1").err().expect("an error");
    assert!(error.0.contains("not at a root shell prompt"), "{error}");
    assert!(fake.typed().is_empty());
    *fake.screen.lock().expect("screen") = "Password: ".to_owned();
    assert!(HerdrRoot::open(fake, "pve1").is_err());
}

#[test]
fn matching_is_exact() {
    let fake = Fake::standard();
    let found = root_herdr::candidates(fake.as_ref(), "pve").expect("candidates");
    assert!(found.is_empty());
    let found = root_herdr::candidates(fake.as_ref(), "pve2").expect("candidates");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].workspace, "PVE");
}

#[test]
fn single_pane_tabs_are_offered_with_their_prompt_and_filtered_by_mask() {
    let fake = Fake::standard();
    let tabs = root_herdr::tabs(fake.as_ref()).expect("tabs");
    let shown: Vec<(&str, &str)> = tabs.iter().map(|tab| (tab.name.as_str(), tab.title.as_str())).collect();
    // Single-pane tabs across workspaces are returned with their last line
    // (prompt), while multi-pane tabs are omitted.
    assert_eq!(
        shown,
        [
            ("PVE/pve1", "root@pve1:~#"),
            ("PVE/pve2", "root@pve1:~#"),
            ("ROOT/1", "root@pve1:~#"),
        ]
    );
    let default_filtered: Vec<&str> = tabs
        .iter()
        .filter(|tab| root_herdr::matches_tab(&tab.name, root_herdr::DEFAULT_TAB_FILTER))
        .map(|tab| tab.name.as_str())
        .collect();
    assert_eq!(default_filtered, ["PVE/pve1", "PVE/pve2"]);
    let root_filtered: Vec<&str> = tabs
        .iter()
        .filter(|tab| root_herdr::matches_tab(&tab.name, "root"))
        .map(|tab| tab.name.as_str())
        .collect();
    assert_eq!(root_filtered, ["ROOT/1"]);
}
