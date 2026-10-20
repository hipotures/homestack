//! Owned, session-only OpenSSH transport. No API credentials enter this module.
use super::pangolin::{Report, Wizard, validate_destination};
use crate::{
    system::System,
    touch::{Player, Sounds},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, ChildStderr, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub target: String,
    pub remote_port: u16,
    pub revision: String,
}
impl Spec {
    pub fn remote(&self) -> String {
        format!("127.0.0.1:{}", self.remote_port)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connecting,
    Connected,
    Verifying,
    Lost,
    Failed,
    Cancelled,
    Outdated,
}
impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Verifying => "Verifying",
            Self::Lost => "Connection lost",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Outdated => "Outdated",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Authentication,
    HostKey,
    Port,
    Startup,
    Remote,
    Response,
    Timeout,
    Cancelled,
    Lost,
    Config,
}
impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Authentication => "SSH authentication failed for the configured server.",
            Self::HostKey => "SSH server identity could not be verified.",
            Self::Port => "The selected local forwarding port is unavailable. Retry connection.",
            Self::Startup => "SSH forwarding could not be established. Check SSH access and retry.",
            Self::Remote => {
                "SSH connected, but Pangolin Integration API is unreachable. Check Integration API in Step 4."
            }
            Self::Response => "The remote endpoint did not return the expected Pangolin health response. Check Step 4.",
            Self::Timeout => "Connection or health verification timed out. Retry when ready.",
            Self::Cancelled => "Tunnel connection cancelled.",
            Self::Lost => "The SSH tunnel has closed. Reconnect to continue.",
            Self::Config => "Private endpoint or SSH configuration is invalid. Verify Steps 1 and 4.",
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub state: State,
    pub error: Option<Error>,
    pub port: Option<u16>,
}
impl Event {
    fn new(state: State, error: Option<Error>, port: Option<u16>) -> Self {
        Self { state, error, port }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub target: String,
    pub remote: String,
    pub at: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub id: u64,
    pub current: Event,
    pub history: Option<History>,
}
impl Default for View {
    fn default() -> Self {
        Self {
            id: 0,
            current: Event::new(State::Disconnected, None, None),
            history: None,
        }
    }
}
impl View {
    pub fn busy(&self) -> bool {
        matches!(self.current.state, State::Connecting | State::Verifying)
    }
    pub fn text(&self, offset: i64) -> String {
        let mut text = format!(
            "Status: {}\n{}",
            self.current.state.label(),
            self.current.error.map_or(
                "No API credentials used. Only the owned SSH process is managed.",
                Error::message
            )
        );
        if let Some(h) = &self.history {
            let date = Report {
                target: h.target.clone(),
                at: h.at.clone(),
                passed: true,
                detail: String::new(),
                duration_ms: 0,
            }
            .local_date(offset);
            text.push_str(&format!("\nLast successful health check: {date}\nServer: {}\nRemote API: {}\nHistorical result; reconnect after restarting HomeStack.",h.target,h.remote));
        }
        if let Some(port) = self.current.port {
            text.push_str(&format!("\nCurrent local endpoint: http://127.0.0.1:{port}/v1/"));
        }
        text.push_str("\nStartup: 45 seconds; health: 2 seconds; port allocation: 3 attempts.\nHealth requires HTTP 200 and the Pangolin Healthy JSON response, with no redirects, proxies or credentials.\nAn owned private control socket isolates this session from other SSH consumers. Leaving the wizard closes and reaps its SSH process. No SSH output is displayed or persisted.");
        text
    }
}
impl Wizard {
    pub fn tunnel_spec(&self) -> Option<Spec> {
        if self.api_status() != "Verified" {
            return None;
        }
        let api = self.api.as_ref()?;
        if !api.enabled || !api.running || api.access != "Loopback only" || api.port == 0 {
            return None;
        }
        validate_destination(&self.target).ok()?;
        // Require an explicit literal loopback host port, including explicit 80.
        // URL parsers normalize default ports away, so parse the approved shape.
        let port = api
            .endpoint
            .strip_prefix("http://127.0.0.1:")?
            .strip_suffix("/v1/")?
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)?;
        Some(Spec {
            target: self.target.clone(),
            remote_port: port,
            revision: format!("{}|{}|{}", api.installation, api.at, api.endpoint),
        })
    }
    pub fn tunnel_available(&self) -> bool {
        self.tunnel_spec().is_some()
    }
    pub fn begin_tunnel(&mut self) -> Option<(u64, Spec)> {
        if self.selected_step != 4 || self.testing || self.tunnel.busy() || self.help_open || self.report_open {
            return None;
        }
        let spec = self.tunnel_spec()?;
        let connected = self.tunnel.current.state == State::Connected;
        if !connected {
            self.tunnel.id = super::pangolin::next_test();
        }
        self.tunnel.current = Event::new(
            if connected { State::Verifying } else { State::Connecting },
            None,
            self.tunnel.current.port,
        );
        self.message.clear();
        Some((self.tunnel.id, spec))
    }
    pub fn tunnel_event(&mut self, id: u64, event: Event) {
        if self.tunnel.id != id {
            return;
        }
        if event.state == State::Connected {
            if let Some(spec) = self.tunnel_spec() {
                let h = History {
                    target: spec.target.clone(),
                    remote: spec.remote(),
                    at: time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                };
                if let Err(error) = self.save_tunnel_history(&h) {
                    self.message = error;
                }
                self.tunnel.history = Some(h);
            } else {
                return;
            }
        }
        self.tunnel.current = event;
    }
    pub fn end_tunnel(&mut self, state: State) {
        self.tunnel.id = 0;
        self.tunnel.current = Event::new(
            state,
            if state == State::Cancelled {
                Some(Error::Cancelled)
            } else {
                None
            },
            None,
        );
    }
}

