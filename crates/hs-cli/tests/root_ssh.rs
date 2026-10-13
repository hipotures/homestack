//! The SSH root access (`homestack install --ssh USER@HOST`, phase 13c)
//! against a fake `ssh`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hs_cli::root_ssh::{self, SshRoot};
use hs_cli::system::{Output, System};
use hs_cli::touch::Sounds;
use hs_install::Root;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    masters: Mutex<Vec<Vec<String>>>,
    /// What the command run through the master answers.
    answer: Mutex<Option<Output>>,
}

impl System for Fake {
    fn which(&self, name: &str) -> Option<String> {
        (name == "ssh").then(|| "/usr/bin/ssh".to_owned())
    }

    fn run(&self, argv: &[String]) -> io::Result<Output> {
        self.calls.lock().expect("calls").push(argv.to_vec());
        if argv.iter().any(|word| word == "-O") {
            return Ok(Output::stdout(""));
        }
        Ok(self
            .answer
            .lock()
            .expect("answer")
            .clone()
            .unwrap_or_else(|| Output::stdout("")))
    }

    fn run_master(&self, argv: &[String]) -> io::Result<Output> {
        self.masters.lock().expect("masters").push(argv.to_vec());
        Ok(Output::stdout(""))
    }

    fn monotonic(&self) -> f64 {
        0.0
    }
}

fn open(fake: &Arc<Fake>, text: &str, identity: Option<&str>) -> SshRoot {
    SshRoot::open(
        fake.clone(),
        Path::new("/home/nobody"),
        text,
        identity,
        &Sounds::disabled(),
    )
    .expect("open")
}

#[test]
fn the_target_is_user_at_host_and_a_bare_host_is_root() {
    assert_eq!(root_ssh::target("root@pve1").expect("target"), "root@pve1");
    assert_eq!(root_ssh::target(" pve1 ").expect("target"), "root@pve1");
    assert_eq!(
        root_ssh::target("admin@192.0.2.10").expect("target"),
        "admin@192.0.2.10"
    );
    for bad in [
        "",
        "-oProxyCommand=x",
        "root@-x",
        "@pve1",
        "root@",
        "a b@c",
        "root@pve1;rm",
    ] {
        assert!(root_ssh::target(bad).is_err(), "{bad:?} was accepted");
    }
}

#[test]
fn one_master_is_opened_and_commands_go_through_it() {
    let fake = Arc::new(Fake::default());
    *fake.answer.lock().expect("answer") = Some(Output {
        code: 3,
        stdout: "out\n".to_owned(),
        stderr: "err\n".to_owned(),
    });
    {
        let mut root = open(&fake, "root@pve1", None);
        assert_eq!(fake.masters.lock().expect("masters").len(), 1);
        let master = fake.masters.lock().expect("masters")[0].join(" ");
        assert!(master.contains("-M") && master.ends_with("root@pve1"), "{master}");
        let output = root.run("pvesh get /version").expect("run");
        assert_eq!(
            (output.code, output.stdout.as_str(), output.stderr.as_str()),
            (3, "out\n", "err\n")
        );
        let argv = fake.calls.lock().expect("calls").last().expect("a call").clone();
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|word| word == "-S"), "not through the master: {argv:?}");
        assert!(argv.iter().any(|word| word == "BatchMode=yes"), "{argv:?}");
        assert_eq!(&argv[argv.len() - 2..], ["root@pve1", "pvesh get /version"]);
        assert_eq!(root.describe(), "ssh root@pve1");
    }
    // The master is stopped when the access is dropped.
    let last = fake.calls.lock().expect("calls").last().expect("a call").clone();
    assert!(last.windows(2).any(|pair| pair == ["-O", "exit"]), "{last:?}");
}

#[test]
fn an_identity_file_is_the_only_key_offered() {
    let fake = Arc::new(Fake::default());
    let _root = open(&fake, "root@pve1", Some("~/.ssh/id_pve"));
    let master = fake.masters.lock().expect("masters")[0].clone();
    let at = master.iter().position(|word| word == "-i").expect("-i");
    assert_eq!(master[at + 1], "/home/nobody/.ssh/id_pve");
    assert!(
        master.windows(2).any(|pair| pair == ["-o", "IdentitiesOnly=yes"]),
        "{master:?}"
    );
}

#[test]
fn a_failure_of_ssh_itself_is_an_error_with_its_reason() {
    let fake = Arc::new(Fake::default());
    let mut root = open(&fake, "root@pve1", None);
    *fake.answer.lock().expect("answer") = Some(Output::failed(
        255,
        "ssh: connect to host pve1 port 22: Connection timed out\n",
    ));
    let error = root.run("true").expect_err("an error");
    assert!(error.to_string().contains("Connection timed out"), "{error}");
    assert!(error.to_string().contains("root@pve1"), "{error}");
}
