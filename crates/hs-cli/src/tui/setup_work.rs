//! The worker of the setup box: in a thread of its own it resolves the
//! workspace, loads the catalog, opens one connection (the touch of the
//! security key is asked in the TUI through the askpass bridge) and keeps
//! it until setup opens on another workspace, so the box opens again
//! without a touch; it reads the state and runs what the box asks. Interactive
//! installers get the terminal; Herdr registration uses the TUI's SSH prompts.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use hs_client::Client;
use serde_json::Value;

use crate::AppError;
use crate::config::Loaded;
use crate::services::github::Discovery;
use crate::setup::catalog::{Catalog, load_catalog, save_snapshot};
use crate::setup::cli::{
    catalog_with_repositories, connect, connect_asking, key_advice, key_label, local_settings, local_target_of,
    resolve_target, settings_of,
};
use crate::setup::definitions::Entry;
use crate::setup::engine::{Connection, Observer, execute_plan};
use crate::setup::plan::Plan;
use crate::setup::workspace::{LocalWorkspace, Workspace};
use crate::setup::{Settings, Target};
use crate::system::{Output, System};

/// The terminal, lent to the worker: the TUI shows `banner` once it has
/// left the screen, sends on `go`, and takes the screen back when `done`
/// says so.
pub struct Handoff {
    pub banner: Vec<String>,
    pub go: Sender<()>,
    pub done: Receiver<()>,
}

/// What the worker tells the box.
pub enum Event {
    /// A step of the start (Python's spinners before the TUI).
    Loading(String),
    Ready {
        settings: Box<Settings>,
        target: Box<Target>,
        catalog: Box<Catalog>,
        state: Value,
    },
    /// The start failed; the box shows why.
    Failed(String),
    State(Value),
    Catalog(Box<Catalog>, Option<Value>),
    Notice(String, bool),
    Activity(Option<String>, String),
    Progress(String, String),
    Finished(Value, Option<Value>),
    Terminal(Handoff),
    /// The manifests a repository declares.
    Services(String, Result<Discovery, String>),
}

/// What the box asks.
pub enum Command {
    Refresh,
    Catalog,
    Execute(Box<Plan>),
    /// Read a repository's manifests on its default branch (read only).
    Services(String, Option<String>),
    /// The box closed: the connection stays for the next one.
    Close,
    /// A box opened again on the same target: the state is read again.
    Reopen,
    /// The TUI ends: the connection closes.
    Quit,
}

/// What the worker starts from.
pub struct Start {
    /// The workspace; `None` is this desktop (`setup --local`).
    pub vmid: Option<u32>,
    pub client: Option<Arc<Client>>,
    pub system: Arc<dyn System>,
    pub home: PathBuf,
    pub state_home: Option<PathBuf>,
    pub loaded: Loaded,
    pub handle: tokio::runtime::Handle,
    /// The askpass bridge: ssh asks the touch and its questions in the TUI.
    pub ssh_prefix: Vec<String>,
    /// Set when the box closes: ssh stops if it is still connecting.
    pub stop: Arc<AtomicBool>,
}

/// How the worker tells the box.
pub type Tell = Arc<dyn Fn(Event) + Send + Sync>;

/// Run `operation` with the terminal; `None` when the TUI is gone.
pub fn with_terminal<T>(tell: &Tell, banner: Vec<String>, operation: impl FnOnce() -> T) -> Option<T> {
    let (go, started) = channel();
    let (finished, done) = channel();
    tell(Event::Terminal(Handoff { banner, go, done }));
    started.recv().ok()?;
    let value = operation();
    let _ = finished.send(());
    Some(value)
}

/// The run as the box shows it (Python's `progress`, `activity`,
/// `terminal`, `desktop_command`).
pub struct Watcher {
    pub tell: Tell,
    pub cancel: Arc<AtomicBool>,
    pub system: Arc<dyn System>,
    pub ssh_prefix: Vec<String>,
}

impl Observer for Watcher {
    fn progress(&mut self, id: &str, state: &str) {
        (self.tell)(Event::Progress(id.to_owned(), state.to_owned()));
    }

    fn activity(&mut self, id: Option<&str>, message: &str) {
        (self.tell)(Event::Activity(id.map(str::to_owned), message.to_owned()));
    }

