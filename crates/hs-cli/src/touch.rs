//! Audible cues while a security key waits for a touch, and the name of a
//! failed key login (Python `touch.py`, I2).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// ssh gives up on an unconfirmed touch after about 30 s (measured on a
/// YubiKey), so one attempt can run for several windows. The cue repeats
/// every 8 s until the attempt ends.
pub const REPEAT_SECONDS: f64 = 8.0;
pub const TIMEOUT_SECONDS: f64 = 30.0;
pub const INITIAL_DELAY_SECONDS: f64 = 1.0;
pub const MAX_WAIT_SECONDS: f64 = 300.0;

const SOUND_DIR: &str = "/usr/share/sounds/freedesktop/stereo";
const DEFAULT_FILES: [(&str, &str); 3] = [
    ("wait", "window-attention.oga"),
    ("ok", "complete.oga"),
    ("fail", "dialog-error.oga"),
];
/// Players tried in order: the command before the sound file.
const PLAYERS: [&[&str]; 3] = [&["pw-play"], &["paplay"], &["canberra-gtk-play", "-f"]];

/// A cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Wait,
    Ok,
    Fail,
}

impl Event {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Ok => "ok",
            Self::Fail => "fail",
        }
    }
}

/// What plays a cue; tests record instead.
pub type Player = Arc<dyn Fn(&Sounds, Event) + Send + Sync>;

/// The cue settings (`[sounds]` of `client.toml`). A missing player or file
/// falls back to the terminal bell.
#[derive(Debug, Clone, PartialEq)]
pub struct Sounds {
    pub enabled: bool,
    pub player: Vec<String>,
    pub files: BTreeMap<String, String>,
    pub repeat_seconds: f64,
    pub initial_delay: f64,
}

impl Default for Sounds {
    fn default() -> Self {
        Self {
            enabled: false,
            player: Vec::new(),
            files: DEFAULT_FILES
                .iter()
                .map(|(event, file)| ((*event).to_owned(), (*file).to_owned()))
                .collect(),
            repeat_seconds: REPEAT_SECONDS,
            initial_delay: INITIAL_DELAY_SECONDS,
        }
    }
}

impl Sounds {
    /// Silent: tests and library callers.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The `[sounds]` table `hs-config` validated (Python `_parse_sounds`).
    #[must_use]
    pub fn from_table(table: Option<&toml::Table>) -> Self {
        let mut sounds = Self::default();
        let Some(table) = table else {
            return sounds;
        };
        if let Some(enabled) = table.get("enabled").and_then(toml::Value::as_bool) {
            sounds.enabled = enabled;
        }
        match table.get("player") {
            Some(toml::Value::String(player)) => sounds.player = vec![player.clone()],
            Some(toml::Value::Array(items)) => {
                sounds.player = items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect();
            }
            _ => {}
        }
        for event in ["wait", "ok", "fail"] {
            if let Some(file) = table.get(event).and_then(toml::Value::as_str) {
                sounds.files.insert(event.to_owned(), file.to_owned());
            }
        }
        match table.get("repeat_seconds") {
            Some(toml::Value::Integer(seconds)) => sounds.repeat_seconds = *seconds as f64,
            Some(toml::Value::Float(seconds)) => sounds.repeat_seconds = *seconds,
            _ => {}
        }
        sounds
    }

    /// The argv that plays one cue, or `None` without a player or file.
    #[must_use]
    pub fn command(&self, event: Event) -> Option<Vec<String>> {
        let name = self.files.get(event.as_str()).filter(|name| !name.is_empty())?;
        let mut path = PathBuf::from(expand_user(name));
        if !path.is_absolute() {
            path = Path::new(SOUND_DIR).join(name);
        }
        if !path.is_file() {
            return None;
        }
        let prefix: Vec<String> = if self.player.is_empty() {
            PLAYERS
                .iter()
                .find(|player| crate::system::which(player[0]).is_some())
                .map(|player| player.iter().map(|part| (*part).to_owned()).collect())?
        } else {
            self.player.clone()
        };
        crate::system::which(prefix.first()?)?;
        let mut argv = prefix;
        argv.push(path.display().to_string());
        Some(argv)
    }
}

fn expand_user(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_owned(),
    }
}

/// Ring the terminal bell on the controlling terminal; never fails.
pub fn ring_bell() {
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(b"\x07");
    }
}

