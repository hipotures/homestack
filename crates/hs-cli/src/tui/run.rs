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

use super::action::{Decision, Desktop, Progress, work};
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
    Details(u32, Value),
    Report(String, Vec<String>),
    Log(String),
    /// What the install worker tells.
    Install(super::install::Event),
    /// ssh asks through the askpass bridge.
    Ask(crate::askpass::Question),
    /// ssh's question is over.
    AskEnded(u64),
    /// The running action's progress.
    Action(Progress),
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
        execute!(stdout, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;
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
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, crossterm::cursor::Show);
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// The local time `HH:MM:SS`, from the offset of `date +%z` read once.
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

    fn now(&self) -> String {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
        let day = (seconds + self.offset).rem_euclid(86_400);
        format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
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

/// Read the status and the servers' health now and every [`REFRESH`].
fn spawn_refresh(client: Arc<Client>, updates: mpsc::UnboundedSender<Update>, stop: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            read_status(&client, &updates, &stop).await;
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
        Request::Details(vmid) => {
            let vmid = *vmid;
            tokio::spawn(async move {
                match client.get(&format!("/v1/workspaces/{vmid}")).await {
                    Ok(details) => {
                        let _ = updates.send(Update::Details(vmid, details));
                    }
                    Err(error) => {
                        let _ = updates.send(Update::Log(format!("details of {vmid}: {}", client_error(error).0)));
                    }
                }
            });
        }
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
            self.worker = None;
        }
    }

    fn start(&mut self, app: &mut App, ctx: &Context, updates: &mpsc::UnboundedSender<Update>) {
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
    if let Some(problem) = problem {
        app.log(&clock.now(), &format!("{problem}; the HomeStack theme is used"));
    }
    let (updates, mut received) = mpsc::unbounded_channel::<Update>();
    app.set_keys(public_keys(&ctx.home));
    let first_start = !config::default_path(&ctx.home, ctx.config_home.as_deref()).exists();
    let mut connection = if first_start {
        app.open_install(Mode::Install, true);
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
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if keys.send(event).is_err() {
                return;
            }
        }
    });
    let mut screen = Screen::open().map_err(|error| AppError::new(format!("Cannot open the terminal: {error}")))?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut connected: Option<String> = None;
    let mut decide: Option<mpsc::UnboundedSender<Decision>> = None;
    loop {
        app.clock = clock.now();
        screen
            .terminal
            .draw(|frame| draw(frame, &app))
            .map_err(|error| AppError::new(format!("Cannot draw: {error}")))?;
        tokio::select! {
            Some(event) = events.recv() => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                    && let Some(request) = app.key(key)
                {
                    if request == Request::Quit {
                        installing.send(Command::Cancel, &mut app, ctx, &updates);
                        return Ok(0);
                    }
                    match request {
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
                        Request::HerdrTabs => match crate::root_herdr::tabs(ctx.system.as_ref()) {
                            Ok(tabs) => app.set_herdr_tabs(
                                tabs.into_iter().map(|tab| (tab.name, tab.title)).collect(),
                            ),
                            Err(error) => app.log(&clock.now(), &error.0),
                        },
                        Request::Installed => {
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
                Update::Unreachable(servers) => {
                    if connected.take().is_some() || app.unreachable.is_empty() {
                        app.log(&clock.now(), "no server answers");
                    }
                    app.set_unreachable(servers);
                }
                Update::Details(vmid, details) => app.set_details(vmid, &details),
                Update::Report(title, lines) => {
                    if matches!(app.overlay, Some(Overlay::Report { .. })) {
                        app.overlay = Some(Overlay::Report { title, lines });
                    }
                }
                Update::Log(line) => app.log(&clock.now(), &line),
                Update::Install(event) => app.install_event(event),
                Update::Ask(question) => {
                    app.ask(AskView::new(question.id, question.kind, &question.prompt));
                    if question.kind != crate::askpass::Kind::Touch {
                        installing.pending = Some(question);
                    }
                }
                Update::AskEnded(id) => {
                    app.ask_ended(id);
                    if installing.pending.as_ref().is_some_and(|question| question.id == id) {
                        installing.pending = None;
                    }
                }
                Update::Action(progress) => {
                    if let Progress::Done { ok, lines } = &progress {
                        let title = app.action.as_ref().map(super::action::Action::title).unwrap_or_default();
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
            _ = tick.tick() => app.tick(),
        }
    }
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
