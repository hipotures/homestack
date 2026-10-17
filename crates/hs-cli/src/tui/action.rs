//! Actions on workspaces in the TUI (phase 13b, step 2): the box inside
//! the workspaces panel walks form › plan › run. The work is the CLI's: the
//! same plan, the same question, the same job, the same desktop part
//! (`app::after_*`), the same words at the end.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hs_client::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::app::{self, client_error};
use crate::config::Loaded;
use crate::view::{self, Style};
use crate::{expand_home, identity};

/// What the box does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    New,
    Refresh,
    Resize,
    Addkey,
    Migrate,
    Destroy,
    /// Start or stop (owner, 2026-10-14).
    Start,
    Stop,
    /// Server containers made again from another image (`hs-deploy`, from
    /// this desktop).
    Reinstall,
}

impl Kind {
    #[must_use]
    pub fn operation(self) -> &'static str {
        match self {
            Self::New => "workspace.create",
            Self::Refresh => "workspace.refresh",
            Self::Resize => "workspace.resize",
            Self::Addkey => "workspace.addkey",
            Self::Migrate => "workspace.migrate",
            Self::Destroy => "workspace.destroy",
            Self::Start => "workspace.start",
            Self::Stop => "workspace.stop",
            Self::Reinstall => "server.reinstall",
        }
    }

    /// The word in the box title and in the row while it runs.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Refresh => "refresh",
            Self::Resize => "resize",
            Self::Addkey => "add key",
            Self::Migrate => "migrate",
            Self::Destroy => "destroy",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reinstall => "reinstall",
        }
    }

    /// Whether the action has a form before its plan.
    #[must_use]
    pub fn has_form(self) -> bool {
        !matches!(self, Self::Refresh | Self::Destroy | Self::Start | Self::Stop)
    }

    /// The CLI's question before the change.
    #[must_use]
    pub fn question(self, recover: bool) -> &'static str {
        match self {
            Self::New => "Create this workspace?",
            Self::Refresh if recover => "Recover this interrupted workspace refresh?",
            Self::Refresh => "Refresh this workspace root?",
            Self::Resize => "Resize this workspace disk?",
            Self::Addkey => "Prepare these keys? The VM is not restarted without a separate question.",
            Self::Migrate => "Migrate this workspace?",
            Self::Destroy => "Permanently destroy this workspace INCLUDING its persistent home?",
            Self::Start => "Start this workspace?",
            Self::Stop => "Stop this workspace? Its system shuts down; it is never forced.",
            Self::Reinstall => {
                "Reinstall these servers? Each container is made again from the image; what is only inside it is lost."
            }
        }
    }
}

/// What the loop starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub kind: Kind,
    pub vmid: Option<u32>,
    /// The plan's parameters, as the CLI sends them.
    pub parameters: Value,
    /// `addkey`: the public key file chosen (`~/.ssh/….pub`).
    pub key_files: Vec<String>,
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Apply the plan, with its secrets (the LUKS passphrase).
    Apply(Option<Value>),
    Cancel,
    /// `addkey`: restart the VM now or not.
    Restart(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Not started yet (reinstall: the servers after the one under way).
    Waiting,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub state: StepState,
}

/// What the work tells the box.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// The plan: its lines, the question, why it cannot go on (if so), and
    /// the prompt of a secret it needs.
    Plan {
        lines: Vec<String>,
        question: String,
        blocked: Option<String>,
        secret: Option<String>,
    },
    Steps(Vec<Step>),
    /// `addkey`: the key is prepared; restart now?
    Question {
        lines: Vec<String>,
    },
    Done {
        ok: bool,
        lines: Vec<String>,
    },
}

/// The desktop side the work needs.
#[derive(Debug, Clone)]
pub struct Desktop {
    pub home: PathBuf,
    pub loaded: Loaded,
    pub poll: Duration,
}