/// Live endpoint access checks both the owned child and the verified state.
/// Future API consumers use this handle, never a saved port or a TUI label.
pub struct Session {
    pub id: u64,
    pub spec: Spec,
    stop: Arc<AtomicBool>,
    commands: mpsc::Sender<()>,
    live: Arc<Mutex<Live>>,
    thread: Option<thread::JoinHandle<()>>,
}
struct Live {
    process: Option<Owned>,
    verified: bool,
    port: Option<u16>,
}
struct Owned {
    child: Child,
    stderr: ChildStderr,
    diagnostic: Vec<u8>,
    _directory: tempfile::TempDir,
}
impl Owned {
    fn alive(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|s| s.is_none())
    }
    fn error(&mut self) -> Error {
        let mut chunk = [0; 1024];
        for _ in 0..8 {
            match self.stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let count = n.min(4096usize.saturating_sub(self.diagnostic.len()));
                    self.diagnostic.extend_from_slice(&chunk[..count]);
                }
                Err(_) => break,
            }
        }
        classify(&String::from_utf8_lossy(&self.diagnostic))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // A retained, unreaped Child prevents PID reuse. Never signal a shared master.
        if self.child.try_wait().is_ok_and(|s| s.is_none()) {
            let pid = rustix::process::Pid::from_child(&self.child);
            if rustix::process::getpgid(Some(pid)).is_ok_and(|g| g == pid) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
impl Session {
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn endpoint(&self, spec: &Spec) -> Option<String> {
        if &self.spec != spec {
            return None;
        }
        let mut live = self.live.lock().ok()?;
        if !live.verified || !live.process.as_mut()?.alive() {
            return None;
        }
        Some(format!("http://127.0.0.1:{}/v1/", live.port?))
    }
    pub fn verify(&self) {
        if let Ok(mut live) = self.live.lock() {
            live.verified = false;
        }
        let _ = self.commands.send(());
    }
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        id: u64,
        spec: Spec,
        system: Arc<dyn System>,
        prefix: Vec<String>,
        sounds: Sounds,
        player: Player,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let live = Arc::new(Mutex::new(Live {
            process: None,
            verified: false,
            port: None,
        }));
        let (commands, receiver) = mpsc::channel();
        let stopped = Arc::clone(&stop);
        let current = Arc::clone(&live);
        let target = spec.clone();
        let thread = thread::spawn(move || {
            let outcome = crate::touch::touch_wait(&target.target, &sounds, player, |_| {
                connect(&target, system.as_ref(), &prefix, &stopped, &current, &notify)
            });
            if let Err(error) = outcome {
                notify(Event::new(
                    if error == Error::Cancelled {
                        State::Cancelled
                    } else {
                        State::Failed
                    },
                    Some(error),
                    None,
                ));
                return;
            }
            loop {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let alive = current.lock().ok().is_some_and(|mut l| {
                    l.process.as_mut().is_some_and(|p| {
                        let _ = p.error();
                        p.alive()
                    })
                });
                if !alive {
                    notify(Event::new(State::Lost, Some(Error::Lost), None));
                    break;
                }
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => {
                        let port = current.lock().ok().and_then(|mut l| {
                            l.verified = false;
                            l.port
                        });
                        notify(Event::new(State::Verifying, None, port));
                        let result = port.ok_or(Error::Startup).and_then(|p| health(p, &stopped));
                        if let Err(error) = result {
                            notify(Event::new(
                                if error == Error::Cancelled {
                                    State::Cancelled
                                } else {
                                    State::Failed
                                },
                                Some(error),
                                None,
                            ));
                            break;
                        }
                        if let Ok(mut l) = current.lock() {
                            if l.process.as_mut().is_none_or(|p| !p.alive()) {
                                notify(Event::new(State::Lost, Some(Error::Lost), None));
                                break;
                            }
                            l.verified = true;
                        }
                        notify(Event::new(State::Connected, None, port));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            if let Ok(mut l) = current.lock() {
                l.verified = false;
                l.process = None;
                l.port = None;
            }
        });
        Self {
            id,
            spec,
            stop,
            commands,
            live,
            thread: Some(thread),
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(mut l) = self.live.lock() {
            l.process = None;
            l.verified = false;
            l.port = None;
        }
    }
}
fn classify(text: &str) -> Error {
    let text = text.to_ascii_lowercase();
    if text.contains("permission denied") || text.contains("authentication failed") || text.contains("signing failed") {
        Error::Authentication
    } else if text.contains("host key verification failed") || text.contains("host identification has changed") {
        Error::HostKey
    } else if text.contains("address already in use") || text.contains("cannot listen to port") {
        Error::Port
    } else if text.contains("timed out") {
        Error::Timeout
    } else {
        Error::Startup
    }
}

#[allow(clippy::too_many_arguments)]
fn control(
    system: &dyn System,
    ssh: &str,
    socket: &std::path::Path,
    target: &str,
    operation: &str,
    forward: Option<&str>,
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<crate::system::Output, Error> {
    let mut args = vec![
        ssh.into(),
        "-F".into(),
        "/dev/null".into(),
        "-S".into(),
        socket.to_string_lossy().into_owned(),
        "-O".into(),
        operation.into(),
        "-o".into(),
        "ClearAllForwardings=no".into(),
    ];
    if let Some(forward) = forward {
        args.extend(["-L".into(), forward.into()]);
    }
    args.extend(["--".into(), target.into()]);
    system.run_until(&args, stop, timeout).map_err(|e| {
        if stop.load(Ordering::SeqCst) {
            Error::Cancelled
        } else if e.kind() == std::io::ErrorKind::TimedOut {
            Error::Timeout
        } else {
            Error::Startup
        }
    })
}
fn connect(
    spec: &Spec,
    system: &dyn System,
    prefix: &[String],
    stop: &AtomicBool,
    live: &Mutex<Live>,
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<(), Error> {
    validate_destination(&spec.target).map_err(|_| Error::Config)?;
    if spec.remote_port == 0 {
        return Err(Error::Config);
    }
    let started = Instant::now();
    let deadline = Duration::from_secs(45);
    let remaining = || {
        deadline
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(Error::Timeout)
    };
    let ssh = system.which("ssh").ok_or(Error::Startup)?;
    let options = [
        "-T",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "BatchMode=no",
        "-o",
        "ControlPersist=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "RemoteCommand=none",
        "-o",
        "ForkAfterAuthentication=no",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "ForwardAgent=no",
        "-o",
        "GatewayPorts=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "Tunnel=no",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
    ];
    let mut config = vec![ssh.clone()];
    config.extend(options.map(str::to_owned));
    config.extend(["-G".into(), "--".into(), spec.target.clone()]);
    let resolved = system.run_until(&config, stop, remaining()?).map_err(|e| {
        if stop.load(Ordering::SeqCst) {
            Error::Cancelled
        } else if e.kind() == std::io::ErrorKind::TimedOut {
            Error::Timeout
        } else {
            Error::Config
        }
    })?;
    if resolved.code != 0
        || !["hostname ", "user ", "port "]
            .iter()
            .all(|key| resolved.stdout.lines().any(|line| line.starts_with(key)))
    {
        return Err(Error::Config);
    }
    if !resolved.stdout.lines().any(|line| {
        matches!(
            line,
            "stricthostkeychecking ask"
                | "stricthostkeychecking true"
                | "stricthostkeychecking yes"
                | "stricthostkeychecking accept-new"
        )
    }) {
        return Err(Error::HostKey);
    }
    let dir = tempfile::Builder::new()
        .prefix("hs-pangolin-tunnel-")
        .tempdir()
        .map_err(|_| Error::Startup)?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Startup)?;
    let socket = dir.path().join("control");
    let mut args = prefix.to_vec();
    args.push(ssh.clone());
    args.extend(options.map(str::to_owned));
    args.extend([
        "-N".into(),
        "-M".into(),
        "-S".into(),
        socket.to_string_lossy().into_owned(),
        "-o".into(),
        "SessionType=none".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "--".into(),
        spec.target.clone(),
    ]);
    let (program, args) = args.split_first().ok_or(Error::Startup)?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| Error::Startup)?;
    let stderr = child.stderr.take().ok_or(Error::Startup)?;
    let mut owned = Owned {
        child,
        stderr,
        diagnostic: Vec::new(),
        _directory: dir,
    };
    rustix::fs::fcntl_setfl(
        &owned.stderr,
        rustix::fs::fcntl_getfl(&owned.stderr).map_err(|_| Error::Startup)? | rustix::fs::OFlags::NONBLOCK,
    )
    .map_err(|_| Error::Startup)?;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        if !owned.alive() {
            return Err(owned.error());
        }
        let _ = owned.error();
        if socket.exists()
            && control(
                system,
                &ssh,
                &socket,
                &spec.target,
                "check",
                None,
                stop,
                remaining()?.min(Duration::from_secs(2)),
            )?
            .code
                == 0
        {
            break;
        }
        remaining()?;
        thread::sleep(Duration::from_millis(50));
    }
    let mut port = None;
    for _ in 0..3 {
        if stop.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| Error::Port)?;
        let allocated = listener.local_addr().map_err(|_| Error::Port)?.port();
        if allocated < 1024 {
            return Err(Error::Port);
        }
        let forward = format!("127.0.0.1:{allocated}:127.0.0.1:{}", spec.remote_port);
        drop(listener);
        // Mux acknowledges a successfully owned listener, not merely a free-port probe.
        let response = control(
            system,
            &ssh,
            &socket,
            &spec.target,
            "forward",
            Some(&forward),
            stop,
            remaining()?.min(Duration::from_secs(3)),
        )?;
        if response.code == 0 {
            port = Some(allocated);
            break;
        }
        if !owned.alive() {
            return Err(owned.error());
        }
        if classify(&response.stderr) != Error::Port && owned.error() != Error::Port {
            return Err(Error::Startup);
        }
    }
    let port = port.ok_or(Error::Port)?;
    notify(Event::new(State::Verifying, None, Some(port)));
    health_with_limit(port, stop, remaining()?.min(Duration::from_secs(2)))?;
    if !owned.alive() {
        return Err(owned.error());
    }
    let mut state = live.lock().map_err(|_| Error::Startup)?;
    state.process = Some(owned);
    state.verified = true;
    state.port = Some(port);
    notify(Event::new(State::Connected, None, Some(port)));
    Ok(())
}

/// Direct socket HTTP: no proxy lookup, redirects, bearer headers or public fallback.
pub fn health(port: u16, stop: &AtomicBool) -> Result<(), Error> {
    health_with_limit(port, stop, Duration::from_secs(2))
}
fn health_error(error: std::io::Error) -> Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        Error::Timeout
    } else {
        Error::Remote
    }
}
fn health_with_limit(port: u16, stop: &AtomicBool, limit: Duration) -> Result<(), Error> {
    let started = Instant::now();
    if stop.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    let mut stream = TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(500),
    )
    .map_err(health_error)?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|_| Error::Remote)?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|_| Error::Remote)?;
    stream
        .write_all(b"GET /v1/ HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\nConnection: close\r\n\r\n")
        .map_err(health_error)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        if started.elapsed() >= limit {
            return Err(Error::Timeout);
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.len() > 9216 {
                    return Err(Error::Response);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => continue,
            Err(_) => return Err(Error::Remote),
        }
    }
    if bytes.is_empty() {
        return Err(Error::Remote);
    }
    let boundary = bytes.windows(4).position(|w| w == b"\r\n\r\n").ok_or(Error::Response)?;
    if boundary > 8192 {
        return Err(Error::Response);
    }
    let headers = std::str::from_utf8(&bytes[..boundary]).map_err(|_| Error::Response)?;
    let mut lines = headers.split("\r\n");
    let status = lines
        .next()
        .ok_or(Error::Response)?
        .split_whitespace()
        .collect::<Vec<_>>();
    if status.len() < 2 || !matches!(status[0], "HTTP/1.1" | "HTTP/1.0") || status[1] != "200" {
        return Err(Error::Response);
    }
    let mut body = bytes[boundary + 4..].to_vec();
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let (key, value) = line.split_once(':').ok_or(Error::Response)?;
        if key.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(Error::Response);
            }
            length = Some(value.trim().parse::<usize>().map_err(|_| Error::Response)?);
        } else if key.eq_ignore_ascii_case("transfer-encoding") {
            if value.trim() != "chunked" || chunked {
                return Err(Error::Response);
            }
            chunked = true;
        }
    }
    if chunked {
        if length.is_some() {
            return Err(Error::Response);
        }
        let mut decoded = Vec::new();
        let mut input = body.as_slice();
        loop {
            let at = input.windows(2).position(|w| w == b"\r\n").ok_or(Error::Response)?;
            let count = usize::from_str_radix(
                std::str::from_utf8(&input[..at])
                    .map_err(|_| Error::Response)?
                    .split(';')
                    .next()
                    .ok_or(Error::Response)?,
                16,
            )
            .map_err(|_| Error::Response)?;
            input = &input[at + 2..];
            if count == 0 {
                break;
            }
            if count > 1024usize.saturating_sub(decoded.len())
                || input.len() < count + 2
                || &input[count..count + 2] != b"\r\n"
            {
                return Err(Error::Response);
            }
            decoded.extend_from_slice(&input[..count]);
            input = &input[count + 2..];
        }
        body = decoded;
    }
    if body.len() > 1024 || length.is_some_and(|l| l != body.len()) {
        return Err(Error::Response);
    }
    if serde_json::from_slice::<serde_json::Value>(&body).ok() != Some(serde_json::json!({"message":"Healthy"})) {
        return Err(Error::Response);
    }
    Ok(())
}
