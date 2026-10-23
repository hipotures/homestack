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
    Startup(&'static str),
    IntegrationTest(u64, super::integrations::TestReport),
    PangolinTest(u64, super::pangolin::Report),
    PangolinDiscovery(u64, super::pangolin::Discovery),
    PangolinApi(u64, Box<super::pangolin_api::Api>),
    PangolinTunnel(u64, super::pangolin_tunnel::Event),
    PangolinKey(u64, super::pangolin_key::Outcome),
    PangolinPermanent(u64, super::pangolin_permanent::Outcome),
    PangolinAsk(u64, AskEvent),
    Status(String, Value),
    StatusRejected(String),
    ApiUpdates(u64, Result<super::reinstall::Images, String>),
    Storage(Result<Value, String>),
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
    /// The server's pools, default home size (MiB) and workspace network.
    Defaults(Option<String>, Option<String>, Option<u64>, hs_model::config::Network),
    /// The images of the reinstall box of these containers.
    Images(Vec<u32>, Result<super::reinstall::Images, String>),
}

/// The terminal in raw mode on the alternate screen; put back on drop,
/// also after a panic.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    copying: bool,
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
            copying: false,
        })
    }

    /// Freeze the displayed text and let the terminal handle mouse selection.
    fn toggle_copy(&mut self, app: &App) -> io::Result<()> {
        if self.copying {
            execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
            self.copying = false;
        } else {
            self.terminal.draw(|frame| {
                draw(frame, app);
                let area = frame.area();
                if area.height > 0 {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new(
                            "Copy mode: select text with the mouse and use your terminal's Copy. F2 or Esc returns.",
                        )
                        .style(
                            ratatui::style::Style::default()
                                .fg(app.palette.accent)
                                .bg(app.palette.background),
                        ),
                        ratatui::layout::Rect::new(area.x, area.bottom() - 1, area.width, 1),
                    );
                }
            })?;
            execute!(io::stdout(), crossterm::event::DisableMouseCapture)?;
            self.copying = true;
        }
        Ok(())
    }
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        crossterm::event::DisableBracketedPaste,
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
    let code = super::shell::command(target)
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
/// not answer, has no configuration yet, or holds another configuration
/// than the others (a client switching to it would get other behaviour).
async fn health(client: &Arc<Client>) -> Vec<(String, Option<String>)> {
    let urls: Vec<String> = client.urls().map(ToString::to_string).collect();
    let mut checks = tokio::task::JoinSet::new();
    for (index, url) in urls.into_iter().enumerate() {
        let client = Arc::clone(client);
        checks.spawn(async move {
            let answer = tokio::time::timeout(Duration::from_secs(2), client.config_version_on(index))
                .await
                .unwrap_or_else(|_| {
                    Err(hs_client::ClientError::Unreachable(
                        "Server health check timed out".to_owned(),
                    ))
                });
            (index, url, answer)
        });
    }
    let mut answers = Vec::new();
    while let Some(result) = checks.join_next().await {
        if let Ok(answer) = result {
            answers.push(answer);
        }
    }
    answers.sort_by_key(|(index, _, _)| *index);
    // The version most servers hold is the expected one.
    let held: Vec<&String> = answers
        .iter()
        .filter_map(|(_, _, answer)| answer.as_ref().ok().and_then(Option::as_ref))
        .collect();
    let common = held
        .iter()
        .max_by_key(|version| held.iter().filter(|other| other == version).count())
        .map(|version| (*version).clone());
    answers
        .into_iter()
        .map(|(_, url, answer)| {
            let error = match answer {
                Ok(Some(version)) if common.as_ref().is_none_or(|common| *common == version) => None,
                Ok(Some(version)) => Some(format!(
                    "This server holds configuration {}, the others {}; send the same server.toml to every server.",
                    &version[..12.min(version.len())],
                    common.as_deref().map_or("", |common| &common[..12.min(common.len())])
                )),
                Ok(None) => Some("The server has no configuration yet; upload one with PUT /v1/config.".to_owned()),
                Err(error) => Some(client_error(error).0),
            };
            (url, error)
        })
        .collect()
}