/// The CLI's text without its frame: the box is the frame.
#[must_use]
pub fn unframe(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut framed: Vec<String> = Vec::new();
    let flush = |framed: &mut Vec<String>, lines: &mut Vec<String>| {
        let indent = framed
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.chars().take_while(|c| *c == ' ').count())
            .min()
            .unwrap_or(0);
        for line in framed.drain(..) {
            lines.push(line.chars().skip(indent).collect::<String>().trim_end().to_owned());
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('╭') {
            let title = trimmed.trim_matches(|c| c == '╭' || c == '╮' || c == '─' || c == ' ');
            if !title.is_empty() {
                lines.push(title.to_owned());
            }
        } else if trimmed.starts_with('╰') {
            flush(&mut framed, &mut lines);
        } else if let Some(inner) = trimmed.strip_prefix('│') {
            let inner = inner.strip_suffix('│').unwrap_or(inner);
            framed.push(inner.to_owned());
        } else {
            flush(&mut framed, &mut lines);
            lines.push(line.trim_end().to_owned());
        }
    }
    flush(&mut framed, &mut lines);
    // A rule of dashes between groups is a blank line in the box; no blank
    // lines at the ends, and never two in a row.
    let mut tidy: Vec<String> = Vec::new();
    for line in lines {
        let line = if !line.is_empty() && line.chars().all(|c| c == '-' || c == '─') {
            String::new()
        } else {
            line
        };
        if line.is_empty() && tidy.last().is_none_or(String::is_empty) {
            continue;
        }
        tidy.push(line);
    }
    while tidy.last().is_some_and(String::is_empty) {
        tidy.pop();
    }
    tidy
}

fn steps(job: &Value) -> Vec<Step> {
    job["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| Step {
            name: view::text(&step["name"]),
            state: match step["state"].as_str() {
                Some("done") => StepState::Done,
                Some("failed") => StepState::Failed,
                _ => StepState::Running,
            },
        })
        .collect()
}

fn plain() -> Style {
    Style {
        color: false,
        width: 100,
    }
}

/// The plan's text as the CLI shows it.
fn plan_text(kind: Kind, summary: &Value) -> String {
    let style = plain();
    match kind {
        Kind::New => view::create_plan(summary, &style),
        Kind::Refresh => view::refresh_plan(summary, &style),
        Kind::Resize => app::resize_plan(summary, &style),
        Kind::Addkey => app::addkey_plan(summary, &style),
        Kind::Migrate => view::migrate_plan(summary, &style),
        Kind::Destroy => view::destroy_plan(summary, &style),
        Kind::Start | Kind::Stop => power_text(summary),
        Kind::Reinstall => String::new(),
    }
}

fn failed(progress: &(dyn Fn(Progress) + Send + Sync), message: String) {
    progress(Progress::Done {
        ok: false,
        lines: message.lines().map(str::to_owned).collect(),
    });
}

/// Do `start`: the plan, the user's decision from `decisions`, the job and
/// the desktop's part; each change is told through `progress`. When
/// `decisions` closes before an answer, nothing is applied.
pub async fn work(
    client: &Client,
    desktop: &Desktop,
    start: Start,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
    progress: &(dyn Fn(Progress) + Send + Sync),
) {
    let kind = start.kind;
    let mut parameters = start.parameters.clone();
    let mut private_shown = Vec::new();
    let mut public_keys = Vec::new();
    for file in &start.key_files {
        let path = expand_home(file, &desktop.home);
        match std::fs::read_to_string(&path) {
            Ok(text) => public_keys.push(text.trim().to_owned()),
            Err(error) => return failed(progress, format!("Could not read the public key {file}: {error}")),
        }
        match identity::private_key_for(&desktop.home, Path::new(file)) {
            Ok((_, shown)) => private_shown.push(shown),
            Err(error) => return failed(progress, error.0),
        }
    }
    if !public_keys.is_empty() {
        parameters["key"] = json!(public_keys.join("\n"));
    }
    let session = match client.plan(kind.operation(), parameters.clone()).await {
        Ok(session) => session,
        Err(error) => return failed(progress, client_error(error).0),
    };
    let summary = session.plan["summary"].clone();
    let recover = summary["mode"] == "recover";
    let blocked = if kind == Kind::Resize {
        app::resize_same_size(&summary)
    } else {
        None
    };
    let needs_passphrase = summary["secrets_required"]
        .as_array()
        .is_some_and(|names| names.iter().any(|name| name == "luks_passphrase"));
    progress(Progress::Plan {
        lines: unframe(&plan_text(kind, &summary)),
        question: kind.question(recover).to_owned(),
        blocked,
        secret: needs_passphrase.then(|| format!("LUKS passphrase for VM {}", view::text(&summary["vmid"]))),
    });
    let secrets = match decisions.recv().await {
        Some(Decision::Apply(secrets)) => secrets,
        Some(_) => {
            progress(Progress::Done {
                ok: true,
                lines: vec!["Cancelled. No changes were made.".to_owned()],
            });
            return;
        }
        None => return,
    };
    let job = app::run_job(&session, secrets.as_ref(), desktop.poll, |_, job| {
        progress(Progress::Steps(steps(job)));
    })
    .await;
    let mut result = match job {
        Ok(result) => result,
        Err(error) => return failed(progress, error.0),
    };
    let home = &desktop.home;
    let style = plain();
    let vmid = start
        .vmid
        .or_else(|| result["vmid"].as_u64().and_then(|vmid| u32::try_from(vmid).ok()))
        .unwrap_or(0);
    let text = match kind {
        Kind::New => app::after_create(home, &desktop.loaded.client.workspace_ssh, &mut result)
            .map(|()| view::create_result(&result, &style, Some(&home.display().to_string()))),
        Kind::Refresh => app::after_refresh(home, &desktop.loaded.client.workspace_ssh.user, vmid, &mut result)
            .map(|()| view::refresh_result(&result, &style)),
        Kind::Destroy => app::after_destroy(home, vmid, &mut result).map(|()| view::destroy_result(&result, &style)),
        Kind::Start | Kind::Stop => Ok(power_text(&result)),
        Kind::Reinstall => Ok(String::new()),
        Kind::Resize => Ok(app::resize_result_text(&summary)),
        Kind::Migrate => Ok(app::migrate_result_text(&result)),
        Kind::Addkey => {
            if let Err(message) =
                restart_if_asked(client, desktop, vmid, &parameters, &mut result, decisions, progress).await
            {
                return failed(progress, message);
            }
            let name = view::text(&result["name"]);
            app::after_addkey(home, &desktop.loaded, vmid, &private_shown, &mut result)
                .map(|()| app::addkey_result_text(vmid, &name, &result, &style))
        }
    };
    match text {
        Ok(text) => progress(Progress::Done {
            ok: true,
            lines: unframe(&text),
        }),
        Err(error) => failed(progress, error.0),
    }
}

