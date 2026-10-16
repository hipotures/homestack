//! The terminal and the event loop of the TUI: keys from a reader thread,
//! the status from the servers every few seconds, requests started by keys.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{execute, terminal};
use hs_client::Client;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc;

use super::action::{Decision, Desktop, Kind, Progress, work};
use super::draw::draw;
use super::install::{AskView, Mode};
use super::installing::{self, Command, Setup};
use super::state::{App, Overlay, Request};
use super::theme::Preferences;
use crate::app::{Context, client_error, discovery_report, transport_report};
use crate::askpass::{Bridge, Event as AskEvent};
use crate::config::Loaded;
use crate::touch::Sounds;
use crate::{AppError, config, view};

/// How often the status is read.
const REFRESH: Duration = Duration::from_secs(10);

/// What a background task brings back.
enum Update {
    Status(String, Value),
    Servers(Vec<(String, Option<String>)>),
    Unreachable(Vec<(String, String)>),
    Report(String, Vec<String>),
    Log(String),
    /// What the install worker tells.
    Install(super::install::Event),
    /// ssh asks through the askpass bridge.
    Ask(crate::askpass::Question),
    /// ssh's question is over.
    AskEnded(u64),
    /// What the setup box's worker tells, with the number of the box it
    /// was started for.
    Setup(Option<u32>, super::setup_work::Event),
    /// The running action's progress.
    Action(Progress),
    /// The server's workspace and Gold pools, and default home size (MiB).
    Defaults(Option<String>, Option<String>, Option<u64>),
    /// The images of the reinstall box of these containers.
    Images(Vec<u32>, Result<super::reinstall::Images, String>),
}

/// The terminal in raw mode on the alternate screen; put back on drop,
/// also after a panic.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn open() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::event::EnableMouseCapture
        )?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        crossterm::event::DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// The thread that reads the terminal's events; it stops reading while ssh
/// has the terminal, so no key typed for ssh is taken.
struct Reader {
    paused: Arc<AtomicBool>,
    /// Set by the thread once it no longer reads.
    idle: Arc<AtomicBool>,
}

