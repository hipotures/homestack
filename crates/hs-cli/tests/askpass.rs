//! ssh's questions reach the TUI (phase 13b step 3): ssh runs `homestack`
//! as its `SSH_ASKPASS`, which asks through the bridge's socket.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use hs_cli::askpass::{Bridge, Event, Kind, SOCKET_ENV, kind_of};

fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_homestack"))
}

fn next(events: &Receiver<Event>) -> Event {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("an event from the bridge")
}

/// Run `homestack` as ssh runs its askpass: the prompt as the argument.
fn askpass(bridge: &Bridge, hint: Option<&str>, prompt: &str) -> std::process::Child {
    let mut command = Command::new(program());
    command
        .arg(prompt)
        .env(SOCKET_ENV, bridge.socket())
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    if let Some(hint) = hint {
        command.env("SSH_ASKPASS_PROMPT", hint);
    }
    command.spawn().expect("homestack as askpass")
}

#[test]
fn the_kind_of_question_comes_from_ssh_s_hint_and_prompt() {
    assert_eq!(
        kind_of(Some("none"), "Confirm user presence for key ECDSA-SK SHA256:abc"),
        Kind::Touch
    );
    assert_eq!(
        kind_of(
            None,
            "The authenticity of host 'pve2 (192.0.2.11)' can't be established.\n\
             ED25519 key fingerprint is SHA256:xyz.\n\
             Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ),
        Kind::HostKey
    );
    assert_eq!(kind_of(Some("confirm"), "Allow use of key?"), Kind::HostKey);
    assert_eq!(
        kind_of(None, "Enter PIN for ECDSA-SK key /home/u/.ssh/id_sk: "),
        Kind::Secret
    );
    assert_eq!(
        kind_of(None, "Enter passphrase for key '/home/u/.ssh/id': "),
        Kind::Secret
    );
}

#[test]
fn an_answer_goes_back_to_ssh_and_a_cancel_fails_the_askpass() {
    let (bridge, events) = Bridge::start(program()).expect("bridge");
    let mut child = askpass(&bridge, None, "Enter PIN for ECDSA-SK key /home/u/.ssh/id_sk: ");
    match next(&events) {
        Event::Ask(question) => {
            assert_eq!(question.kind, Kind::Secret);
            assert!(question.prompt.starts_with("Enter PIN"), "{}", question.prompt);
            question.answer(Some("1234".to_owned()));
        }
        Event::Ended(_) => panic!("ended before the answer"),
    }
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(out, "1234\n");

    let mut child = askpass(&bridge, None, "Enter passphrase for key '/home/u/.ssh/id': ");
    match next(&events) {
        Event::Ask(question) => question.answer(None),
        Event::Ended(_) => panic!("ended before the answer"),
    }
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(!child.wait().unwrap().success(), "a cancel must fail the askpass");
    assert_eq!(out, "");
}

#[test]
fn a_touch_notice_ends_when_ssh_stops_it() {
    let (bridge, events) = Bridge::start(program()).expect("bridge");
    let mut child = askpass(
        &bridge,
        Some("none"),
        "Confirm user presence for key ECDSA-SK SHA256:abc",
    );
    let id = match next(&events) {
        Event::Ask(question) => {
            assert_eq!(question.kind, Kind::Touch);
            question.id
        }
        Event::Ended(_) => panic!("ended before it was shown"),
    };
    // ssh's notify_complete: SIGTERM to the notifier.
    child.kill().unwrap();
    child.wait().unwrap();
    match next(&events) {
        Event::Ended(ended) => assert_eq!(ended, id),
        Event::Ask(_) => panic!("a second question"),
    }
}

#[test]
fn ssh_run_through_the_prefix_asks_through_the_bridge_without_a_terminal() {
    let (bridge, events) = Bridge::start(program()).expect("bridge");
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    // What ssh does with a security key that needs a PIN and a touch.
    let script = format!(
        "pin=$(\"$SSH_ASKPASS\" 'Enter PIN for ECDSA-SK key /k: ') || exit 7\n\
         SSH_ASKPASS_PROMPT=none \"$SSH_ASKPASS\" 'Confirm user presence for key ECDSA-SK SHA256:abc' &\n\
         notifier=$!\n\
         sleep 0.3\n\
         kill $notifier\n\
         if tty -s; then t=tty; else t=none; fi\n\
         printf '%s %s %s\\n' \"$pin\" \"$SSH_ASKPASS_REQUIRE\" \"$t\" > {}\n",
        out.display()
    );
    let mut argv = bridge.command_prefix();
    argv.extend(["sh".to_owned(), "-c".to_owned(), script]);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    match next(&events) {
        Event::Ask(question) => {
            assert_eq!(question.kind, Kind::Secret);
            question.answer(Some("4321".to_owned()));
        }
        Event::Ended(_) => panic!("ended before the PIN"),
    }
    match next(&events) {
        Event::Ask(question) => assert_eq!(question.kind, Kind::Touch),
        Event::Ended(_) => panic!("ended before the touch"),
    }
    assert!(matches!(next(&events), Event::Ended(_)));
    assert!(child.wait().unwrap().success());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "4321 force none\n");
}

#[test]
fn a_cancelled_touch_stops_ssh_waiting_on_the_key() {
    let (bridge, events) = Bridge::start(program()).expect("bridge");
    // ssh waits on the key, not on its touch notifier.
    let script = "SSH_ASKPASS_PROMPT=none \"$SSH_ASKPASS\" 'Confirm user presence for key ECDSA-SK SHA256:abc' &\n\
                  sleep 30\n";
    let mut argv = bridge.command_prefix();
    argv.extend(["sh".to_owned(), "-c".to_owned(), script.to_owned()]);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    match next(&events) {
        Event::Ask(question) => {
            assert_eq!(question.kind, Kind::Touch);
            question.answer(None);
        }
        Event::Ended(_) => panic!("ended before the touch"),
    }
    assert!(matches!(next(&events), Event::Ended(_)));
    let started = std::time::Instant::now();
    assert!(!child.wait().unwrap().success());
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// Esc while the master connects: ssh, the leader of its own group behind
/// `setsid`, stops with what it started.
#[test]
fn a_stopped_master_stops_at_once_with_its_group() {
    use hs_cli::system::{RealSystem, System};
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let argv: Vec<String> = ["setsid", "sh", "-c", "sleep 30 & sleep 30"]
        .map(str::to_owned)
        .to_vec();
    let started = std::time::Instant::now();
    let output = RealSystem::default().run_master_until(&argv, &stop).unwrap();
    assert_eq!(output.code, -1);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_socket_goes_with_the_bridge() {
    let (bridge, _events) = Bridge::start(program()).expect("bridge");
    let socket = bridge.socket().to_path_buf();
    assert!(socket.exists());
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(socket.parent().unwrap()).unwrap().permissions())
            & 0o777;
    assert_eq!(mode, 0o700, "only this user may ask");
    drop(bridge);
    assert!(!socket.exists());
}
