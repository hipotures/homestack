//! ssh's questions in the TUI (phase 13b step 3). ssh runs with no terminal
//! and `homestack` as its `SSH_ASKPASS` (`SSH_ASKPASS_REQUIRE=force`); run
//! that way, `homestack` sends the prompt through the bridge's Unix socket
//! and prints the answer, as an askpass program does. OpenSSH (`readpass.c`)
//! asks it for a PIN, a passphrase and an unknown host key, and shows the
//! touch of a security key through it with `SSH_ASKPASS_PROMPT=none`,
//! stopping that one with SIGTERM once the key was touched.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use serde_json::{Value, json};

/// The socket an askpass run of `homestack` asks through.
pub const SOCKET_ENV: &str = "HOMESTACK_ASKPASS_SOCKET";

/// What ssh asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Touch the security key; nothing to answer.
    Touch,
    /// Yes or no, such as an unknown host key.
    HostKey,
    /// A PIN or a passphrase, typed hidden.
    Secret,
}

/// The kind of a question from ssh's hint (`SSH_ASKPASS_PROMPT`) and prompt.
#[must_use]
pub fn kind_of(hint: Option<&str>, prompt: &str) -> Kind {
    match hint {
        Some("none") => Kind::Touch,
        Some("confirm") => Kind::HostKey,
        _ if prompt.contains("(yes/no") => Kind::HostKey,
        _ => Kind::Secret,
    }
}

/// One question; [`Question::answer`] sends the answer back to ssh.
pub struct Question {
    pub id: u64,
    pub kind: Kind,
    pub prompt: String,
    reply: Option<Sender<Option<String>>>,
    /// The touch: the process group of the askpass, ssh's (`setsid` made
    /// ssh its leader), stopped when the touch is cancelled.
    group: Option<i32>,
}

impl Question {
    /// The answer, or `None` to cancel (the askpass fails, so ssh does).
    /// A touch takes no answer; cancelled, ssh is stopped, as ssh waits on
    /// the key and not on the askpass.
    pub fn answer(mut self, answer: Option<String>) {
        let cancelled = answer.is_none();
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(answer);
        }
        if cancelled
            && let Some(group) = self.group.and_then(rustix::process::Pid::from_raw)
            // Never the TUI's own group (ssh without `setsid`).
            && group != rustix::process::getpgrp()
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
        }
    }
}

impl std::fmt::Debug for Question {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Question")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("prompt", &self.prompt)
            .finish_non_exhaustive()
    }
}

/// What the bridge tells the TUI.
#[derive(Debug)]
pub enum Event {
    Ask(Question),
    /// The question `id` is over without an answer: ssh stopped its touch
    /// notice, or the askpass went away.
    Ended(u64),
}

/// The listening end, in the TUI. Dropping it removes the socket.
pub struct Bridge {
    program: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}

impl Bridge {
    /// Listen on a new socket in a private directory; `program` is what ssh
    /// runs as its askpass (this `homestack`).
    pub fn start(program: PathBuf) -> std::io::Result<(Self, Receiver<Event>)> {
        let dir = tempfile::Builder::new().prefix("hs-askpass-").tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = dir.path().join("socket");
        let listener = UnixListener::bind(&socket)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (events, receiver) = channel();
        let ids = Arc::new(AtomicU64::new(1));
        {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let events = events.clone();
                    let id = ids.fetch_add(1, Ordering::SeqCst);
                    std::thread::spawn(move || serve(stream, id, &events));
                }
            });
        }
        Ok((
            Self {
                program,
                socket,
                stop,
                _dir: dir,
            },
            receiver,
        ))
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The words before `ssh`: its askpass is this bridge's, and `setsid`
    /// takes the terminal away, so ssh writes nothing on the TUI's screen.
    #[must_use]
    pub fn command_prefix(&self) -> Vec<String> {
        vec![
            "env".to_owned(),
            format!("SSH_ASKPASS={}", self.program.display()),
            "SSH_ASKPASS_REQUIRE=force".to_owned(),
            format!("{SOCKET_ENV}={}", self.socket.display()),
            "setsid".to_owned(),
            "-w".to_owned(),
        ]
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accepting thread so it sees the stop.
        let _ = UnixStream::connect(&self.socket);
    }
}

/// One askpass run: its question to the TUI, the answer back.
fn serve(stream: UnixStream, id: u64, events: &Sender<Event>) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut lines = BufReader::new(reader);
    let mut line = String::new();
    if lines.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let prompt = request["prompt"].as_str().unwrap_or_default().to_owned();
    let kind = kind_of(request["hint"].as_str(), &prompt);
    if kind == Kind::Touch {
        // Short-lived touch notices must not briefly open a modal. Show only
        // a sustained wait; prompts requiring answers remain immediate.
        if lines
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .is_err()
        {
            return;
        }
        let mut rest = String::new();
        match lines.read_line(&mut rest) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            _ => return,
        }
        if lines.get_ref().set_read_timeout(None).is_err() {
            return;
        }
        let group = request["group"]
            .as_i64()
            .and_then(|group| i32::try_from(group).ok())
            .filter(|group| *group > 1);
        let _ = events.send(Event::Ask(Question {
            id,
            kind,
            prompt,
            reply: None,
            group,
        }));
        // The notice lasts until ssh stops the askpass: its end of the
        // socket closes.
        while lines.read_line(&mut rest).unwrap_or(0) != 0 {
            rest.clear();
        }
        let _ = events.send(Event::Ended(id));
        return;
    }
    let (reply, answer) = channel();
    if events
        .send(Event::Ask(Question {
            id,
            kind,
            prompt,
            reply: Some(reply),
            group: None,
        }))
        .is_err()
    {
        return;
    }
    let response = match answer.recv() {
        Ok(Some(text)) => json!({"answer": text}),
        Ok(None) | Err(_) => json!({"cancel": true}),
    };
    let mut writer = stream;
    let _ = writeln!(writer, "{response}");
}

/// `homestack` run by ssh as its askpass: ask through `socket` and print
/// the answer; the exit code (1 when cancelled or unreachable).
#[must_use]
pub fn client(socket: &Path, hint: Option<&str>, prompt: &str) -> i32 {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return 1;
    };
    // The group lets the TUI stop ssh when the touch is cancelled.
    let group = rustix::process::getpgrp().as_raw_nonzero().get();
    let request = json!({"hint": hint, "prompt": prompt, "group": group});
    if writeln!(stream, "{request}").is_err() {
        return 1;
    }
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
        return 1;
    }
    let Ok(response) = serde_json::from_str::<Value>(&line) else {
        return 1;
    };
    match response["answer"].as_str() {
        Some(answer) => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{answer}").is_err() || out.flush().is_err() {
                return 1;
            }
            0
        }
        None => 1,
    }
}