impl Reader {
    fn start(keys: mpsc::UnboundedSender<Event>) -> Self {
        let paused = Arc::new(AtomicBool::new(false));
        let idle = Arc::new(AtomicBool::new(false));
        let (thread_paused, thread_idle) = (Arc::clone(&paused), Arc::clone(&idle));
        std::thread::spawn(move || {
            loop {
                if thread_paused.load(Ordering::SeqCst) {
                    thread_idle.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if keys.send(event).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        Self { paused, idle }
    }

    /// Stop reading; returns once the thread is out of its last poll.
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        while !self.idle.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn resume(&self) {
        self.idle.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
    }
}

/// Give the terminal to `ssh TARGET` and take it back when ssh ends; the
/// exit code (`None`: ssh did not start). A failed ssh waits for Enter, so
/// its message can be read.
fn hand_over(screen: &mut Screen, reader: &Reader, target: &str, name: &str) -> Option<i32> {
    reader.pause();
    restore();
    println!("{}", super::shell::banner(target, name));
    let code = std::process::Command::new("ssh")
        .arg(target)
        .status()
        .ok()
        .map(|status| status.code().unwrap_or(-1));
    if code != Some(0) {
        println!("{}", super::shell::outcome(target, code));
        println!("Press Enter to go back to HomeStack.");
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }
    let _ = terminal::enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture
    );
    let _ = screen.terminal.clear();
    reader.resume();
    code
}

/// Lend the terminal to the setup's worker: leave the screen, show the
/// banner, let the worker run, and take the screen back when it is done.
fn lend_terminal(screen: &mut Screen, reader: &Reader, handoff: &super::setup_work::Handoff) {
    reader.pause();
    restore();
    for line in &handoff.banner {
        println!("{line}");
    }
    if handoff.go.send(()).is_ok() {
        let _ = handoff.done.recv();
    }
    let _ = terminal::enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture
    );
    let _ = screen.terminal.clear();
    reader.resume();
}

/// The setup box's worker: where its commands go, and its cancel flag.
struct SetupWorker {
    commands: std::sync::mpsc::Sender<super::setup_work::Command>,
    cancel: Arc<AtomicBool>,
    /// Set on close: ssh stops if it is still connecting.
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// The local time `HH:MM:SS` and date, from the offset of `date +%z` read once.
struct Clock {
    offset: i64,
}

impl Clock {
    fn new() -> Self {
        let offset = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| {
                let text = text.trim();
                let sign = if text.starts_with('-') { -1 } else { 1 };
                let digits = text.trim_start_matches(['+', '-']);
                let hours: i64 = digits.get(0..2)?.parse().ok()?;
                let minutes: i64 = digits.get(2..4)?.parse().ok()?;
                Some(sign * (hours * 3600 + minutes * 60))
            })
            .unwrap_or(0);
        Self { offset }
    }

    fn local_seconds(&self) -> i64 {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
        seconds + self.offset
    }

    fn now(&self) -> String {
        let day = self.local_seconds().rem_euclid(86_400);
        format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
    }

    /// The local date `YYYY-MM-DD`.
    fn today(&self) -> String {
        time::OffsetDateTime::from_unix_timestamp(self.local_seconds()).map_or_else(
            |_| String::new(),
            |at| format!("{:04}-{:02}-{:02}", at.year(), u8::from(at.month()), at.day()),
        )
    }
}

/// Every server's answer to `/v1/config/version`: the URL and why it did
/// not answer.
async fn health(client: &Client) -> Vec<(String, Option<String>)> {
    let urls: Vec<String> = client.urls().map(ToString::to_string).collect();
    let mut servers = Vec::new();
    for (index, url) in urls.into_iter().enumerate() {
        let error = client
            .config_version_on(index)
            .await
            .err()
            .map(|error| client_error(error).0);
        servers.push((url, error));
    }
    servers
}

/// Read the status and the servers' health once; nothing is told once
/// `stop` is set (a new client.toml replaced this client meanwhile).
async fn read_status(client: &Client, updates: &mpsc::UnboundedSender<Update>, stop: &AtomicBool) {
    let servers = health(client).await;
    // The servers' dots at once; the status takes a few seconds more.
    if !stop.load(Ordering::SeqCst) {
        let _ = updates.send(Update::Servers(servers.clone()));
    }
    let answer = client.get_from("/v1/status").await;
    if stop.load(Ordering::SeqCst) {
        return;
    }
    match answer {
        Ok((url, status)) => {
            let _ = updates.send(Update::Servers(servers));
            let _ = updates.send(Update::Status(url.to_string(), status));
        }
        Err(error) => {
            let reason = client_error(error).0;
            let unreachable = servers
                .into_iter()
                .map(|(url, error)| (url, error.unwrap_or_else(|| reason.clone())))
                .collect();
            let _ = updates.send(Update::Unreachable(unreachable));
        }
    }
}

/// Read what a new workspace gets by default: the server's workspace pool
/// and its home size; false when no server answered.
async fn read_defaults(client: &Client, updates: &mpsc::UnboundedSender<Update>, stop: &AtomicBool) -> bool {
    let Ok((_, capabilities)) = client.get_from("/v1/capabilities").await else {
        return false;
    };
    let pool = capabilities["installation"]["pool"].as_str().map(str::to_owned);
    let gold_pool = capabilities["installation"]["gold_pool"].as_str().map(str::to_owned);
    let home = match client.get_from("/v1/config").await {
        Ok((_, stored)) => stored
            .as_str()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("home")?.get("default_size")?.as_str().and_then(mib_of)),
        Err(_) => None,
    };
    if !stop.load(Ordering::SeqCst) {
        let _ = updates.send(Update::Defaults(pool, gold_pool, home));
    }
    true
}

/// The MiB of `512M`, `64G` or `1T`.
fn mib_of(size: &str) -> Option<u64> {
    let size = size.trim().to_ascii_uppercase();
    let (number, unit) = size.split_at(size.len().checked_sub(1)?);
    let number: u64 = number.parse().ok()?;
    match unit {
        "M" => Some(number),
        "G" => number.checked_mul(1024),
        "T" => number.checked_mul(1024 * 1024),
        _ => None,
    }
}

/// Read the status and the servers' health now and every [`REFRESH`].
fn spawn_refresh(client: Arc<Client>, updates: mpsc::UnboundedSender<Update>, stop: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut defaults_read = false;
        loop {
            read_status(&client, &updates, &stop).await;
            if !defaults_read {
                defaults_read = read_defaults(&client, &updates, &stop).await;
            }
            if updates.is_closed() || stop.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(REFRESH).await;
        }
    });
}

fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or(url)
        .to_owned()
}

fn transport_lines(report: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for instance in report["transport"]["instances"].as_array().into_iter().flatten() {
        let host = host(instance["url"].as_str().unwrap_or_default());
        if instance["ok"] == true {
            let version = instance["config_version"].as_str().unwrap_or("none yet");
            let short: String = version.chars().take(12).collect();
            lines.push(format!("✓ {host:<24} config {short}"));
        } else {
            lines.push(format!("✗ {host:<24} {}", view::text(&instance["error"])));
        }
    }
    let pve = &report["pve"];
    lines.push(String::new());
    lines.push(format!("node     {}", view::text(&pve["node"])));
    lines.push(format!("PVE      {}", view::text(&pve["version"])));
    lines.push(format!("kernel   {}", view::text(&pve["kernel"])));
    if let Some(seconds) = pve["uptime"].as_u64() {
        lines.push(format!(
            "uptime   {} days {} h",
            seconds / 86_400,
            seconds % 86_400 / 3600
        ));
    }
    lines
}

fn discovery_lines(report: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, found) in report["local"]["commands"].as_object().into_iter().flatten() {
        let state = if found == true { "✓ found" } else { "✗ missing" };
        lines.push(format!("{name:<28} {state}"));
    }
    let keys = report["local"]["hardware_ssh_identities"]
        .as_array()
        .map_or(0, Vec::len);
    lines.push(format!("{:<28} {keys}", "security-key SSH identities"));
    lines.push(String::new());
    let server = &report["server"];
    if server.is_null() {
        for error in report["errors"].as_array().into_iter().flatten() {
            lines.push(format!("✗ {}", view::text(error)));
        }
        return lines;
    }
    lines.push(format!("server   {}", host(server["url"].as_str().unwrap_or_default())));
    let nodes: Vec<String> = server["discovery"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|node| format!("{} {}", view::text(&node["node"]), view::text(&node["status"])))
        .collect();
    lines.push(format!("nodes    {}", nodes.join(", ")));
    for gold in server["discovery"]["gold_candidates"].as_array().into_iter().flatten() {
        lines.push(format!(
            "Gold     {} {} on {}",
            view::text(&gold["vmid"]),
            view::text(&gold["name"]),
            view::text(&gold["node"])
        ));
    }
    lines
}

/// Start what `request` asks for.
fn start(request: &Request, client: Option<&Arc<Client>>, ctx: &Context, updates: &mpsc::UnboundedSender<Update>) {
    let Some(client) = client.cloned() else {
        let _ = updates.send(Update::Report(
            "HomeStack".to_owned(),
            vec!["There is no client.toml yet: nothing to ask.".to_owned()],
        ));
        return;
    };
    let updates = updates.clone();
    match request {
        Request::Transport => {
            tokio::spawn(async move {
                let lines = match transport_report(&client).await {
                    Ok(report) => transport_lines(&report),
                    Err(error) => super::draw::unreachable_lines(&error.0),
                };
                let _ = updates.send(Update::Report("servers".to_owned(), lines));
            });
        }
        Request::Discover => {
            let system = Arc::clone(&ctx.system);
            let home = ctx.home.clone();
            tokio::spawn(async move {
                let report = discovery_report(&client, system.as_ref(), &home).await;
                let _ = updates.send(Update::Report("this desktop".to_owned(), discovery_lines(&report)));
            });
        }
        Request::Quit
        | Request::Act(_)
        | Request::Decide(_)
        | Request::Reload
        | Request::Ssh(_)
        | Request::Images(_)
        | Request::Setup(_)
        | Request::SetupAsk(_)
        | Request::Install(_)
        | Request::Answer(_)
        | Request::HerdrTabs
        | Request::Installed => {}
    }
}

