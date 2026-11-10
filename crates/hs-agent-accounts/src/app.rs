use crate::{
    db::Database,
    model::*,
    process,
    providers::{self, Provider},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

pub struct Job {
    pub value: Operation,
    pub cancel: CancellationToken,
    pub codes: tokio::sync::mpsc::Sender<String>,
}
pub struct Context {
    pub root: PathBuf,
    _instance_lock: std::fs::File,
    pub db: Database,
    pub providers: Vec<Arc<dyn Provider>>,
    pub jobs: Mutex<HashMap<String, Job>>,
    pub locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    pub gates: HashMap<String, Arc<RwLock<()>>>,
    pub slots: Semaphore,
    pub interval: u64,
    pub heartbeat_seconds: u64,
    pub heartbeat: Mutex<(Instant, u64)>,
    pub shutdown: CancellationToken,
    pub telegram: Option<crate::telegram::Telegram>,
    pub telegram_misconfigured: bool,
}
impl Context {
    pub fn open(root: PathBuf, interval: u64, concurrency: usize, heartbeat_seconds: u64) -> Result<Arc<Self>> {
        process::directory(&root)?;
        let (telegram, telegram_misconfigured) = match crate::telegram::Telegram::from_env() {
            Ok(telegram) => (telegram, false),
            Err(error) => {
                eprintln!("Telegram notifications disabled: {}", error.message);
                (None, true)
            }
        };
        let canonical = root
            .canonicalize()
            .map_err(|_| Failure::new("storage_failure", "Cannot resolve persistent storage"))?;
        if canonical != root {
            return Err(Failure::new(
                "storage_failure",
                "Persistent storage must not contain symlinks",
            ));
        }
        if std::env::var("AGENT_ACCOUNTS_EPHEMERAL").as_deref() != Ok("1") {
            let mounted = std::fs::read_to_string("/proc/self/mountinfo")
                .unwrap_or_default()
                .lines()
                .any(|l| l.split_whitespace().nth(4) == root.to_str());
            if !mounted {
                return Err(Failure::new(
                    "storage_failure",
                    "Persistent storage must be mounted; AGENT_ACCOUNTS_EPHEMERAL=1 permits disposable local checks",
                ));
            }
        }
        for name in ["profiles", "database", "state", "runtimes"] {
            process::directory(&root.join(name))?;
        }
        let instance_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("state/service.lock"))
            .map_err(|_| Failure::new("storage_failure", "Cannot open service instance lock"))?;
        rustix::fs::flock(&instance_lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| Failure::new("busy", "Another Agent Accounts process owns this persistent home"))?;
        for entry in std::fs::read_dir(root.join("state"))
            .map_err(|_| Failure::new("storage_failure", "Cannot inspect runtime staging"))?
        {
            let entry = entry.map_err(|_| Failure::new("storage_failure", "Cannot inspect runtime staging"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name
                .strip_prefix("runtime-")
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
                && entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink())
            {
                std::fs::remove_dir_all(entry.path())
                    .map_err(|_| Failure::new("storage_failure", "Cannot remove interrupted runtime staging"))?;
            }
        }
        let db = Database::open(&root.join("database/accounts.db"))?;
        let ctx = Arc::new(Self {
            root,
            _instance_lock: instance_lock,
            db,
            providers: providers::registered(),
            jobs: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            gates: providers::registered()
                .iter()
                .map(|p| (p.id().to_owned(), Arc::new(RwLock::new(()))))
                .collect(),
            slots: Semaphore::new(concurrency),
            interval,
            heartbeat_seconds,
            heartbeat: Mutex::new((Instant::now(), 0)),
            shutdown: CancellationToken::new(),
            telegram,
            telegram_misconfigured,
        });
        for provider in &ctx.providers {
            ctx.seed(provider.id())?;
            if provider.bundled_version().is_some() {
                ctx.binary(provider.id())?;
            }
        }
        ctx.db.interrupt_operations()?;
        for p in ctx.db.profiles()? {
            if p.deleting {
                ctx.delete_files(&p.id)?;
                ctx.db.delete(&p.id)?;
            }
        }
        Ok(ctx)
    }
    pub fn provider(&self, id: &str) -> Result<Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .cloned()
            .ok_or_else(Failure::unsupported)
    }
    pub fn seed(&self, provider: &str) -> Result<()> {
        if self.db.runtime(provider)?.is_some() {
            return Ok(());
        }
        let Some(version) = self.provider(provider)?.bundled_version() else {
            return Ok(());
        };
        let source = PathBuf::from(
            std::env::var("AGENT_ACCOUNTS_BUNDLED_RUNTIMES").unwrap_or_else(|_| "/opt/agent-accounts".into()),
        )
        .join(provider);
        let target = self.root.join("runtimes").join(provider).join(version);
        process::directory(&target)?;
        std::fs::copy(source, target.join(provider))
            .map_err(|_| Failure::new("runtime_failure", "Bundled runtime is missing or cannot be installed"))?;
        self.db.save_runtime(&Runtime {
            provider: provider.into(),
            version: version.into(),
            previous_version: None,
            latest_version: None,
            checked_at: None,
            last_result: None,
            last_error: None,
            verified_at: None,
        })
    }
    pub fn binary(&self, provider: &str) -> Result<PathBuf> {
        let runtime = self
            .db
            .runtime(provider)?
            .ok_or_else(|| Failure::new("runtime_failure", "No runtime pin is configured"))?;
        let path = self
            .root
            .join("runtimes")
            .join(provider)
            .join(runtime.version)
            .join(provider);
        if !path.is_file() || path.is_symlink() {
            return Err(Failure::new("runtime_failure", "Pinned runtime is missing"));
        }
        Ok(path)
    }
    pub fn lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.into())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut ps = self.db.profiles()?;
        for p in &mut ps {
            p.stale = self.stale(p);
        }
        Ok(ps)
    }
    pub fn stale(&self, p: &Profile) -> bool {
        p.error.is_some()
            || p.last_success.is_none_or(|t| now() - t > (self.interval * 2) as i64)
            || p.snapshot
                .as_ref()
                .is_some_and(|s| s.limits.iter().any(|l| l.resets_at.is_some_and(|t| t <= now())))
    }
    pub fn create(&self, provider: &str, name: &str) -> Result<Profile> {
        self.provider(provider)?;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(Failure::new(
                "invalid_input",
                "Display name must contain 1–80 printable characters",
            ));
        }
        let p = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            provider: provider.into(),
            display_name: name.into(),
            enabled: true,
            deleting: false,
            auth_status: "unauthenticated".into(),
            account: None,
            last_success: None,
            last_attempt: None,
            error: None,
            failures: 0,
            total_failures: 0,
            probe_duration_seconds: None,
            window_started_at: None,
            last_payment: None,
            snapshot: None,
            notifications: Vec::new(),
            created_at: now(),
            updated_at: now(),
            stale: true,
        };
        process::prepare(&self.root, &p.id)?;
        self.db.save(&p, None)?;
        Ok(p)
    }
    pub fn update_operation(&self, id: &str, f: impl FnOnce(&mut Operation)) {
        if let Some(j) = self.jobs.lock().unwrap_or_else(PoisonError::into_inner).get_mut(id) {
            f(&mut j.value);
        }
    }
    pub fn operation(&self, id: &str) -> Result<Operation> {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|j| j.value.clone())
            .map(Ok)
            .unwrap_or_else(|| self.db.read_operation(id))
    }
    pub fn submit_code(&self, profile: &str, code: String) -> Result<()> {
        if code.is_empty() || code.len() > 4096 || code.chars().any(char::is_control) {
            return Err(Failure::new(
                "invalid_input",
                "Authorization code must be one printable line",
            ));
        }
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        let job = jobs
            .values()
            .find(|j| j.value.profile_id.as_deref() == Some(profile) && j.value.accepts_code && !j.value.terminal())
            .ok_or_else(|| Failure::new("busy", "This profile is not waiting for an authorization code"))?;
        job.codes
            .try_send(code)
            .map_err(|_| Failure::new("busy", "An authorization code is already being processed"))
    }
    pub fn cancel_profile(&self, id: &str) {
        for j in self.jobs.lock().unwrap_or_else(PoisonError::into_inner).values() {
            if j.value.profile_id.as_deref() == Some(id) && !j.value.terminal() {
                j.cancel.cancel();
            }
        }
    }
    pub fn begin(self: &Arc<Self>, p: &Profile, kind: &str) -> Result<Operation> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(j) = jobs
            .values()
            .find(|j| j.value.profile_id.as_deref() == Some(&p.id) && !j.value.terminal())
        {
            if j.value.kind == kind {
                return Ok(j.value.clone());
            }
            return Err(Failure::new(
                "busy",
                "This profile already has an operation in progress",
            ));
        }
        if jobs.values().filter(|j| !j.value.terminal()).count() >= 32 {
            return Err(Failure::new("busy", "Operation queue is full"));
        }
        if jobs.len() >= 128 {
            jobs.retain(|_, j| !j.value.terminal() || now() - j.value.finished_at.unwrap_or(now()) < 600);
        }
        if jobs.len() >= 256 {
            return Err(Failure::new("busy", "Operation history is full; retry shortly"));
        }
        let op = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            profile_id: Some(p.id.clone()),
            provider: p.provider.clone(),
            kind: kind.into(),
            status: "queued".into(),
            message: "Waiting for a process slot".into(),
            authorization_url: None,
            user_code: None,
            accepts_code: false,
            error: None,
            started_at: now(),
            finished_at: None,
            result: None,
        };
        let cancel = self.shutdown.child_token();
        let (codes, receiver) = tokio::sync::mpsc::channel(1);
        jobs.insert(
            op.id.clone(),
            Job {
                value: op.clone(),
                cancel: cancel.clone(),
                codes,
            },
        );
        drop(jobs);
        let ctx = self.clone();
        let id = op.id.clone();
        let profile = p.id.clone();
        let kind = kind.to_owned();
        let provider_id = p.provider.clone();
        tokio::spawn(async move {
            // Keep the code receiver owned by the selected operation, never by a global terminal.
            let limit = match kind.as_str() {
                "authentication" => 600,
                "start_window" => 150,
                _ => 90,
            };
            let result = process::bounded(&cancel, limit, async {
                let gate = ctx.gates.get(&provider_id).cloned().ok_or_else(Failure::unsupported)?;
                let _provider_guard = gate.read().await;
                let lock = ctx.lock(&profile);
                let _guard = lock.lock().await;
                let _slot = ctx
                    .slots
                    .acquire()
                    .await
                    .map_err(|_| Failure::new("cancelled", "Service is stopping"))?;
                let mut p = ctx.db.profile(&profile)?;
                if p.deleting {
                    return Err(Failure::new("not_found", "Profile is being deleted"));
                }
                if kind != "authentication" && !p.enabled {
                    return Err(Failure::new("disabled", "Profile is disabled"));
                }
                ctx.update_operation(&id, |o| {
                    o.status = "running".into();
                    o.message = "Contacting provider".into();
                });
                let provider = ctx.provider(&p.provider)?;
                if kind == "authentication" {
                    let account = provider
                        .authenticate(ctx.clone(), p.clone(), id.clone(), receiver)
                        .await?;
                    p.account = Some(account);
                    p.auth_status = "authenticated".into();
                    p.error = Some(Failure::new(
                        "stale_measurement",
                        "Authentication completed; awaiting a fresh usage check",
                    ));
                    p.updated_at = now();
                    ctx.db.save(&p, None)?;
                    Ok(None)
                } else if kind == "statistics" {
                    let value = provider.statistics(&ctx, &p).await?;
                    ctx.update_operation(&id, |o| o.result = Some(value));
                    Ok(None)
                } else {
                    p.last_attempt = Some(now());
                    ctx.db.save(&p, None)?;
                    let binary = ctx
                        .db
                        .runtime(&p.provider)?
                        .map(|_| ctx.binary(&p.provider))
                        .transpose()?;
                    if kind == "start_window" {
                        let runtime = binary
                            .as_deref()
                            .ok_or_else(|| Failure::new("runtime_failure", "Managed runtime is unavailable"))?;
                        ctx.update_operation(&id, |o| o.message = "Sending a minimal request".into());
                        let sent = process::bounded(&cancel, 60, provider.start_window(&ctx, &p, runtime)).await?;
                        p.window_started_at = Some(now());
                        ctx.update_operation(&id, |o| {
                            o.message = "Checking usage".into();
                            o.result = Some(sent);
                        });
                    }
                    let started = Instant::now();
                    let result = process::bounded(&cancel, 60, async {
                        let first = provider.probe(&ctx, &p, binary.as_deref()).await;
                        if first.as_ref().is_err_and(|e| e.code == "network_failure") {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            provider.probe(&ctx, &p, binary.as_deref()).await
                        } else {
                            first
                        }
                    })
                    .await;
                    p.probe_duration_seconds = Some(started.elapsed().as_secs_f64());
                    match result {
                        Ok(snapshot) => {
                            crate::telegram::observe(&mut p, &snapshot, ctx.telegram.is_some());
                            p.auth_status = "authenticated".into();
                            p.account = Some(snapshot.account.clone());
                            p.last_success = Some(snapshot.observed_at);
                            p.error = None;
                            p.failures = 0;
                            p.snapshot = Some(snapshot.clone());
                            p.updated_at = now();
                            ctx.db.save(&p, Some(&snapshot))?;
                            if let Some(mut runtime) = ctx.db.runtime(&p.provider)? {
                                runtime.verified_at = Some(now());
                                ctx.db.save_runtime(&runtime)?;
                            }
                            Ok(Some(snapshot))
                        }
                        Err(e) => {
                            p.failures = p.failures.saturating_add(1);
                            p.total_failures = p.total_failures.saturating_add(1);
                            if e.code == "reauthentication_required" || e.code == "authentication_expired" {
                                p.auth_status = e.code.clone();
                            }
                            p.error = Some(e.clone());
                            p.updated_at = now();
                            ctx.db.save(&p, None)?;
                            Err(e)
                        }
                    }
                }
            })
            .await;
            let success = result.is_ok();
            if kind == "authentication"
                && let Err(error) = &result
                && error.code != "cancelled"
            {
                let lock = ctx.lock(&profile);
                let _guard = lock.lock().await;
                if let Ok(mut p) = ctx.db.profile(&profile)
                    && !p.deleting
                {
                    p.error = Some(error.clone());
                    p.updated_at = now();
                    let _ = ctx.db.save(&p, None);
                }
            }
            ctx.update_operation(&id, |o| {
                o.finished_at = Some(now());
                o.authorization_url = None;
                o.user_code = None;
                o.accepts_code = false;
                match &result {
                    Ok(_) => {
                        o.status = "succeeded".into();
                        if o.kind != "statistics" {
                            o.message = "Completed".into();
                        }
                    }
                    Err(e) => {
                        o.status = if e.code == "cancelled" { "cancelled" } else { "failed" }.into();
                        o.message = e.message.clone();
                        o.error = Some(e.clone());
                    }
                }
            });
            if let Ok(op) = ctx.operation(&id) {
                let _ = ctx.db.operation(&op);
            }
            if success
                && kind == "authentication"
                && let Ok(p) = ctx.db.profile(&profile)
                && p.enabled
            {
                let _ = ctx.begin(&p, "probe");
            }
        });
        Ok(op)
    }
    pub fn delete_files(&self, id: &str) -> Result<()> {
        let path = process::profile_dir(&self.root, id)?;
        if path.exists() {
            std::fs::remove_dir_all(path).map_err(|_| {
                Failure::new(
                    "storage_failure",
                    "Cannot remove profile files; deletion will resume on restart",
                )
            })?;
        }
        Ok(())
    }
    pub async fn scheduler(self: Arc<Self>) {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {_=self.shutdown.cancelled()=>break,_=timer.tick()=>{}};
            match self.db.profiles() {
                Ok(ps) => {
                    for p in ps {
                        if p.enabled
                            && !p.deleting
                            && p.auth_status == "authenticated"
                            && self
                                .provider(&p.provider)
                                .is_ok_and(|adapter| adapter.capabilities().contains(&"subscription_limits"))
                            && p.last_attempt.is_none_or(|t| now() - t >= self.interval as i64)
                        {
                            let _ = self.begin(&p, "probe");
                        }
                    }
                }
                Err(e) => eprintln!("Scheduler cycle failed: {}", e.code),
            }
        }
    }
    pub async fn heartbeat(self: Arc<Self>, mut addr: std::net::SocketAddr) {
        if addr.ip().is_unspecified() {
            addr.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        }
        let Ok(client) = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
        else {
            return;
        };
        let mut timer = tokio::time::interval(Duration::from_secs(self.heartbeat_seconds));
        loop {
            tokio::select! {_=self.shutdown.cancelled()=>break,_=timer.tick()=>{}};
            if client
                .get(format!("http://{addr}/health/heartbeat"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                let mut h = self.heartbeat.lock().unwrap_or_else(PoisonError::into_inner);
                h.0 = Instant::now();
                h.1 = h.1.saturating_add(1);
            }
        }
    }
    pub fn liveness(&self) -> (bool, u64, u64) {
        let h = self.heartbeat.lock().unwrap_or_else(PoisonError::into_inner);
        let age = h.0.elapsed().as_secs();
        (age <= self.heartbeat_seconds * 3, age, h.1)
    }
}