/// Start the player in its own session, reaped by a thread.
pub fn spawn_player(argv: &[String]) -> std::io::Result<()> {
    let (program, args) = argv.split_first().ok_or_else(|| std::io::Error::other("no player"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Play one cue without blocking; never fails.
pub fn play(sounds: &Sounds, event: Event, spawn: &dyn Fn(&[String]) -> std::io::Result<()>, bell: &dyn Fn()) {
    if !sounds.enabled {
        return;
    }
    match sounds.command(event) {
        None => bell(),
        Some(argv) => {
            if spawn(&argv).is_err() {
                bell();
            }
        }
    }
}

/// The real player: [`play`] with [`spawn_player`] and [`ring_bell`].
#[must_use]
pub fn default_player() -> Player {
    Arc::new(|sounds: &Sounds, event: Event| play(sounds, event, &spawn_player, &ring_bell))
}

#[derive(Default)]
struct WatchState {
    done: bool,
    finished: bool,
    alerted: bool,
}

struct Shared {
    state: Mutex<WatchState>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait up to `seconds` or until done; whether it is done.
    fn wait_done(&self, seconds: f64) -> bool {
        let guard = self.lock();
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, Duration::from_secs_f64(seconds.max(0.0)), |state| !state.done)
            .unwrap_or_else(PoisonError::into_inner);
        guard.done
    }
}

/// The cue schedule of one wait on a security key: the attention cue after
/// `initial_delay` (a login that needs no touch stays silent), again every
/// `repeat_seconds` until the attempt ends or the timeout; [`finish`]
/// plays the success cue (only if a cue was played) or the failure cue.
///
/// [`finish`]: TouchWatch::finish
pub struct TouchWatch {
    sounds: Sounds,
    player: Player,
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TouchWatch {
    /// Start the schedule.
    #[must_use]
    pub fn start(_label: &str, sounds: Sounds, player: Player, timeout: f64) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(WatchState::default()),
            wake: Condvar::new(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            let sounds = sounds.clone();
            let player = Arc::clone(&player);
            std::thread::spawn(move || {
                if shared.wait_done(sounds.initial_delay) {
                    return;
                }
                let mut elapsed = sounds.initial_delay;
                loop {
                    {
                        let mut state = shared.lock();
                        if state.finished {
                            return;
                        }
                        state.alerted = true;
                        player(&sounds, Event::Wait);
                    }
                    if shared.wait_done(sounds.repeat_seconds) {
                        return;
                    }
                    elapsed += sounds.repeat_seconds;
                    if elapsed >= timeout {
                        return;
                    }
                }
            })
        };
        Self {
            sounds,
            player,
            shared,
            thread: Mutex::new(Some(thread)),
        }
    }

    /// End the wait; only the first call plays a cue.
    pub fn finish(&self, ok: bool) {
        {
            let mut state = self.shared.lock();
            if state.finished {
                return;
            }
            state.finished = true;
            state.done = true;
            self.shared.wake.notify_all();
            if !ok {
                (self.player)(&self.sounds, Event::Fail);
            } else if state.alerted {
                (self.player)(&self.sounds, Event::Ok);
            }
        }
        let thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for TouchWatch {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.done = true;
        self.shared.wake.notify_all();
    }
}

/// Cue a hardware-key wait around `work`: an error plays the failure cue,
/// success the success cue if one was needed (Python's `touch_wait`).
pub fn touch_wait<T, E>(
    label: &str,
    sounds: &Sounds,
    player: Player,
    work: impl FnOnce(&TouchWatch) -> Result<T, E>,
) -> Result<T, E> {
    let watch = TouchWatch::start(label, sounds.clone(), player, MAX_WAIT_SECONDS);
    let result = work(&watch);
    watch.finish(result.is_ok());
    result
}

/// Why an ssh attempt with a security key failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Absent,
    NotConfirmed,
    Denied,
    /// ssh was stopped by a signal: Esc in the TUI while it connected.
    Cancelled,
    Failed,
}

/// The short message for a failure.
#[must_use]
pub const fn message(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Absent => "Security key not found; plug it in and retry.",
        FailureKind::NotConfirmed => "Key touch was not confirmed in time.",
        FailureKind::Denied => "The server rejected the key.",
        FailureKind::Cancelled => "Connection cancelled.",
        FailureKind::Failed => "SSH authentication failed.",
    }
}

/// A missing device fails at once with "device not found"; an unconfirmed
/// touch runs to ssh's own timeout and then reports a signing failure. With
/// several keys an unconfirmed touch wins.
#[must_use]
pub fn classify_ssh_failure(code: i32, elapsed: f64, stderr: &str) -> FailureKind {
    // No exit code: a signal stopped ssh.
    if code == -1 {
        return FailureKind::Cancelled;
    }
    let text = stderr.to_lowercase();
    if text.contains("signing failed") && elapsed >= TIMEOUT_SECONDS - 5.0 {
        return FailureKind::NotConfirmed;
    }
    if text.contains("device not found") {
        return FailureKind::Absent;
    }
    if text.contains("permission denied") {
        return FailureKind::Denied;
    }
    FailureKind::Failed
}
