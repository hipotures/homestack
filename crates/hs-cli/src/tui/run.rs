//! The terminal and the event loop of the TUI: keys from a reader thread,
//! the status from the servers every few seconds, requests started by keys.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{execute, terminal};
use hs_client::Client;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc;

use super::draw::draw;
use super::state::{App, Overlay, Request};
use super::theme::Preferences;
use crate::app::{Context, client_error, discovery_report, transport_report};
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

/// Read the status and the servers' health now and every [`REFRESH`].
fn spawn_refresh(client: Arc<Client>, updates: mpsc::UnboundedSender<Update>) {
    tokio::spawn(async move {
        loop {
            let servers = health(&client).await;
            match client.get_from("/v1/status").await {
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
            if updates.is_closed() {
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
                    Err(error) => vec![error.0],
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
        Request::Quit => {}
    }
}

/// The client of `client.toml`, or why there is none.
fn client(ctx: &Context) -> Result<Client, (String, Vec<String>)> {
    let path = config::default_path(&ctx.home, ctx.config_home.as_deref());
    if !path.exists() {
        return Err((
            "HomeStack is not installed on this desktop yet.".to_owned(),
            vec![
                format!("There is no {}.", path.display()),
                "The install screen comes to the TUI in step 3 of phase 13b;".to_owned(),
                "until then quit (q) and run: homestack install".to_owned(),
            ],
        ));
    }
    let loaded = config::load(&path).map_err(|error| ("client.toml cannot be used".to_owned(), vec![error.0]))?;
    let settings = loaded
        .client_config(&ctx.home)
        .map_err(|error| ("client.toml cannot be used".to_owned(), vec![error.0]))?;
    Client::new(settings).map_err(|error| ("The client cannot start".to_owned(), vec![client_error(error).0]))
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
    let clock = Clock::new();
    if let Some(problem) = problem {
        app.log(&clock.now(), &format!("{problem}; the HomeStack theme is used"));
    }
    let (updates, mut received) = mpsc::unbounded_channel::<Update>();
    let client = match client(ctx) {
        Ok(client) => {
            let client = Arc::new(client);
            spawn_refresh(Arc::clone(&client), updates.clone());
            Some(client)
        }
        Err(notice) => {
            app.set_notice(Some(notice));
            None
        }
    };
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
                        return Ok(0);
                    }
                    start(&request, client.as_ref(), ctx, &updates);
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
            },
            _ = tick.tick() => {}
        }
    }
}