/// Read the status and the servers' health once; nothing is told once
/// `stop` is set (a new client.toml replaced this client meanwhile).
async fn read_status(client: &Arc<Client>, updates: &mpsc::UnboundedSender<Update>, stop: &AtomicBool) -> bool {
    let servers = health(client).await;
    // The servers' dots at once; the status takes a few seconds more.
    if !stop.load(Ordering::SeqCst) {
        let _ = updates.send(Update::Servers(servers.clone()));
    }
    let parallel = super::state::PARALLEL.load(Ordering::SeqCst);
    let answer = client.get_from(&format!("/v1/status?parallel={parallel}")).await;
    if stop.load(Ordering::SeqCst) {
        return false;
    }
    match answer {
        Ok((url, status)) => {
            if let Err(error) = serde_json::from_value::<hs_model::status::GlobalStatus>(status.clone()) {
                let _ = updates.send(Update::StatusRejected(format!(
                    "Invalid server status: {error}. Automatic reads stopped."
                )));
                return false;
            }
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
    true
}

/// Read the pools, home size and network for a new workspace; retry if the
/// server configuration is not available.
async fn read_defaults(client: &Client, updates: &mpsc::UnboundedSender<Update>, stop: &AtomicBool) -> bool {
    let Ok((_, capabilities)) = client.get_from("/v1/capabilities").await else {
        return false;
    };
    let pool = capabilities["installation"]["pool"].as_str().map(str::to_owned);
    let gold_pool = capabilities["installation"]["gold_pool"].as_str().map(str::to_owned);
    let config = match client.get_from("/v1/config").await {
        Ok((_, stored)) => stored
            .as_str()
            .and_then(|text| hs_model::config::Config::parse(text).ok()),
        Err(_) => None,
    };
    let Some(config) = config else {
        return false;
    };
    let home = mib_of(&config.default_home_size);
    let network = config.network;
    if !stop.load(Ordering::SeqCst) {
        let _ = updates.send(Update::Defaults(pool, gold_pool, home, network));
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
        let _ = updates.send(Update::Startup("Reading the status from the servers"));
        let mut defaults_read = false;
        loop {
            if !read_status(&client, &updates, &stop).await {
                return;
            }
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
                let report = discovery_report(Some(&client), system.as_ref(), &home).await;
                let _ = updates.send(Update::Report("this desktop".to_owned(), discovery_lines(&report)));
            });
        }
        Request::Quit
        | Request::TestIntegration(_, _)
        | Request::TestPangolin(_, _)
        | Request::DiscoverPangolin(_, _)
        | Request::PangolinApi(_, _, _, _)
        | Request::PangolinTunnel(_, _)
        | Request::StopPangolinTunnel(_)
        | Request::VerifyPangolinKey(_)
        | Request::PangolinPermanent(_)
        | Request::PangolinSites
        | Request::CopyJson
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
        | Request::Installed
        | Request::UpdateApi => {}
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
    server_domain: Option<String>,
    server_image: Option<String>,
    /// Sites mode: what the Pangolin wizard hands the worker.
    sites: Option<super::pangolin_sites::Context>,
    /// Service mode: the manifest revision to deploy.
    service: Option<crate::services::deploy::Source>,
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
        let mode = app.install.as_ref().map_or(Mode::Install, |install| install.mode);
        // A Site and a service published through Pangolin use its SSH tunnel.
        let quiet = mode == Mode::Service
            && !self
                .service
                .as_ref()
                .is_some_and(|source| source.is_site() || source.manifest.pangolin.is_some());
        if !quiet {
            self.bridge(app, updates);
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let client_path = config::default_path(&ctx.home, ctx.config_home.as_deref());
        let token_dir = client_path
            .parent()
            .map_or_else(|| ctx.home.clone(), std::path::Path::to_path_buf);
        let sounds = config::load(&client_path)
            .map(|loaded| Sounds::from_table(loaded.client.sounds.as_ref()))
            .unwrap_or_default();
        let setup = Setup {
            mode,
            system: Arc::clone(&ctx.system),
            home: ctx.home.clone(),
            token_dir,
            client_path,
            servers: Arc::clone(&ctx.servers),
            sounds,
            ssh_prefix: if quiet {
                Vec::new()
            } else {
                self.bridge.as_ref().map(Bridge::command_prefix).unwrap_or_default()
            },
            stop: Arc::clone(&self.stop),
            server_image: self.server_image.clone(),
            server_domain: self.server_domain.clone(),
            handle: tokio::runtime::Handle::current(),
            sites: (mode == Mode::Sites).then(|| self.sites.clone()).flatten(),
            service: (mode == Mode::Service).then(|| self.service.clone()).flatten(),
            offset: app.utc_offset,
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
pub async fn run(ctx: &mut Context, options: super::Options) -> Result<i32, AppError> {
    let path = Preferences::default_path(&ctx.home, ctx.config_home.as_deref());
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let (mut preferences, problem) = match Preferences::load(&path) {
        Ok(preferences) => (preferences, None),
        Err(error) => (Preferences::fallback(&path), Some(error.0)),
    };
    let explicit_install = options.ssh.is_some() || options.server_domain.is_some();
    if let Some(host) = options.ssh {
        preferences
            .table
            .insert("install_host".to_owned(), toml::Value::String(host));
        preferences
            .table
            .insert("install_method".to_owned(), toml::Value::String("ssh".to_owned()));
    }
    let mut app = App::new(preferences, no_color)?;
    app.profile = std::env::var("HOMESTACK_ACTIVE_PROFILE").unwrap_or_else(|_| "default".to_owned());
    if app.profile == "e2e" {
        app.lab_gold_vmid = std::env::var("HOMESTACK_E2E_GOLD_VMID")
            .ok()
            .and_then(|id| id.parse().ok());
    }
    app.log_file = Some(std::path::PathBuf::from("/tmp/tui.log"));
    let clock = Clock::new();
    app.clock = clock.now();
    app.today = clock.today();
    app.load_earlier();
    if let Some(problem) = problem {
        app.log(&clock.now(), &format!("{problem}; the HomeStack theme is used"));
    }
    let (updates, mut received) = mpsc::unbounded_channel::<Update>();
    app.set_keys(public_keys(&ctx.home));
    if let Ok(deployment) = hs_deploy::config::DeployConfig::load(&deploy_toml(ctx)) {
        app.api_instances = deployment.instances.iter().map(|instance| instance.ctid).collect();
    }
    let first_start = !config::default_path(&ctx.home, ctx.config_home.as_deref()).exists();
    let mut connection = if first_start || explicit_install {
        if app.open_install(Mode::Install, first_start) == Some(Request::HerdrTabs) {
            match crate::root_herdr::tabs(ctx.system.as_ref()) {
                Ok(tabs) => app.set_herdr_tabs(tabs.into_iter().map(|tab| (tab.name, tab.title)).collect()),
                Err(error) => {
                    if let Some(install) = app.install.as_mut() {
                        install.error = Some(error.0.clone());
                    }
                    app.log(&clock.now(), &error.0);
                }
            }
        }
        app.log(
            &clock.now(),
            if first_start {
                "no client.toml: the install screen"
            } else {
                "TUI install arguments: the install screen"
            },
        );
        Connection::none()
    } else {
        Connection::open(ctx, &updates).unwrap_or_else(|notice| {
            app.set_notice(Some(notice));
            Connection::none()
        })
    };
    let mut installing = Installing {
        server_domain: options.server_domain,
        server_image: options.server_image,
        ..Installing::default()
    };
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
    let mut api_check_started = false;
    // At the start and then every hour: a new API image shows up by itself.
    let mut api_check_at = std::time::Instant::now();
    let mut api_check_generation = 0_u64;
    let mut storage_loading = false;
    let mut storage_visible = false;
    let mut storage_next = std::time::Instant::now();
    let mut integration_paste = false;
    let mut pangolin_worker: Option<(super::pangolin::Pending, Bridge)> = None;
    let mut pangolin_tunnel: Option<(super::pangolin_tunnel::Session, Bridge)> = None;
    let mut pangolin_question: Option<crate::askpass::Question> = None;
    let mut pangolin_key: Option<super::pangolin::Pending> = None;
    let mut pangolin_permanent: Option<super::pangolin::Pending> = None;
    loop {
        if let Some((session, _)) = &pangolin_tunnel {
            let valid = matches!(&app.overlay,Some(Overlay::Pangolin(w)) if w.tunnel.id==session.id && w.tunnel_spec().as_ref()==Some(&session.spec));
            if !valid {
                if let Some(question) = pangolin_question.take() {
                    app.ask_ended(question.id);
                    question.answer(None);
                }
                pangolin_tunnel = None;
                if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
                    w.end_tunnel(super::pangolin_tunnel::State::Outdated);
                }
            } else if session.finished() {
                if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut()
                    && (w.tunnel.current.state == super::pangolin_tunnel::State::Connected || w.tunnel.busy())
                {
                    w.tunnel_event(
                        session.id,
                        super::pangolin_tunnel::Event {
                            state: super::pangolin_tunnel::State::Lost,
                            error: Some(super::pangolin_tunnel::Error::Lost),
                            port: None,
                        },
                    );
                }
                if matches!(&app.overlay,Some(Overlay::Pangolin(w)) if !w.tunnel.busy()) {
                    if let Some(question) = pangolin_question.take() {
                        app.ask_ended(question.id);
                        question.answer(None);
                    }
                    pangolin_tunnel = None;
                }
            }
        }
        if let Some(worker) = &pangolin_permanent
            && !matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.permanent.running == worker.id && !w.permanent.cancel_requested && w.permanent_current() && w.permanent_available())
        {
            worker.cancel.store(true, Ordering::SeqCst);
        }
        // A verification never outlives its tunnel session, edit or wizard.
        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut()
            && w.bootstrap.running != 0
            && (!w.tunnel_connected() || !pangolin_tunnel.as_ref().is_some_and(|(s, _)| s.id == w.tunnel.id))
        {
            w.cancel_bootstrap(super::pangolin_key::Problem::Lost);
        }
        if pangolin_key.as_ref().is_some_and(
            |worker| !matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.bootstrap.running == worker.id),
        ) {
            pangolin_key = None;
        }
        if let Some((worker, _)) = &pangolin_worker {
            let active = matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.testing && w.test_id == worker.id);
            if !active {
                worker.cancel.store(true, Ordering::SeqCst);
                if let Some(question) = pangolin_question.take() {
                    app.ask_ended(question.id);
                    question.answer(None);
                }
            }
        }
        app.utc_offset = clock.offset;
        app.clock = clock.now();
        app.today = clock.today();
        details.update(app.details_target(), connection.client.as_ref());
        let deployment = deploy_toml(ctx);
        if !api_check_started
            && std::time::Instant::now() >= api_check_at
            && app.install.is_none()
            && deployment.is_file()
        {
            api_check_started = true;
            api_check_generation += 1;
            let generation = api_check_generation;
            // Only the first check shows "API …"; later ones keep the last answer.
            app.api_updates_checking = app.api_updates.is_none();
            let updates = updates.clone();
            let offset = clock.offset;
            tokio::spawn(async move {
                let check = async {
                    let config = hs_deploy::config::DeployConfig::load(&deployment)?;
                    let targets: Vec<_> = super::reinstall::api_servers(&config)
                        .await?
                        .into_iter()
                        .map(|(ctid, _)| ctid)
                        .collect();
                    super::reinstall::read(&deployment, &targets, offset).await
                };
                let result = tokio::time::timeout(Duration::from_secs(120), check)
                    .await
                    .unwrap_or_else(|_| Err("API image check timed out".to_owned()));
                let _ = updates.send(Update::ApiUpdates(generation, result));
            });
        }
        let visible = matches!(app.view, 2 | 3);
        if visible
            && !storage_loading
            && (!storage_visible
                || (app.storage_overview.as_ref().is_some_and(Result::is_ok)
                    && std::time::Instant::now() >= storage_next))
            && let Some(client) = &connection.client
        {
            storage_loading = true;
            let client = Arc::clone(client);
            let updates = updates.clone();
            let stop = Arc::clone(&connection.stop);
            tokio::spawn(async move {
                let result = match client.get("/v1/storage/overview").await {
                    Ok(value) => Ok(value),
                    Err(error) => Err(client_error(error).0),
                };
                if !stop.load(Ordering::SeqCst) {
                    let _ = updates.send(Update::Storage(result));
                }
            });
        }
        storage_visible = visible;
        let editing_credentials = matches!(
            app.overlay,
            Some(super::state::Overlay::Integration(_) | super::state::Overlay::Pangolin(_))
        );
        if editing_credentials != integration_paste {
            if editing_credentials {
                execute!(io::stdout(), crossterm::event::EnableBracketedPaste)
            } else {
                execute!(io::stdout(), crossterm::event::DisableBracketedPaste)
            }
            .map_err(|error| AppError::new(format!("Cannot change paste mode: {error}")))?;
            integration_paste = editing_credentials;
        }
        if !screen.copying {
            screen
                .terminal
                .draw(|frame| draw(frame, &app))
                .map_err(|error| AppError::new(format!("Cannot draw: {error}")))?;
        }
        tokio::select! {
            Some(event) = events.recv() => {
                let request = match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press
                        && (key.code == event::KeyCode::F(2) || (screen.copying && key.code == event::KeyCode::Esc)) => {
                        screen.toggle_copy(&app).map_err(|error| AppError::new(format!("Cannot change copy mode: {error}")))?;
                        None
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press && (!screen.copying ||
                        (key.modifiers.contains(event::KeyModifiers::CONTROL) && matches!(key.code, event::KeyCode::Char('c' | 'q')))) => app.key(key),
                    Event::Mouse(mouse) if !screen.copying => app.mouse(mouse),
                    Event::Paste(mut text) if !screen.copying => {
                        match app.overlay.as_mut() {
                            Some(Overlay::Integration(editor)) => editor.paste(&text),
                            Some(Overlay::Pangolin(wizard)) => wizard.paste(&text),
                            _ => {}
                        }
                        // Pasted text may be a credential: erase the event buffer.
                        zeroize::Zeroize::zeroize(&mut text);
                        None
                    }
                    _ => None,
                };
                if let Some(request) = request {
                    if request == Request::Quit {
                        if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() {
                            wizard.clear_bootstrap();
                        }
                        for (_, worker) in setup_workers.drain() {
                            worker.close();
                        }
                        installing.send(Command::Cancel, &mut app, ctx, &updates);
                        return Ok(0);
                    }
                    match request {
                        Request::StopPangolinTunnel(cancelled)=>{
                            if let Some(question)=pangolin_question.take(){app.ask_ended(question.id);question.answer(None);}
                            pangolin_tunnel=None;
                            if let Some(Overlay::Pangolin(w))=app.overlay.as_mut(){w.end_tunnel(if cancelled{super::pangolin_tunnel::State::Cancelled}else{super::pangolin_tunnel::State::Disconnected});}
                        }
                        Request::PangolinSites => {
                            let Some(Overlay::Pangolin(w)) = app.overlay.as_ref() else { continue };
                            let endpoint = pangolin_tunnel.as_ref().filter(|(s, _)| s.id == w.tunnel.id).zip(w.tunnel_spec()).and_then(|((s, _), spec)| s.live_endpoint(&spec));
                            match endpoint.and_then(|endpoint| w.sites_context(endpoint)) {
                                Some(context) => {
                                    installing.send(Command::Cancel, &mut app, ctx, &updates);
                                    installing.sites = Some(context);
                                    app.open_sites();
                                }
                                None => {
                                    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
                                        w.message = "Live tunnel unavailable. Reconnect Step 5.".to_owned();
                                    }
                                }
                            }
                        }
                        Request::PangolinPermanent(id) => {
                            let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() else { continue };
                            let endpoint = pangolin_tunnel.as_ref().filter(|(s, _)| s.id == w.tunnel.id).zip(w.tunnel_spec()).and_then(|((s, _), spec)| s.live_endpoint(&spec));
                            let (Some(endpoint), Some(input)) = (endpoint, w.permanent_input(id)) else {
                                w.complete_permanent(id, super::pangolin_permanent::Outcome::failed("Live tunnel unavailable. Reconnect Step 5.")); continue;
                            };
                            let cancel = Arc::new(AtomicBool::new(false));
                            pangolin_permanent = Some(super::pangolin::Pending { id, cancel: Arc::clone(&cancel) });
                            let sender = updates.clone();
                            tokio::task::spawn_blocking(move || { let result = super::pangolin_permanent::execute(&endpoint, input, &cancel); let _ = sender.send(Update::PangolinPermanent(id, result)); });
                        }
                        Request::VerifyPangolinKey(id) => {
                            let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() else { continue };
                            let endpoint = pangolin_tunnel
                                .as_ref()
                                .filter(|(session, _)| session.id == w.tunnel.id)
                                .zip(w.tunnel_spec())
                                .and_then(|((session, _), spec)| session.live_endpoint(&spec));
                            let (Some(endpoint), Some((organization, credential))) = (endpoint, w.bootstrap_input(id)) else {
                                w.complete_bootstrap(id, super::pangolin_key::Outcome::failed(super::pangolin_key::Problem::Disconnected));
                                continue;
                            };
                            let cancel = Arc::new(AtomicBool::new(false));
                            pangolin_key = Some(super::pangolin::Pending { id, cancel: Arc::clone(&cancel) });
                            let sender = updates.clone();
                            tokio::task::spawn_blocking(move || {
                                let outcome = super::pangolin_key::verify(&endpoint, &organization, &credential, &cancel);
                                drop(credential);
                                let _ = sender.send(Update::PangolinKey(id, outcome));
                            });
                        }
                        Request::PangolinTunnel(mut id, spec) => {
                            if let Some((session, _)) = &pangolin_tunnel
                                && session.id == id && session.spec == spec && session.endpoint(&spec).is_some() {
                                session.verify();
                                continue;
                            }
                            if pangolin_tunnel.is_some() {
                                id = super::pangolin::next_test();
                                if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() {
                                    w.tunnel.id = id;
                                    w.tunnel.current.state = super::pangolin_tunnel::State::Connecting;
                                    w.tunnel.current.port = None;
                                }
                            }
                            pangolin_tunnel=None;
                            if pangolin_worker.is_some(){
                                if let Some(Overlay::Pangolin(w))=app.overlay.as_mut(){w.tunnel_event(id,super::pangolin_tunnel::Event{state:super::pangolin_tunnel::State::Failed,error:Some(super::pangolin_tunnel::Error::Startup),port:None});}
                                continue;
                            }
                            let program=std::env::current_exe().map_err(|_|AppError::new("Cannot locate SSH askpass program"))?;
                            let (bridge,asks)=match Bridge::start(program){Ok(v)=>v,Err(_)=>{
                                if let Some(Overlay::Pangolin(w))=app.overlay.as_mut(){w.tunnel_event(id,super::pangolin_tunnel::Event{state:super::pangolin_tunnel::State::Failed,error:Some(super::pangolin_tunnel::Error::Startup),port:None});}
                                continue;
                            }};
                            let sender=updates.clone();
                            std::thread::spawn(move||{for ask in asks{if sender.send(Update::PangolinAsk(id,ask)).is_err(){break;}}});
                            let client_path=config::default_path(&ctx.home,ctx.config_home.as_deref());
                            let sounds=config::load(&client_path).map(|loaded|Sounds::from_table(loaded.client.sounds.as_ref())).unwrap_or_default();
                            let sender=updates.clone();
                            let notify=Arc::new(move|event|{let _=sender.send(Update::PangolinTunnel(id,event));});
                            let session=super::pangolin_tunnel::Session::start(id,spec,Arc::clone(&ctx.system),bridge.command_prefix(),sounds,crate::touch::default_player(),notify);
                            pangolin_tunnel=Some((session,bridge));
                        }
                        request @ (Request::TestPangolin(_, _) | Request::DiscoverPangolin(_, _) | Request::PangolinApi(_, _, _, _)) => {
                            let (id, target, api) = match request {
                                Request::PangolinApi(id, target, discovery, plan) => (id, target, Some((discovery, plan))),
                                Request::TestPangolin(id, target) | Request::DiscoverPangolin(id, target) => (id, target, None),
                                _ => unreachable!(),
                            };
                            let discovery = matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.discovering);
                            if pangolin_worker.is_some() {
                                if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() {
                                    wizard.worker_failed(id, "Previous SSH operation is stopping; retry shortly.");
                                }
                                continue;
                            }
                            let program = std::env::current_exe().map_err(|_| AppError::new("Cannot locate SSH askpass program"))?;
                            let (bridge, asks) = match Bridge::start(program) {
                                Ok(value) => value,
                                Err(_) => {
                                    if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() {
                                        wizard.worker_failed(id, "Cannot start the existing SSH askpass bridge.");
                                    }
                                    continue;
                                }
                            };
                            let sender = updates.clone();
                            std::thread::spawn(move || {
                                for ask in asks {
                                    if sender.send(Update::PangolinAsk(id, ask)).is_err() { break; }
                                }
                            });
                            let prefix = bridge.command_prefix();
                            let cancel = Arc::new(AtomicBool::new(false));
                            pangolin_worker = Some((super::pangolin::Pending { id, cancel: Arc::clone(&cancel) }, bridge));
                            let client_path = config::default_path(&ctx.home, ctx.config_home.as_deref());
                            let sounds = config::load(&client_path)
                                .map(|loaded| Sounds::from_table(loaded.client.sounds.as_ref()))
                                .unwrap_or_default();
                            let system = Arc::clone(&ctx.system);
                            let sender = updates.clone();
                            tokio::task::spawn_blocking(move || {
                                let started = std::time::Instant::now();
                                if let Some((discovery, plan)) = api {
                                    let result = super::pangolin_api::execute(system.as_ref(), &target, &discovery, plan.as_deref(), &prefix, &cancel, &sounds, crate::touch::default_player());
                                    let _ = sender.send(Update::PangolinApi(id, Box::new(result)));
                                    return;
                                }
                                if discovery {
                                    let result = super::pangolin::discover(system.as_ref(), &target, &prefix, &cancel, &sounds, crate::touch::default_player());
                                    let _ = sender.send(Update::PangolinDiscovery(id, result));
                                    return;
                                }
                                let result = super::pangolin::test_ssh(system.as_ref(), &target, &prefix, &cancel, &sounds, crate::touch::default_player());
                                let report = super::pangolin::Report {
                                    target, at: String::new(), passed: result.is_ok(), detail: result.unwrap_or_else(|e| e),
                                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                                };
                                let _ = sender.send(Update::PangolinTest(id, report));
                            });
                        }
                        Request::TestIntegration(id, editor) => {
                            let sender = updates.clone();
                            tokio::spawn(async move {
                                let started = std::time::Instant::now();
                                let result = editor.test_connection().await;
                                let report = super::integrations::TestReport::completed(&editor, result, u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                                let _ = sender.send(Update::IntegrationTest(id, report));
                            });
                        }
                        Request::CopyJson => {
                            let size = screen.terminal.size().map_err(|error| AppError::new(format!("Cannot read terminal size: {error}")))?;
                            let result = app.displayed_json(ratatui::layout::Rect::new(0, 0, size.width, size.height))
                                .ok_or_else(|| "No API data loaded to copy".to_owned())
                                .and_then(|json| serde_json::to_string_pretty(&json).map_err(|error| error.to_string()))
                                .and_then(|text| super::clipboard::copy(&mut io::stdout(), &text).map_err(|error| error.to_string()));
                            let message = match result {
                                Ok(()) => "JSON sent to the terminal clipboard".to_owned(),
                                Err(error) => format!("Cannot copy JSON: {error}"),
                            };
                            app.log(&clock.now(), &message);
                        }
                        Request::Act(start) if start.kind == Kind::Migrate && start.parameters["service"] == true => {
                            let (sender, mut decisions) = mpsc::unbounded_channel();
                            decide = Some(sender);
                            let updates = updates.clone();
                            let deploy_toml = deploy_toml(ctx);
                            // A Site connector's move is verified through its own Pangolin tunnel.
                            let prefix = installing.bridge(&mut app, &updates);
                            let site_access = (!prefix.is_empty()).then(|| {
                                let sounds = config::load(&config::default_path(&ctx.home, ctx.config_home.as_deref()))
                                    .map(|loaded| Sounds::from_table(loaded.client.sounds.as_ref())).unwrap_or_default();
                                super::reinstall::SiteAccess { system: Arc::clone(&ctx.system), prefix, sounds }
                            });
                            tokio::spawn(async move {
                                let told = updates.clone();
                                let tell = move |progress: Progress| {
                                    let _ = told.send(Update::Action(progress));
                                };
                                let say = move |line: &str| {
                                    let _ = updates.send(Update::Log(line.trim().to_owned()));
                                };
                                super::reinstall::migrate_work(&deploy_toml, &start, &mut decisions, &tell, &say, site_access).await;
                            });
                        }
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
                        Request::UpdateApi => {
                            let path = deploy_toml(ctx);
                            match hs_deploy::config::DeployConfig::load(&path) {
                                Ok(config) => {
                                    // The startup check found the API containers in the cluster.
                                    let servers = match &app.api_updates {
                                        Some(Ok(images)) if !images.servers.is_empty() => images
                                            .servers
                                            .iter()
                                            .map(|server| (server.vmid, server.name.clone()))
                                            .collect(),
                                        _ => config.instances.iter().map(|instance| (instance.ctid, instance.name.clone())).collect(),
                                    };
                                    if let Some(Request::Images(targets)) = app.open_image_update(servers) {
                                        let updates = updates.clone();
                                        let offset = clock.offset;
                                        tokio::spawn(async move {
                                            let images = super::reinstall::read(&path, &targets, offset).await;
                                            let _ = updates.send(Update::Images(targets, images));
                                        });
                                    }
                                }
                                Err(error) => app.overlay = Some(Overlay::Report {
                                    title: "Update API containers".to_owned(), lines: vec![error],
                                }),
                            }
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
                        Request::Act(start) if start.kind == Kind::Addkey && start.parameters["alien_ssh"].is_string() => {
                            if let (Some(client), Some(desktop)) = (connection.client.clone(), connection.desktop.clone()) {
                                let ssh_prefix = installing.bridge(&mut app, &updates);
                                if ssh_prefix.is_empty() {
                                    app.progress(Progress::Done { ok: false, lines: vec!["Could not start SSH authentication prompts.".to_owned()] });
                                    continue;
                                }
                                let system = Arc::clone(&ctx.system);
                                let (sender, mut decisions) = mpsc::unbounded_channel();
                                decide = Some(sender);
                                let updates = updates.clone();
                                tokio::spawn(async move {
                                    let tell = move |progress: Progress| { let _ = updates.send(Update::Action(progress)); };
                                    if let Err(message) = super::alien_keys::work(&client, &desktop, start, ssh_prefix, system, &mut decisions, &tell).await {
                                        tell(Progress::Done { ok: false, lines: message.lines().map(str::to_owned).collect() });
                                    }
                                });
                            }
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
                            if command == Command::Apply
                                && app.install.as_ref().is_some_and(|install| install.mode == Mode::Uninstall)
                            {
                                connection.stop.store(true, Ordering::SeqCst);
                                connected = None;
                            }
                            app.log(&clock.now(), &format!("install: {command:?}"));
                            installing.send(command, &mut app, ctx, &updates);
                        }
                        Request::Answer(answer) => {
                            if let Some(question) = pangolin_question.take() {
                                let cancelled = answer.is_none();
                                question.answer(answer);
                                if cancelled && pangolin_tunnel.is_some()
                                    && matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.tunnel.busy()) {
                                    pangolin_tunnel = None;
                                    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() { w.end_tunnel(super::pangolin_tunnel::State::Cancelled); }
                                }
                            } else if let Some(question) = installing.pending.take() {
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
                                    Ask::Deploy => {
                                        if let Some(source) = app.setup.as_mut().and_then(|setup| setup.deploy_request.take()) {
                                            app.open_service_deploy(&source.manifest.name);
                                            installing.service = Some(source);
                                            let host = app.preferences.table.get("install_host").and_then(toml::Value::as_str).unwrap_or_default().to_owned();
                                            installing.send(
                                                Command::Connect { method: super::install::Method::Ssh, target: host },
                                                &mut app,
                                                ctx,
                                                &updates,
                                            );
                                        }
                                        None
                                    }
                                    Ask::Services => app
                                        .setup
                                        .as_mut()
                                        .and_then(|setup| setup.services_request.take())
                                        .map(|(repository, branch)| Work::Services(repository, branch)),
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
                            open_ssh(&mut app, vmid, &mut screen, &reader);
                        }
                        Request::HerdrTabs => match crate::root_herdr::tabs(ctx.system.as_ref()) {
                            Ok(tabs) => app.set_herdr_tabs(
                                tabs.into_iter().map(|tab| (tab.name, tab.title)).collect(),
                            ),
                            Err(error) => {
                                if let Some(install) = app.install.as_mut() { install.error = Some(error.0.clone()); }
                                app.log(&clock.now(), &error.0);
                            }
                        },
                        Request::Installed => {
                            details.cancel();
                            installing.send(Command::Cancel, &mut app, ctx, &updates);
                            connection.stop.store(true, Ordering::SeqCst);
                            connected = None;
                            let has_client = config::default_path(&ctx.home, ctx.config_home.as_deref()).exists();
                            if app.after_installed(has_client) == Some(Request::HerdrTabs) {
                                match crate::root_herdr::tabs(ctx.system.as_ref()) {
                                    Ok(tabs) => app.set_herdr_tabs(
                                        tabs.into_iter().map(|tab| (tab.name, tab.title)).collect(),
                                    ),
                                    Err(error) => {
                                        if let Some(install) = app.install.as_mut() { install.error = Some(error.0.clone()); }
                                        app.log(&clock.now(), &error.0);
                                    }
                                }
                            }
                            connection = if has_client {
                                Connection::open(ctx, &updates).unwrap_or_else(|notice| {
                                    app.set_notice(Some(notice));
                                    Connection::none()
                                })
                            } else {
                                app.log(&clock.now(), "no client.toml: the install screen");
                                Connection::none()
                            };
                        }
                        other => start(&other, connection.client.as_ref(), ctx, &updates),
                    }
                }
            }
            Some(update) = received.recv() => match update {
                Update::PangolinTunnel(id, mut event) => {
                    if event.state == super::pangolin_tunnel::State::Connected
                        && !pangolin_tunnel.as_ref().is_some_and(|(session, _)| session.id == id && session.endpoint(&session.spec).is_some()) {
                        event = super::pangolin_tunnel::Event { state: super::pangolin_tunnel::State::Lost, error: Some(super::pangolin_tunnel::Error::Lost), port: None };
                    }
                    if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() { w.tunnel_event(id, event); }
                }
                Update::PangolinPermanent(id, result) => {
                    if pangolin_permanent.as_ref().is_some_and(|w| w.id == id) {
                        pangolin_permanent = None;
                        if let Some(Overlay::Pangolin(w)) = app.overlay.as_mut() { w.complete_permanent(id, result); }
                    }
                }
                Update::PangolinKey(id, outcome) => {
                    if pangolin_key.as_ref().is_some_and(|worker| worker.id == id) {
                        pangolin_key = None;
                        if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() { wizard.complete_bootstrap(id, outcome); }
                    }
                }
                Update::PangolinApi(id, report) => {
                    if pangolin_worker.as_ref().is_some_and(|(worker, _)| worker.id == id) {
                        pangolin_worker = None;
                        if let Some(question) = pangolin_question.take() { app.ask_ended(question.id); question.answer(None); }
                        if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() { wizard.complete_api(id, *report); }
                    }
                }
                Update::PangolinDiscovery(id, report) => {
                    if pangolin_worker.as_ref().is_some_and(|(worker, _)| worker.id == id) {
                        pangolin_worker = None;
                        if let Some(question) = pangolin_question.take() { app.ask_ended(question.id); question.answer(None); }
                        if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() { wizard.complete_discovery(id, report); }
                    }
                }
                Update::PangolinTest(id, report) => {
                    if pangolin_worker.as_ref().is_some_and(|(worker, _)| worker.id == id) {
                        pangolin_worker = None;
                        if let Some(question) = pangolin_question.take() {
                            app.ask_ended(question.id);
                            question.answer(None);
                        }
                        if let Some(Overlay::Pangolin(wizard)) = app.overlay.as_mut() { wizard.complete(id, report); }
                    }
                }
                Update::PangolinAsk(id, event) => {
                    let active = matches!(&app.overlay, Some(Overlay::Pangolin(w)) if (w.testing && w.test_id == id) || (w.tunnel.busy() && w.tunnel.id==id));
                    match event {
                        AskEvent::Ask(question) if active => {
                            let mut view = AskView::new(question.id, question.kind, &question.prompt);
                            view.key_names = crate::identity::key_names(&ctx.home, &question.prompt);
                            app.ask(view);
                            pangolin_question = Some(question);
                        }
                        AskEvent::Ask(question) => question.answer(None),
                        AskEvent::Ended(question_id) if (pangolin_worker.as_ref().is_some_and(|(worker, _)| worker.id == id) || pangolin_tunnel.as_ref().is_some_and(|(session,_)|session.id==id))
                            && pangolin_question.as_ref().is_some_and(|q| q.id == question_id) => {
                            app.ask_ended(question_id); pangolin_question = None;
                        }
                        _ => {}
                    }
                }
                Update::IntegrationTest(id, result) => app.integration_test_result(id, result),
                Update::Startup(message) => app.startup_message = Some(message),
                Update::Status(url, status) => {
                    let host = host(&url).split('.').next().unwrap_or_default().to_owned();
                    if connected.as_deref() != Some(host.as_str()) {
                        app.log(&clock.now(), &format!("status from {host}"));
                        connected = Some(host);
                    }
                    app.set_status(&status);
                }
                Update::ApiUpdates(generation, result) => {
                    if generation == api_check_generation {
                        app.set_api_updates(result);
                        api_check_started = false;
                        api_check_at = std::time::Instant::now() + Duration::from_secs(3600);
                    }
                }
                Update::StatusRejected(error) => {
                    app.startup_message = None;
                    app.log(&clock.now(), &error);
                    app.set_notice(Some(("The server returned invalid status data".to_owned(), vec![error])));
                }
                Update::Storage(result) => {
                    storage_loading = false;
                    storage_next = std::time::Instant::now() + REFRESH;
                    app.set_storage_json(result);
                }
                Update::Servers(servers) => app.set_servers(servers),
                Update::Images(targets, images) => app.set_images(&targets, images),
                Update::Defaults(pool, gold_pool, home, network) => {
                    app.pool = pool;
                    app.gold_pool = gold_pool;
                    app.home_default = home;
                    app.network = Some(network);
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
                Update::Install(event) => {
                    let reload = matches!(&event, super::install::Event::Done { .. })
                        && app.install.as_ref().is_some_and(|install| install.mode == Mode::Service);
                    app.install_event(event);
                    if reload {
                        match Connection::open(ctx, &updates) {
                            Ok(next) => {
                                connection.stop.store(true, Ordering::SeqCst);
                                connection = next;
                                connected = None;
                            }
                            Err(notice) => app.set_notice(Some(notice)),
                        }
                    }
                },
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
                            api_check_started = false;
                            api_check_at = std::time::Instant::now();
                        }
                        let outcome = if *ok { "done" } else { "failed" };
                        let first = lines.first().cloned().unwrap_or_default();
                        app.log(&clock.now(), &format!("{title}: {outcome}; {first}"));
                        if !*ok {
                            for line in lines.iter().skip(1).filter(|line| !line.is_empty()) {
                                app.log(&clock.now(), line);
                            }
                        }
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
                let _ = app.set_details(vmid, result);
            },
            _ = tick.tick() => app.tick(),
            _ = fast.tick(), if (app.status.is_none() && app.unreachable.is_empty())
                || matches!(&app.overlay, Some(Overlay::Pangolin(w)) if w.bootstrap.key_visible())
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

/// Open the selected VM through its desktop SSH alias.
fn open_ssh(app: &mut App, vmid: u32, screen: &mut Screen, reader: &Reader) {
    let Some(target) = super::shell::prepare(app, vmid) else {
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
fn public_keys(home: &std::path::Path) -> Vec<super::action::KeyFile> {
    crate::identity::public_keys(home)
        .into_iter()
        .map(|(path, name)| super::action::KeyFile { path, name })
        .collect()
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