/// `addkey` on a running VM: the key waits for a restart; ask, and restart
/// with a second plan when the user says so (as the CLI's second question).
async fn restart_if_asked(
    client: &Client,
    desktop: &Desktop,
    vmid: u32,
    parameters: &Value,
    result: &mut Value,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<(), String> {
    if parameters["restart"] == true || result["pending_restart"] != true {
        return Ok(());
    }
    let name = view::text(&result["name"]);
    progress(Progress::Question {
        lines: vec![
            format!(
                "The key is prepared. It applies after VM {vmid} ({name}) restarts; everything running on it will stop."
            ),
            format!("Restart {name} now?"),
        ],
    });
    if decisions.recv().await != Some(Decision::Restart(true)) {
        return Ok(());
    }
    let again = client
        .plan("workspace.addkey", json!({"vmid": vmid, "restart": true}))
        .await
        .map_err(|error| client_error(error).0)?;
    let restarted = app::run_job(&again, None, desktop.poll, |_, job| {
        progress(Progress::Steps(steps(job)));
    })
    .await
    .map_err(|error| error.0)?;
    for field in ["restarted", "pending_restart", "verified", "new_instance_at_next_start"] {
        result[field] = restarted[field].clone();
    }
    Ok(())
}

/// A desktop public key, named by its comment and identified by its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    pub path: String,
    pub name: String,
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Check {
        checked: bool,
        value: String,
        caption: String,
    },
    Text(String),
    Choice {
        options: Vec<String>,
        index: usize,
    },
    /// A size in GiB that only grows from `min`.
    Size {
        gib: u64,
        min: u64,
    },
    /// A size in whole GiB and hundreds of MiB, each part with its - and +
    /// (`2.3G  -2G+  -300M+`); `set` is false while it is the server's
    /// default, which is then not sent.
    Amount {
        g: u64,
        m: u64,
        part: Part,
        set: bool,
    },
}

/// The part of an amount that ← →, the wheel and its - + change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    G,
    M,
}

/// MiB in a GiB; the M part goes up in [`M_STEP`] to [`M_MAX`].
pub const MIB_PER_GIB: u64 = 1024;
pub const M_STEP: u64 = 100;
pub const M_MAX: u64 = 900;

impl Input {
    /// An amount of `mib`, the M part rounded down to its steps.
    #[must_use]
    pub fn amount(mib: u64, set: bool) -> Self {
        Self::Amount {
            g: mib / MIB_PER_GIB,
            m: (mib % MIB_PER_GIB / M_STEP * M_STEP).min(M_MAX),
            part: Part::G,
            set,
        }
    }