/// The client of `client.toml` and the file itself, or why there is none.
fn client(ctx: &Context) -> Result<(Client, Loaded), (String, Vec<String>)> {
    let path = config::default_path(&ctx.home, ctx.config_home.as_deref());
    if !path.exists() {
        return Err((
            "HomeStack is not installed on this desktop yet.".to_owned(),
            vec![
                format!("There is no {}.", path.display()),
                "Menu › Install or reconfigure sets it up.".to_owned(),
            ],
        ));
    }
    let loaded = config::load(&path).map_err(|error| ("client.toml cannot be used".to_owned(), vec![error.0]))?;
    let settings = loaded
        .client_config(&ctx.home)
        .map_err(|error| ("client.toml cannot be used".to_owned(), vec![error.0]))?;
    let client =
        Client::new(settings).map_err(|error| ("The client cannot start".to_owned(), vec![client_error(error).0]))?;
    Ok((client, loaded))
}

/// The client of the TUI and how it is read: started again after an install.
struct Connection {
    client: Option<Arc<Client>>,
    desktop: Option<Desktop>,
    stop: Arc<AtomicBool>,
}

impl Connection {
    /// Read `client.toml` and start reading the status; a notice when it
    /// cannot be used.
    fn open(ctx: &Context, updates: &mpsc::UnboundedSender<Update>) -> Result<Self, (String, Vec<String>)> {
        let (client, loaded) = client(ctx)?;
        let client = Arc::new(client);
        let stop = Arc::new(AtomicBool::new(false));
        spawn_refresh(Arc::clone(&client), updates.clone(), Arc::clone(&stop));
        Ok(Self {
            client: Some(client),
            desktop: Some(Desktop {
                home: ctx.home.clone(),
                loaded,
                poll: ctx.poll,
            }),
            stop,
        })
    }

