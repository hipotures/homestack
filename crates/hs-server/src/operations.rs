//! The operation model (mapping 8.4): plans, jobs and what runs them.
//!
//! An [`Operation`] plans from the cluster as it is now and executes a plan
//! step by step. The server keeps plans for a while ([`AppOptions::plan_ttl`]),
//! plans again before applying one and runs the job on its own task, so a
//! job does not depend on the request or stream that follows it. One job at
//! a time works on a VM. Plans and jobs live in memory: a restart forgets
//! them (several instances, phase 11).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use hs_core::CoreError;
use hs_core::lease;
use hs_core::tasks::Observer;
use hs_model::jobs::{Job, JobError, JobEvent, JobProgress, JobState, JobStep, JobTask, Plan, StepState, TaskStatus};
use hs_pve::PveClient;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::config_store::Stored;

/// A boxed future, as operations return them.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What an operation may use.
pub struct OpContext {
    pub pve: Arc<PveClient>,
    /// The configuration when the plan was made or applied.
    pub config: Option<Arc<Stored>>,
    /// The instance's pools.
    pub pools: Arc<crate::Pools>,
    /// The secrets of the apply request (decision 6); empty while planning.
    pub secrets: hs_model::jobs::ApplySecrets,
}

/// What an operation would do.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// VMIDs the operation changes.
    pub targets: Vec<u32>,
    /// The plan as the client sees it.
    pub summary: Value,
}

/// Why planning or executing failed.
#[derive(Debug)]
pub enum OpFailure {
    /// The parameters are invalid (`422`); the message is fixed text.
    Invalid(&'static str),
    /// A VM the parameters name does not exist (`404`).
    NotFound(&'static str),
    /// A step failed.
    Core(CoreError),
}

impl OpFailure {
    #[must_use]
    pub const fn invalid(message: &'static str) -> Self {
        Self::Invalid(message)
    }

    #[must_use]
    pub const fn not_found(message: &'static str) -> Self {
        Self::NotFound(message)
    }

    /// The job's error: Proxmox failures keep the API's error code.
    fn job_error(&self) -> JobError {
        let (code, message) = match self {
            Self::Invalid(message) | Self::NotFound(message) => ("operation_failed".to_owned(), (*message).to_owned()),
            Self::Core(CoreError::Pve(error)) => {
                let code = serde_json::to_value(crate::error::pve_error_code(error))
                    .ok()
                    .and_then(|code| code.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "pve_error".to_owned());
                (code, error.to_string())
            }
            Self::Core(error @ CoreError::OutsidePool(_)) => ("outside_pool".to_owned(), error.to_string()),
            Self::Core(error @ CoreError::Busy(_)) => ("busy".to_owned(), error.to_string()),
            Self::Core(error) => ("operation_failed".to_owned(), error.to_string()),
        };
        JobError { code, message }
    }
}

impl From<CoreError> for OpFailure {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

/// A HomeStack operation (`workspace.create`, ...).
pub trait Operation: Send + Sync {
    /// The name clients use in `POST /v1/plans`.
    fn name(&self) -> &'static str;

    /// Plan from the cluster as it is now. Must not change anything, and
    /// must give the same plan for the same parameters and cluster state:
    /// the plan is made again before it runs and compared by digest.
    fn plan<'a>(&'a self, ctx: &'a OpContext, parameters: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>>;

    /// Carry out a plan; the result becomes the job's `result`.
    fn execute<'a>(
        &'a self,
        ctx: &'a OpContext,
        planned: &'a Planned,
        job: &'a JobHandle,
    ) -> BoxFuture<'a, Result<Value, OpFailure>>;
}

/// Settings of the operation model.
#[derive(Clone)]
pub struct AppOptions {
    /// Operations clients may plan: `workspace.create`,
    /// `workspace.destroy` (phase 5), `workspace.refresh` (phase 6),
    /// `workspace.resize` (phase 7), `workspace.migrate` (phase 8) and
    /// `workspace.addkey` (phase 9), unless replaced.
    pub operations: Vec<Arc<dyn Operation>>,
    /// Longest silence on a job stream.
    pub heartbeat: Duration,
    /// How long a plan can be applied.
    pub plan_ttl: Duration,
    /// Finished jobs kept for `GET /v1/jobs`.
    pub retained_jobs: usize,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            operations: crate::lifecycle::operations(hs_core::lifecycle::LifecycleTiming::default()),
            heartbeat: Duration::from_secs(5),
            plan_ttl: Duration::from_secs(15 * 60),
            retained_jobs: 100,
        }
    }
}