    fn terminal(&mut self, operation: &mut dyn FnMut()) {
        with_terminal(&self.tell, Vec::new(), operation);
    }

    fn register_desktop(&mut self, argv: &[String]) -> Result<Output, AppError> {
        if self.cancelled() {
            return Err(AppError::new("Cancelled before desktop Herdr registration"));
        }
        if self.ssh_prefix.is_empty() {
            return Err(AppError::new(
                "Desktop Herdr registration needs the TUI SSH prompt bridge; close and reopen setup",
            ));
        }
        let mut command = self.ssh_prefix.clone();
        command.extend_from_slice(argv);
        self.system
            .run_until(&command, &self.cancel, std::time::Duration::from_secs(300))
            .map_err(|error| {
                AppError::new(match error.kind() {
                    std::io::ErrorKind::Interrupted => {
                        "Desktop Herdr registration cancelled; guest service remains configured"
                    }
                    std::io::ErrorKind::TimedOut => {
                        "Desktop Herdr registration timed out; guest service remains configured"
                    }
                    _ => "Could not run desktop Herdr registration; guest service remains configured",
                })
            })
    }

    fn cancelled(&mut self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// What the worker keeps between commands.
pub struct Session {
    pub settings: Settings,
    pub target: Target,
    pub entries: Vec<Entry>,
    pub workspace: Box<dyn Workspace>,
}

/// The connection, then the state read through it (Python `run_setup`
/// before `SetupApp.run`): `connecting` is shown while `connect` runs; only
/// a connection that opened says "SSH connected". A failed inspection drops
/// the connection.
pub fn startup(
    tell: &Tell,
    settings: Settings,
    target: Target,
    catalog: &Catalog,
    connecting: &str,
    connect: impl FnOnce(&Settings, &Target) -> Result<Box<dyn Workspace>, AppError>,
    inspect: impl FnOnce(&mut Session) -> Result<Value, AppError>,
) -> Result<(Session, Value), AppError> {
    if !connecting.is_empty() {
        tell(Event::Loading(connecting.to_owned()));
    }
    let workspace = connect(&settings, &target)?;
    let mut session = Session {
        settings,
        target,
        entries: catalog.entries.clone(),
        workspace,
    };
    tell(Event::Loading(if session.target.local {
        "Inspecting this desktop".to_owned()
    } else {
        "SSH connected; inspecting workspace".to_owned()
    }));
    let state = inspect(&mut session)?;
    Ok((session, state))
}

/// Resolve, load, connect and inspect.
fn open(start: &Start, tell: &Tell) -> Result<(Session, Catalog, Value), AppError> {
    let mut settings = settings_of(&start.home, start.state_home.as_deref(), &start.loaded)?;
    let (target, mut catalog) = match start.vmid {
        None => {
            settings = local_settings(settings)?;
            (local_target_of(&settings), load_catalog(&settings, None))
        }
        Some(vmid) => {
            tell(Event::Loading("Resolving workspace".to_owned()));
            let client = start
                .client
                .as_ref()
                .ok_or_else(|| AppError::new("There is no client.toml yet"))?;
            let target = start
                .handle
                .block_on(resolve_target(client, &mut settings, &vmid.to_string()))?;
            tell(Event::Loading("Loading setup catalog".to_owned()));
            (target, catalog_with_repositories(start.system.as_ref(), &settings))
        }
    };
    save_snapshot(&settings, &mut catalog)?;
    let key = key_label(start.system.as_ref(), &start.loaded);
    let connecting = if target.local {
        String::new()
    } else {
        format!("Connecting/authenticating SSH (touch your {key} if prompted)")
    };
    let connect = |settings: &Settings, target: &Target| -> Result<Box<dyn Workspace>, AppError> {
        if target.local {
            return Ok(Box::new(LocalWorkspace {
                home: settings.home.clone(),
                user: settings.user.clone(),
            }));
        }
        // The touch is asked in the TUI through the askpass bridge; without
        // the bridge, ssh gets the terminal.
        if !start.ssh_prefix.is_empty() {
            return connect_asking(
                &start.system,
                &start.home,
                &start.loaded,
                settings,
                target,
                start.ssh_prefix.clone(),
                Arc::clone(&start.stop),
            );
        }
        let mut banner = vec![format!(
            "SSH: authenticate once; this session remains open until setup exits. Touch your {key} if prompted."
        )];
        banner.extend(key_advice(start.system.as_ref(), &start.loaded));
        with_terminal(tell, banner, || {
            connect(&start.system, &start.home, &start.loaded, settings, target)
        })
        .unwrap_or_else(|| Err(AppError::new("Setup was closed before the connection")))
    };
    let system = start.system.as_ref();
    let (session, state) = startup(tell, settings, target, &catalog, &connecting, connect, |session| {
        session.inspect(system)
    })?;
    Ok((session, catalog, state))
}

impl Session {
    fn inspect(&mut self, system: &dyn System) -> Result<Value, AppError> {
        crate::setup::engine::inspect_workspace_state_with(
            self.workspace.as_mut(),
            &self.settings,
            system,
            &self.target,
            &self.entries,
        )
    }
}

/// Start the worker; the box's commands go to the returned sender, the
/// cancel flag stops a run between items.
pub fn spawn(start: Start, tell: Tell, cancel: Arc<AtomicBool>) -> (Sender<Command>, std::thread::JoinHandle<()>) {
    let (commands, received) = channel::<Command>();
    let thread = std::thread::spawn(move || run(&start, &tell, &cancel, &received));
    (commands, thread)
}

fn run(start: &Start, tell: &Tell, cancel: &Arc<AtomicBool>, commands: &Receiver<Command>) {
    let (mut session, mut catalog, state) = match open(start, tell) {
        Ok(opened) => opened,
        Err(error) => {
            tell(Event::Failed(error.0));
            return;
        }
    };
    tell(Event::Ready {
        settings: Box::new(session.settings.clone()),
        target: Box::new(session.target.clone()),
        catalog: Box::new(catalog.clone()),
        state,
    });
    let system = start.system.as_ref();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Close => {}
            Command::Quit => return,
            Command::Reopen => match session.inspect(system) {
                Ok(state) => tell(Event::Ready {
                    settings: Box::new(session.settings.clone()),
                    target: Box::new(session.target.clone()),
                    catalog: Box::new(catalog.clone()),
                    state,
                }),
                // The connection is gone: the next box connects again.
                Err(error) => {
                    tell(Event::Failed(error.0));
                    return;
                }
            },
            Command::Services(repository, branch) => {
                let found = crate::services::github::discover(system, &repository, branch.as_deref());
                tell(Event::Services(repository, found));
            }
            Command::Refresh => match session.inspect(system) {
                Ok(state) => tell(Event::State(state)),
                Err(error) => tell(Event::Notice(format!("State refresh failed: {}", error.0), true)),
            },
            Command::Catalog => {
                catalog = if session.target.local {
                    load_catalog(&session.settings, None)
                } else {
                    catalog_with_repositories(system, &session.settings)
                };
                session.entries.clone_from(&catalog.entries);
                let state = match session.inspect(system) {
                    Ok(state) => Some(state),
                    Err(error) => {
                        tell(Event::Notice(format!("State refresh failed: {}", error.0), true));
                        None
                    }
                };
                if save_snapshot(&session.settings, &mut catalog).is_err() {
                    tell(Event::Notice(
                        "Catalog snapshot could not be saved; stable-ID selection remains available".to_owned(),
                        true,
                    ));
                }
                tell(Event::Catalog(Box::new(catalog.clone()), state));
            }
            Command::Execute(plan) => {
                cancel.store(false, Ordering::SeqCst);
                let mut watcher = Watcher {
                    tell: Arc::clone(tell),
                    cancel: Arc::clone(cancel),
                    system: Arc::clone(&start.system),
                    ssh_prefix: start.ssh_prefix.clone(),
                };
                let result = execute_plan(
                    &session.settings,
                    system,
                    &plan,
                    Connection::Open(session.workspace.as_mut()),
                    &mut watcher,
                );
                tell(Event::Activity(None, "Refresh workspace state".to_owned()));
                let state = match session.inspect(system) {
                    Ok(state) => {
                        tell(Event::Activity(None, "Workspace state refreshed".to_owned()));
                        Some(state)
                    }
                    Err(error) => {
                        tell(Event::Activity(None, "Workspace state refresh failed".to_owned()));
                        tell(Event::Notice(format!("State refresh failed: {}", error.0), true));
                        None
                    }
                };
                tell(Event::Finished(result, state));
            }
        }
    }
}