    fn none() -> Self {
        Self {
            client: None,
            desktop: None,
            stop: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// The install worker and the askpass bridge it asks through.
#[derive(Default)]
struct Installing {
    bridge: Option<Bridge>,
    worker: Option<std::sync::mpsc::Sender<Command>>,
    /// The worker's: set, ssh stops connecting.
    stop: Arc<AtomicBool>,
    /// ssh's question waiting for its answer.
    pending: Option<crate::askpass::Question>,
}

impl Installing {
    fn send(&mut self, command: Command, app: &mut App, ctx: &Context, updates: &mpsc::UnboundedSender<Update>) {
        if self.worker.is_none() && matches!(command, Command::Connect { .. }) {
            self.start(app, ctx, updates);
        }
        let cancel = command == Command::Cancel;
        if let Some(worker) = &self.worker {
            let _ = worker.send(command);
        }
        if cancel {
            // A worker still connecting does not read its commands.
            self.stop.store(true, Ordering::SeqCst);
            self.worker = None;
        }
    }

    /// The askpass bridge (started once): the prefix that sends ssh's
    /// questions, the touch of a security key among them, to the TUI.
    fn bridge(&mut self, app: &mut App, updates: &mpsc::UnboundedSender<Update>) -> Vec<String> {
        if self.bridge.is_none() {
            let program = std::env::current_exe().unwrap_or_else(|_| "homestack".into());
            match Bridge::start(program) {
                Ok((bridge, events)) => {
                    let updates = updates.clone();
                    std::thread::spawn(move || {
                        for event in events {
                            let update = match event {
                                AskEvent::Ask(question) => Update::Ask(question),
                                AskEvent::Ended(id) => Update::AskEnded(id),
                            };
                            if updates.send(update).is_err() {
                                return;
                            }
                        }
                    });
                    self.bridge = Some(bridge);
                }
                Err(error) => app.log(
                    &app.clock.clone(),
                    &format!("ssh's questions cannot reach the TUI: {error}"),
                ),
            }
        }
        self.bridge.as_ref().map(Bridge::command_prefix).unwrap_or_default()
    }

    fn start(&mut self, app: &mut App, ctx: &Context, updates: &mpsc::UnboundedSender<Update>) {
        self.bridge(app, updates);
        self.stop = Arc::new(AtomicBool::new(false));
        let client_path = config::default_path(&ctx.home, ctx.config_home.as_deref());
        let token_dir = client_path
            .parent()
            .map_or_else(|| ctx.home.clone(), std::path::Path::to_path_buf);
        let sounds = config::load(&client_path)
            .map(|loaded| Sounds::from_table(loaded.client.sounds.as_ref()))
            .unwrap_or_default();
        let mode = app.install.as_ref().map_or(Mode::Install, |install| install.mode);
        let setup = Setup {
            mode,
            system: Arc::clone(&ctx.system),
            home: ctx.home.clone(),
            token_dir,
            client_path,
            servers: Arc::clone(&ctx.servers),
            sounds,
            ssh_prefix: self.bridge.as_ref().map(Bridge::command_prefix).unwrap_or_default(),
            stop: Arc::clone(&self.stop),
            server_image: None,
            handle: tokio::runtime::Handle::current(),
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        let updates = updates.clone();
        installing::spawn(setup, receiver, move |event| {
            let _ = updates.send(Update::Install(event));
        });
        self.worker = Some(sender);
    }
}

/// Run the TUI until the user quits; the exit code.
pub async fn run(ctx: &mut Context) -> Result<i32, AppError> {
    let path = Preferences::default_path(&ctx.home, ctx.config_home.as_deref());
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let (preferences, problem) = match Preferences::load(&path) {
        Ok(preferences) => (preferences, None),
        Err(error) => (Preferences::fallback(&path), Some(error.0)),
    };
    let mut app = App::new(preferences, no_color)?;
    let log_dir = config::default_path(&ctx.home, ctx.config_home.as_deref())
        .parent()
        .map_or_else(|| ctx.home.clone(), std::path::Path::to_path_buf);
    if std::fs::create_dir_all(&log_dir).is_ok() {
        app.log_file = Some(log_dir.join("tui.log"));
    }
    let clock = Clock::new();
    app.clock = clock.now();
    app.today = clock.today();
    app.load_earlier();
    if let Some(problem) = problem {
        app.log(&clock.now(), &format!("{problem}; the HomeStack theme is used"));
    }
    let (updates, mut received) = mpsc::unbounded_channel::<Update>();
    app.set_keys(public_keys(&ctx.home));
    let first_start = !config::default_path(&ctx.home, ctx.config_home.as_deref()).exists();
    let mut connection = if first_start {
        if app.open_install(Mode::Install, true) == Some(Request::HerdrTabs) {
            match crate::root_herdr::tabs(ctx.system.as_ref()) {
                Ok(tabs) => app.set_herdr_tabs(tabs.into_iter().map(|tab| (tab.name, tab.title)).collect()),
                Err(error) => app.log(&clock.now(), &error.0),
            }
        }
        app.log(&clock.now(), "no client.toml: the install screen");
        Connection::none()
    } else {
        Connection::open(ctx, &updates).unwrap_or_else(|notice| {
            app.set_notice(Some(notice));
            Connection::none()
        })
    };
    let mut installing = Installing::default();
    let (keys, mut events) = mpsc::unbounded_channel::<Event>();
    let reader = Reader::start(keys);
    // While ssh has the terminal it is not in raw mode, and Ctrl+C before
    // ssh's own session must not end the TUI.
    #[cfg(unix)]
    let _interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
    let mut screen = Screen::open().map_err(|error| AppError::new(format!("Cannot open the terminal: {error}")))?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // While the first status is read, the waiting bar moves ten times a second.
    let mut fast = tokio::time::interval(Duration::from_millis(100));
    let mut connected: Option<String> = None;
    let mut decide: Option<mpsc::UnboundedSender<Decision>> = None;
    // The worker and its connection of the last target (`None`: this
    // desktop), kept until setup opens on another: no new touch.
    let mut setup_workers: std::collections::HashMap<Option<u32>, SetupWorker> = std::collections::HashMap::new();
    // The target of the open box; another target's worker cannot speak to it.
    let mut setup_open: Option<Option<u32>> = None;
    let mut details = super::details::Loader::default();
    loop {
        app.clock = clock.now();
        app.today = clock.today();
        details.update(app.details_target(), connection.client.as_ref());
        screen
            .terminal
            .draw(|frame| draw(frame, &app))
            .map_err(|error| AppError::new(format!("Cannot draw: {error}")))?;
        tokio::select! {
            Some(event) = events.recv() => {
                let request = match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
                    Event::Mouse(mouse) => app.mouse(mouse),
                    _ => None,
                };
                if let Some(request) = request {
                    if request == Request::Quit {
                        for (_, worker) in setup_workers.drain() {
                            worker.close();
                        }
                        installing.send(Command::Cancel, &mut app, ctx, &updates);
                        return Ok(0);
                    }
                    match request {
                        Request::Act(start) if start.kind == Kind::Reinstall => {
                            let (sender, mut decisions) = mpsc::unbounded_channel();
                            decide = Some(sender);
                            let updates = updates.clone();
                            let deploy_toml = deploy_toml(ctx);
                            tokio::spawn(async move {
                                let told = updates.clone();
                                let tell = move |progress: Progress| {
                                    let _ = told.send(Update::Action(progress));
                                };
                                let say = move |line: &str| {
                                    let _ = updates.send(Update::Log(line.trim().to_owned()));
                                };
                                super::reinstall::work(deploy_toml, start, &mut decisions, &tell, &say).await;
                            });
                        }
                        Request::Images(targets) => {
                            let updates = updates.clone();
                            let deploy_toml = deploy_toml(ctx);
                            let offset = clock.offset;
                            tokio::spawn(async move {
                                let images = super::reinstall::read(&deploy_toml, &targets, offset).await;
                                let _ = updates.send(Update::Images(targets, images));
                            });
                        }
                        Request::Act(start) => {
                            if let (Some(client), Some(desktop)) = (connection.client.clone(), connection.desktop.clone()) {
                                let (sender, mut decisions) = mpsc::unbounded_channel();
                                decide = Some(sender);
                                let updates = updates.clone();
                                tokio::spawn(async move {
                                    let tell = move |progress: Progress| {
                                        let _ = updates.send(Update::Action(progress));
                                    };
                                    work(&client, &desktop, start, &mut decisions, &tell).await;
                                });
                            }
                        }
                        Request::Decide(decision) => {
                            if let Some(sender) = &decide {
                                let _ = sender.send(decision);
                            }
                        }
                        Request::Reload => {
                            details.cancel();
                            app.clear_details();
                            if let Some(client) = connection.client.clone() {
                                let updates = updates.clone();
                                let stop = Arc::clone(&connection.stop);
                                tokio::spawn(async move { read_status(&client, &updates, &stop).await });
                            }
                        }
                        Request::Install(command) => {
                            app.log(&clock.now(), &format!("install: {command:?}"));
                            installing.send(command, &mut app, ctx, &updates);
                        }
                        Request::Answer(answer) => {
                            if let Some(question) = installing.pending.take() {
                                question.answer(answer);
                            }
                        }
                        Request::Setup(vmid) => {
                            // One connection is kept: another workspace's closes
                            // (the owner, 2026-10-15).
                            setup_workers.retain(|target, worker| {
                                if *target == vmid {
                                    return true;
                                }
                                let _ = worker.commands.send(super::setup_work::Command::Quit);
                                false
                            });
                            match connection.desktop.clone() {
                            Some(_) if setup_workers.get(&vmid).is_some_and(|worker| !worker.thread.is_finished()) => {
                                setup_open = Some(vmid);
                                if let Some(worker) = setup_workers.get(&vmid) {
                                    let _ = worker.commands.send(super::setup_work::Command::Reopen);
                                }
                            }
                            Some(desktop) => {
                                setup_open = Some(vmid);
                                let generation = vmid;
                                let cancel = Arc::new(AtomicBool::new(false));
                                let sender = updates.clone();
                                let tell: Arc<dyn Fn(super::setup_work::Event) + Send + Sync> =
                                    Arc::new(move |event| {
                                        let _ = sender.send(Update::Setup(generation, event));
                                    });
                                let ssh_prefix = installing.bridge(&mut app, &updates);
                                let stop = Arc::new(AtomicBool::new(false));
                                let start = super::setup_work::Start {
                                    vmid,
                                    ssh_prefix,
                                    stop: Arc::clone(&stop),
                                    client: connection.client.clone(),
                                    system: Arc::clone(&ctx.system),
                                    home: ctx.home.clone(),
                                    state_home: ctx.state_home.clone(),
                                    loaded: desktop.loaded,
                                    handle: tokio::runtime::Handle::current(),
                                };
                                let (commands, thread) = super::setup_work::spawn(start, tell, Arc::clone(&cancel));
                                setup_workers.insert(vmid, SetupWorker {
                                    commands,
                                    cancel,
                                    stop,
                                    thread,
                                });
                            }
                            None => {
                                app.setup = None;
                                if let Some(shown) = app.before_setup.take() {
                                    app.shown = shown;
                                }
                                app.log(&clock.now(), "setup: there is no client.toml yet");
                            }
                            }
                        }
                        Request::SetupAsk(ask) => {
                            use super::setup::Ask;
                            use super::setup_work::Command as Work;
                            if let Some(worker) = setup_open.and_then(|target| setup_workers.get(&target)) {
                                let command = match ask {
                                    Ask::Refresh => Some(Work::Refresh),
                                    Ask::Catalog => Some(Work::Catalog),
                                    Ask::Execute => app
                                        .setup
                                        .as_mut()
                                        .and_then(|setup| setup.pending_plan.take())
                                        .map(|plan| Work::Execute(Box::new(plan))),
                                    Ask::Cancel => {
                                        worker.cancel.store(true, Ordering::SeqCst);
                                        None
                                    }
                                    Ask::Close => {
                                        // Still connecting, ssh stops; connected, nothing.
                                        worker.stop.store(true, Ordering::SeqCst);
                                        Some(Work::Close)
                                    }
                                };
                                if let Some(command) = command {
                                    let _ = worker.commands.send(command);
                                }
                            }
                            if ask == Ask::Close {
                                setup_open = None;
                            }
                        }
                        Request::Ssh(vmid) => {
                            open_ssh(&mut app, vmid, &ctx.home, &mut screen, &reader);
                        }
                        Request::HerdrTabs => match crate::root_herdr::tabs(ctx.system.as_ref()) {
                            Ok(tabs) => app.set_herdr_tabs(
                                tabs.into_iter().map(|tab| (tab.name, tab.title)).collect(),
                            ),
                            Err(error) => app.log(&clock.now(), &error.0),
                        },
                        Request::Installed => {
                            details.cancel();
                            app.clear_details();
                            app.status = None;
                            app.pool = None;
                            app.gold_pool = None;
                            installing.send(Command::Cancel, &mut app, ctx, &updates);
                            connection.stop.store(true, Ordering::SeqCst);
                            connected = None;
                            connection = match Connection::open(ctx, &updates) {
                                Ok(opened) => {
                                    app.set_notice(None);
                                    opened
                                }
                                Err(notice) => {
                                    app.set_notice(Some(notice));
                                    Connection::none()
                                }
                            };
                        }
                        other => start(&other, connection.client.as_ref(), ctx, &updates),
                    }
                }
            }
            Some(update) = received.recv() => match update {
                Update::Status(url, status) => {
                    let host = host(&url).split('.').next().unwrap_or_default().to_owned();
                    if connected.as_deref() != Some(host.as_str()) {
                        app.log(&clock.now(), &format!("status from {host}"));
                        connected = Some(host);
                    }
                    app.set_status(&status);
                }
                Update::Servers(servers) => app.set_servers(servers),
                Update::Images(targets, images) => app.set_images(&targets, images),
                Update::Defaults(pool, gold_pool, home) => {
                    app.pool = pool;
                    app.gold_pool = gold_pool;
                    app.home_default = home;
                }
                Update::Unreachable(servers) => {
                    if connected.take().is_some() || app.unreachable.is_empty() {
                        app.log(&clock.now(), "no server answers");
                    }
                    app.set_unreachable(servers);
                }
                Update::Report(title, lines) => {
                    if matches!(app.overlay, Some(Overlay::Report { .. })) {
                        app.overlay = Some(Overlay::Report { title, lines });
                    }
                }
                Update::Log(line) => app.log(&clock.now(), &line),
                Update::Install(event) => app.install_event(event),
                Update::Ask(question) => {
                    let mut view = AskView::new(question.id, question.kind, &question.prompt);
                    view.key_names = crate::identity::key_names(&ctx.home, &question.prompt);
                    app.ask(view);
                    // A touch too: Esc cancels it.
                    installing.pending = Some(question);
                }
                Update::AskEnded(id) => {
                    app.ask_ended(id);
                    if installing.pending.as_ref().is_some_and(|question| question.id == id) {
                        installing.pending = None;
                    }
                }
                // Another target's worker: its terminal request is dropped.
                Update::Setup(target, _) if setup_open != Some(target) => {}
                Update::Setup(_, super::setup_work::Event::Terminal(handoff)) => {
                    // A box closed meanwhile does not get the terminal.
                    if app.setup.is_some() {
                        tokio::task::block_in_place(|| lend_terminal(&mut screen, &reader, &handoff));
                    }
                }
                Update::Setup(_, event) => app.setup_event(event),
                Update::Action(progress) => {
                    if let Progress::Done { ok, lines } = &progress {
                        details.cancel();
                        app.clear_details();
                        let title = app.action.as_ref().map(super::action::Action::title).unwrap_or_default();
                        // The marked servers are done with.
                        if *ok && app.action.as_ref().is_some_and(|action| action.kind == Kind::Reinstall) {
                            app.marked.clear();
                        }
                        let outcome = if *ok { "done" } else { "failed" };
                        let first = lines.first().cloned().unwrap_or_default();
                        app.log(&clock.now(), &format!("{title}: {outcome}; {first}"));
                        decide = None;
                        if let Some(client) = connection.client.clone() {
                            let updates = updates.clone();
                                let stop = Arc::clone(&connection.stop);
                                tokio::spawn(async move { read_status(&client, &updates, &stop).await });
                        }
                    }
                    app.progress(progress);
                }
            },
            (vmid, result) = details.receive() => {
                if let Some(Request::Ssh(vmid)) = app.set_details(vmid, result) {
                    open_ssh(&mut app, vmid, &ctx.home, &mut screen, &reader);
                }
            },
            _ = tick.tick() => app.tick(),
            _ = fast.tick(), if (app.status.is_none() && app.unreachable.is_empty())
                || app.setup.as_ref().is_some_and(super::setup::SetupBox::animating)
                || app.install.as_ref().is_some_and(super::install::Install::connecting) =>
            {
                if app.status.is_none() && app.unreachable.is_empty() {
                    app.waiting += 1;
                }
                if let Some(setup) = app.setup.as_mut() {
                    setup.frame += 1;
                }
                if let Some(install) = app.install.as_mut() {
                    install.frame += 1;
                }
            }
        }
    }
}

/// Resolve SSH only when requested; a desktop entry needs no server read.
fn open_ssh(app: &mut App, vmid: u32, home: &std::path::Path, screen: &mut Screen, reader: &Reader) {
    let Some(target) = super::shell::prepare(app, home, vmid) else {
        return;
    };
    let name = app
        .selected_entry()
        .and_then(|entry| entry.name.clone())
        .unwrap_or_else(|| vmid.to_string());
    let time = app.clock.clone();
    app.log(&time, &format!("ssh {target} ({name})"));
    let code = tokio::task::block_in_place(|| hand_over(screen, reader, &target, &name));
    app.log(&time, &super::shell::outcome(&target, code));
}

/// The public keys in `~/.ssh`, as `~/.ssh/NAME.pub`, sorted.
fn public_keys(home: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join(".ssh")) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".pub"))
        .map(|name| format!("~/.ssh/{name}"))
        .collect();
    keys.sort();
    keys
}

impl SetupWorker {
    /// Close the connection the worker keeps; wait a little for it, so the
    /// master connection does not outlive the TUI.
    fn close(self) {
        let _ = self.commands.send(super::setup_work::Command::Quit);
        self.cancel.store(true, Ordering::SeqCst);
        for _ in 0..30 {
            if self.thread.is_finished() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `deploy.toml`, next to `client.toml`.
fn deploy_toml(ctx: &Context) -> std::path::PathBuf {
    config::default_path(&ctx.home, ctx.config_home.as_deref()).with_file_name("deploy.toml")
}