/// RFC 3339 UTC with milliseconds, e.g. `2026-10-12T12:00:00.000Z`.
#[must_use]
pub fn timestamp(at: SystemTime) -> String {
    let at = time::OffsetDateTime::from(at);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

/// SHA-256 of the canonical JSON (sorted keys) of what was planned.
fn digest(operation: &str, parameters: &Value, planned: &Planned) -> String {
    let mut summary = planned.summary.clone();
    // Free space changes with unrelated writes. Admission is checked again
    // when replanning and immediately before allocation; required bytes stay signed.
    let capacity_paths: &[&str] = match operation {
        "workspace.migrate" => &["/capacity"],
        "workspace.create" => &["/capacity/target", "/capacity/source_clone"],
        _ => &[],
    };
    for path in capacity_paths {
        if let Some(capacity) = summary.pointer_mut(path).and_then(Value::as_object_mut) {
            capacity.remove("available_bytes");
        }
    }
    let canonical = json!({
        "operation": operation,
        "parameters": parameters,
        "targets": planned.targets,
        "summary": summary,
    });
    Sha256::digest(canonical.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct StoredPlan {
    plan: Plan,
    parameters: Value,
    operation: Arc<dyn Operation>,
    expires: Instant,
    applied: bool,
}

/// Why a plan cannot be applied.
pub enum ApplyRefusal {
    Unknown,
    Changed,
    Applied,
    /// Another job holds a VM of the plan: one of this instance (`None`), or
    /// one of another instance (its lease, described).
    Busy(Option<String>),
    /// The secrets sent do not match the plan's `secrets_required`.
    Secrets(&'static str),
    Failure(OpFailure),
}

struct Inner {
    plans: HashMap<String, StoredPlan>,
    jobs: VecDeque<Arc<watch::Sender<Job>>>,
    busy: BTreeSet<u32>,
}

/// Plans, jobs and the VMs jobs are working on.
pub struct OpsState {
    pub options: AppOptions,
    pub instance_id: String,
    /// `HOMESTACK_INSTANCE`, the holder of this instance's leases.
    pub instance: String,
    inner: Mutex<Inner>,
}

impl OpsState {
    #[must_use]
    pub fn new(options: AppOptions, instance: String) -> Self {
        Self {
            options,
            instance_id: uuid::Uuid::new_v4().to_string(),
            instance,
            inner: Mutex::new(Inner {
                plans: HashMap::new(),
                jobs: VecDeque::new(),
                busy: BTreeSet::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        inner.plans.retain(|_, stored| stored.expires > now);
        inner
    }

    /// Whether this instance runs a job with this id now.
    fn running(&self, id: &str) -> bool {
        self.lock().jobs.iter().any(|job| {
            let job = job.borrow();
            job.id == id && !job.state.finished()
        })
    }

    fn holder<'a>(
        &'a self,
        job: &'a str,
        operation: &'a str,
        running: &'a (dyn Fn(&str) -> bool + Sync),
    ) -> lease::Holder<'a> {
        lease::Holder {
            instance: &self.instance,
            job,
            operation,
            now: SystemTime::now(),
            ttl: lease::LEASE_TTL,
            running,
        }
    }

    /// Refuse when another instance holds a target (`busy`).
    async fn check_leases(&self, ctx: &OpContext, operation: &str, targets: &[u32]) -> Result<(), OpFailure> {
        let running = |id: &str| self.running(id);
        for vmid in targets {
            lease::check_free(ctx.pve.as_ref(), *vmid, &self.holder("", operation, &running)).await?;
        }
        Ok(())
    }

    /// Write the job's lease on every target, or on none.
    async fn acquire_leases(
        &self,
        ctx: &OpContext,
        job: &str,
        operation: &str,
        targets: &[u32],
    ) -> Result<(), CoreError> {
        let running = |id: &str| self.running(id);
        for (index, vmid) in targets.iter().enumerate() {
            if let Err(error) = lease::acquire(ctx.pve.as_ref(), *vmid, &self.holder(job, operation, &running)).await {
                self.release_leases(ctx, job, &targets[..index]).await;
                return Err(error);
            }
        }
        Ok(())
    }

    async fn release_leases(&self, ctx: &OpContext, job: &str, targets: &[u32]) {
        for vmid in targets {
            if let Err(error) = lease::release(ctx.pve.as_ref(), *vmid, &self.instance, job).await {
                tracing::warn!(%error, vmid, job, "lease not removed; it expires by itself");
            }
        }
    }

    fn operation(&self, name: &str) -> Option<Arc<dyn Operation>> {
        self.options
            .operations
            .iter()
            .find(|operation| operation.name() == name)
            .cloned()
    }

    /// Plan `operation` and keep the plan. `None` for an unknown operation.
    pub async fn plan(&self, ctx: &OpContext, name: &str, parameters: Value) -> Option<Result<Plan, OpFailure>> {
        let operation = self.operation(name)?;
        let planned = match operation.plan(ctx, &parameters).await {
            Ok(planned) => planned,
            Err(failure) => return Some(Err(failure)),
        };
        if let Err(failure) = self.check_leases(ctx, name, &planned.targets).await {
            return Some(Err(failure));
        }
        let now = SystemTime::now();
        let plan = Plan {
            id: uuid::Uuid::new_v4().to_string(),
            operation: name.to_owned(),
            digest: digest(name, &parameters, &planned),
            created_at: timestamp(now),
            expires_at: timestamp(now + self.options.plan_ttl),
            targets: planned.targets,
            summary: planned.summary,
        };
        self.lock().plans.insert(
            plan.id.clone(),
            StoredPlan {
                plan: plan.clone(),
                parameters,
                operation,
                expires: Instant::now() + self.options.plan_ttl,
                applied: false,
            },
        );
        Some(Ok(plan))
    }

    #[must_use]
    pub fn get_plan(&self, id: &str) -> Option<Plan> {
        self.lock().plans.get(id).map(|stored| stored.plan.clone())
    }

    /// Plan again, compare, and start the job.
    pub async fn apply(self: &Arc<Self>, ctx: OpContext, id: &str, client_digest: &str) -> Result<Job, ApplyRefusal> {
        let (plan, parameters, operation) = {
            let mut inner = self.lock();
            let stored = inner.plans.get(id).ok_or(ApplyRefusal::Unknown)?;
            if stored.plan.digest != client_digest {
                return Err(ApplyRefusal::Changed);
            }
            if stored.applied {
                return Err(ApplyRefusal::Applied);
            }
            let required: BTreeSet<&str> = stored
                .plan
                .summary
                .get("secrets_required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let given: BTreeSet<&str> = ctx.secrets.names().into_iter().collect();
            if !required.is_subset(&given) {
                return Err(ApplyRefusal::Secrets(
                    "The plan needs a secret the request did not send; see the plan's secrets_required.",
                ));
            }
            if !given.is_subset(&required) {
                return Err(ApplyRefusal::Secrets(
                    "The request sends a secret the plan does not need.",
                ));
            }
            let found = (
                stored.plan.clone(),
                stored.parameters.clone(),
                Arc::clone(&stored.operation),
            );
            if found.0.targets.iter().any(|vmid| inner.busy.contains(vmid)) {
                return Err(ApplyRefusal::Busy(None));
            }
            inner.busy.extend(found.0.targets.iter().copied());
            found
        };
        let planned = match operation.plan(&ctx, &parameters).await {
            Ok(planned) => planned,
            Err(failure) => {
                self.release(&plan.targets);
                return Err(ApplyRefusal::Failure(failure));
            }
        };
        if digest(&plan.operation, &parameters, &planned) != plan.digest {
            self.release(&plan.targets);
            return Err(ApplyRefusal::Changed);
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = self
            .acquire_leases(&ctx, &job_id, &plan.operation, &planned.targets)
            .await
        {
            self.release(&plan.targets);
            return Err(match error {
                CoreError::Busy(message) => ApplyRefusal::Busy(Some(message)),
                error => ApplyRefusal::Failure(OpFailure::Core(error)),
            });
        }
        let job = Job {
            id: job_id.clone(),
            operation: plan.operation.clone(),
            plan_id: plan.id.clone(),
            state: JobState::Queued,
            created_at: timestamp(SystemTime::now()),
            started_at: None,
            finished_at: None,
            steps: Vec::new(),
            tasks: Vec::new(),
            progress: None,
            log: Vec::new(),
            result: None,
            error: None,
        };
        let sender = Arc::new(watch::Sender::new(job.clone()));
        {
            let mut inner = self.lock();
            if let Some(stored) = inner.plans.get_mut(id) {
                stored.applied = true;
            }
            inner.jobs.push_front(Arc::clone(&sender));
            let retained = self.options.retained_jobs.max(1);
            while inner.jobs.len() > retained {
                let finished = inner.jobs.iter().rposition(|job| job.borrow().state.finished());
                match finished {
                    Some(index) => {
                        inner.jobs.remove(index);
                    }
                    None => break,
                }
            }
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let handle = JobHandle { sender };
            handle.update(|job| {
                job.state = JobState::Running;
                job.started_at = Some(timestamp(SystemTime::now()));
            });
            let outcome = operation.execute(&ctx, &planned, &handle).await;
            state.release_leases(&ctx, &job_id, &planned.targets).await;
            handle.update(|job| {
                job.finished_at = Some(timestamp(SystemTime::now()));
                job.progress = None;
                match outcome {
                    Ok(result) => {
                        job.state = JobState::Succeeded;
                        job.result = Some(result);
                    }
                    Err(failure) => {
                        tracing::warn!(job = %job.id, operation = %job.operation, error = ?failure, "job failed");
                        job.state = JobState::Failed;
                        job.error = Some(failure.job_error());
                    }
                }
            });
            state.release(&planned.targets);
        });
        Ok(job)
    }

    fn release(&self, targets: &[u32]) {
        let mut inner = self.lock();
        for vmid in targets {
            inner.busy.remove(vmid);
        }
    }

    /// The job's channel, for reading or following it.
    #[must_use]
    pub fn job(&self, id: &str) -> Option<watch::Receiver<Job>> {
        self.lock()
            .jobs
            .iter()
            .find(|job| job.borrow().id == id)
            .map(|job| job.subscribe())
    }

    /// All kept jobs, newest first.
    #[must_use]
    pub fn jobs(&self) -> Vec<Job> {
        self.lock().jobs.iter().map(|job| job.borrow().clone()).collect()
    }
}

/// Log lines kept per job.
const LOG_LINES: usize = 200;

/// What a running operation reports through. Also an [`Observer`], so the
/// `hs-core` functions report their Proxmox tasks into the job.
pub struct JobHandle {
    sender: Arc<watch::Sender<Job>>,
}

impl JobHandle {
    fn update(&self, change: impl FnOnce(&mut Job)) {
        self.sender.send_modify(change);
    }

    /// Run one named step (Python's `start_step` / `finish_step`).
    pub async fn step<T>(&self, name: &str, work: impl Future<Output = Result<T, CoreError>>) -> Result<T, OpFailure> {
        self.event(JobEvent::StepStarted { name: name.to_owned() });
        let outcome = work.await;
        self.event(JobEvent::StepFinished {
            name: name.to_owned(),
            ok: outcome.is_ok(),
        });
        outcome.map_err(OpFailure::Core)
    }
}

impl Observer for JobHandle {
    fn event(&self, event: JobEvent) {
        self.update(|job| apply_event(job, event));
    }
}

fn apply_event(job: &mut Job, event: JobEvent) {
    match event {
        JobEvent::StepStarted { name } => job.steps.push(JobStep {
            name,
            state: StepState::Running,
        }),
        JobEvent::StepFinished { name, ok } => {
            if let Some(step) = job.steps.iter_mut().rev().find(|step| step.name == name) {
                step.state = if ok { StepState::Done } else { StepState::Failed };
            }
            job.progress = None;
        }
        JobEvent::TaskStarted { node, upid, kind, vmid } => job.tasks.push(JobTask {
            node,
            upid,
            kind,
            vmid,
            status: TaskStatus::Running,
            exitstatus: None,
        }),
        JobEvent::TaskLog { lines, .. } => {
            job.log.extend(lines);
            let excess = job.log.len().saturating_sub(LOG_LINES);
            job.log.drain(..excess);
        }
        JobEvent::TaskFinished { upid, exitstatus } => {
            if let Some(task) = job.tasks.iter_mut().find(|task| task.upid == upid) {
                task.status = TaskStatus::Stopped;
                task.exitstatus = Some(exitstatus);
            }
        }
        JobEvent::Progress { percent, detail } => job.progress = Some(JobProgress { percent, detail }),
    }
}