    /// The MiB of an amount; 0 for the other inputs.
    #[must_use]
    pub fn mib(&self) -> u64 {
        match self {
            Self::Amount { g, m, .. } => g * MIB_PER_GIB + m,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub input: Input,
    pub note: String,
}

impl Field {
    #[must_use]
    pub fn text(label: &'static str, value: &str, note: &str) -> Self {
        Self {
            label,
            input: Input::Text(value.to_owned()),
            note: note.to_owned(),
        }
    }

    #[must_use]
    pub fn choice(label: &'static str, options: Vec<String>, note: &str) -> Self {
        Self {
            label,
            input: Input::Choice { options, index: 0 },
            note: note.to_owned(),
        }
    }

    /// A size that starts at `gib` and cannot go below it.
    #[must_use]
    pub fn size(label: &'static str, gib: u64, note: &str) -> Self {
        Self {
            label,
            input: Input::Size { gib, min: gib },
            note: note.to_owned(),
        }
    }

    /// An amount that starts at `mib` (the server's default: not sent until
    /// it changes); 0 when the default is not known.
    #[must_use]
    pub fn amount(label: &'static str, mib: Option<u64>, note: &str) -> Self {
        Self {
            label,
            input: Input::amount(mib.unwrap_or(0), false),
            note: note.to_owned(),
        }
    }

    /// The text, or the chosen option.
    #[must_use]
    pub fn value(&self) -> String {
        match &self.input {
            Input::Check { value, .. } => value.clone(),
            Input::Text(text) => text.trim().to_owned(),
            Input::Choice { options, index } => options.get(*index).cloned().unwrap_or_default(),
            Input::Size { gib, .. } => format!("{gib}G"),
            Input::Amount { set: false, .. } => String::new(),
            Input::Amount { g, m: 0, .. } => format!("{g}G"),
            Input::Amount { .. } => format!("{}M", self.input.mib()),
        }
    }

    #[must_use]
    pub fn index(&self) -> usize {
        match &self.input {
            Input::Check { .. } | Input::Text(_) | Input::Size { .. } | Input::Amount { .. } => 0,
            Input::Choice { index, .. } => *index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
}

/// Where the keys go in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFocus {
    Secret,
    Apply,
    Cancel,
}

/// Where the box is.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Form(Form),
    /// Waiting for the plan.
    Asking,
    Plan {
        lines: Vec<String>,
        question: String,
        blocked: Option<String>,
        /// The prompt and what was typed.
        secret: Option<(String, String)>,
        focus: PlanFocus,
        scroll: usize,
    },
    Running {
        steps: Vec<Step>,
    },
    Question {
        lines: Vec<String>,
        yes: bool,
    },
    Done {
        ok: bool,
        lines: Vec<String>,
    },
}

/// The box.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub kind: Kind,
    pub vmid: Option<u32>,
    /// The workspace's name, for the title.
    pub name: String,
    pub stage: Stage,
    /// Esc while it runs: the box is not drawn, the work goes on.
    pub hidden: bool,
    /// Reinstall: the containers; each row shows ◐ while it runs.
    pub targets: Vec<u32>,
}

impl Action {
    /// The box title: `destroy 205 scratch`, `new workspace`.
    #[must_use]
    pub fn title(&self) -> String {
        match self.vmid {
            _ if self.kind == Kind::Reinstall => format!("reinstall {}", self.name),
            Some(vmid) => format!("{} {vmid} {}", self.kind.word(), self.name),
            None => "new workspace".to_owned(),
        }
    }

    /// Whether the work is under way (the row shows ◐).
    #[must_use]
    pub fn busy(&self) -> bool {
        matches!(
            self.stage,
            Stage::Asking | Stage::Plan { .. } | Stage::Running { .. } | Stage::Question { .. }
        )
    }

    /// Whether it changes something now (after Apply): the row shows it.
    #[must_use]
    pub fn running(&self) -> bool {
        matches!(self.stage, Stage::Running { .. } | Stage::Question { .. })
    }
}

/// The plan or the end of a start or a stop, in a few lines.
fn power_text(summary: &Value) -> String {
    let word = match summary["command"].as_str() {
        Some("start") => "Start",
        _ => "Stop",
    };
    format!(
        "{word} workspace\nVMID   : {}\nName   : {}\nNode   : {}\nState  : {}\n",
        view::text(&summary["vmid"]),
        view::text(&summary["name"]),
        view::text(&summary["node"]),
        view::text(&summary["status"])
    )
}
