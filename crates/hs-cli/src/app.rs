//! The `homestack` commands (Python `cli.main`): reads and operations
//! through the server, the same confirmations, `--json` output and exit
//! codes, and the desktop's own steps after an operation.
//!
//! Exit codes: 0 done (or declined), 1 error, 2 usage, 3 a plan that needs
//! `--yes`, 130 interrupted.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hs_client::{Client, ClientError, Session};
use serde_json::{Map, Value, json};

use crate::args::{self, Args, Command};
use crate::config::{self, Loaded};
use crate::system::{Console, System};
use crate::view::{self, Color, Style};
use crate::{AppError, expand_home, identity, installer, local_ssh};

/// What a run of the command uses from the desktop.
pub struct Context {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, when set.
    pub config_home: Option<PathBuf>,
    /// `$XDG_STATE_HOME`, when set (setup catalog snapshots).
    pub state_home: Option<PathBuf>,
    /// Whether stdin is a terminal (confirmations need one).
    pub stdin_tty: bool,
    pub style: Style,
    pub system: Arc<dyn System>,
    pub console: Box<dyn Console>,
    /// How often a job is read while it runs.
    pub poll: Duration,
    /// The servers of `install` (section 8 of phase-13c.md).
    pub servers: Arc<dyn crate::servers::Servers>,
}

/// The job being followed, for the message on Ctrl+C.
pub type Running = Arc<Mutex<Option<String>>>;

/// How a command ends without an error.
type Code = i32;

const CONFIRM_TTY: &str = "Interactive confirmation requires a TTY; use --yes for automation";
const CANCELLED: &str = "Cancelled. No changes were made.";
/// A job runs at most this long before the client stops following it.
const JOB_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// Run the command line `argv` (without the program name).
pub async fn run(argv: &[String], ctx: &mut Context) -> i32 {
    run_following(argv, ctx, &Running::default()).await
}

/// [`run`], keeping the job it follows in `running`.
pub async fn run_following(argv: &[String], ctx: &mut Context, running: &Running) -> i32 {
    let args = match args::parse(argv) {
        Ok(args) => args,
        Err(error) => {
            ctx.console.err(&format!("{error}\n"));
            return 2;
        }
    };
    if args.help || args.command == Command::None {
        let path = args
            .config
            .clone()
            .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
        ctx.console.out(&help(&path));
        return 0;
    }
    match execute(&args, ctx, running).await {
        Ok(code) => code,
        Err(error) => {
            if args.json {
                emit(
                    ctx,
                    &json!({"ok": false, "command": args.command.name(), "error": error.0}),
                );
            } else {
                let rendered = view::error(&error.0, &ctx.style);
                ctx.console.out(&rendered);
            }
            1
        }
    }
}

/// What to print when the user interrupts `argv`.
pub fn interrupted(argv: &[String], ctx: &mut Context, running: &Running) -> i32 {
    let args = args::parse(argv).ok();
    let job = running.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let mut message = "Interrupted".to_owned();
    if let Some(job) = job {
        message.push_str(&format!(
            "; {job} keeps running on the server: see `homestack status` before running it again"
        ));
    }
    match args {
        Some(args) if args.json => {
            emit(
                ctx,
                &json!({"ok": false, "command": args.command.name(), "error": message}),
            );
        }
        _ => {
            let text = ctx.style.paint(&format!("{message}."), Color::Bold);
            ctx.console.out(&format!("\n{text}\n"));
        }
    }
    130
}

fn emit(ctx: &mut Context, value: &Value) {
    let text = serde_json::to_string_pretty(&sorted(value)).unwrap_or_else(|_| value.to_string());
    ctx.console.out(&format!("{text}\n"));
}

/// Keys sorted at every level, as Python's `json.dumps(sort_keys=True)`.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|key| (key.clone(), sorted(&map[key]))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn help(config: &Path) -> String {
    let rows: &[(&str, &str)] = &[
        (
            "homestack",
            "In a terminal: the HomeStack TUI (workspaces, cluster, storage, log; m for the menu, q to quit).",
        ),
        (
            "homestack discover",
            "Read-only check of the local tools and keys and of what the server finds in Proxmox.",
        ),
        (
            "homestack install (--ssh USER@HOST | --herdr NAME) ...",
            "Install HomeStack: root on a node once for the Proxmox part, then a server on every node, client.toml, and homestack status at the end.",
        ),
        (
            "homestack uninstall (--ssh USER@HOST | --herdr NAME) ...",
            "Remove HomeStack from Proxmox; never a VM, a container or a volume.",
        ),
        (
            "homestack create VMID NAME",
            "Create a full clone from Gold with persistent ext4 home disk.",
        ),
        (
            "homestack refresh VMID|NAME",
            "Replace only the VM root with a fresh full clone from Gold; preserve persistent home.",
        ),
        (
            "homestack resize VMID|NAME --root-size SIZE | --home-size SIZE",
            "Grow a running workspace disk and its filesystem to the specified total size.",
        ),
        (
            "homestack addkey VMID|NAME [--key FILE.pub]",
            "Add one SSH public key (or, without --key, apply the keys already in sshkeys) to root and the workspace user; a running VM gets it at once.",
        ),
        (
            "homestack destroy VMID|NAME",
            "Permanently delete the VM and its persistent home disk.",
        ),
        (
            "homestack migrate VMID|NAME NODE --target-storage STORAGE",
            "Offline-migrate root, persistent home and cloud-init disk.",
        ),
        (
            "homestack setup ...",
            "Workspace setup; still in the Python client (phase 14).",
        ),
        (
            "homestack repo VMID|NAME [OWNER/REPO]",
            "Repository deploy keys; still in the Python client (phase 14).",
        ),
        (
            "homestack status",
            "Show the global HomeStack VM and storage dashboard.",
        ),
        (
            "homestack status VMID|NAME",
            "Show VM, network, QEMU Guest Agent and persistent home state.",
        ),
        (
            "homestack transport",
            "Check the HomeStack server instances and the configured Proxmox node.",
        ),
    ];
    let options: &[(&str, &str)] = &[
        (
            "--home-size SIZE",
            "Persistent home size for create, or total target home size for resize.",
        ),
        ("--root-size SIZE", "Total target root disk size for resize."),
        (
            "--ssh USER@HOST, --herdr NAME",
            "install, uninstall: how to reach root on a Proxmox node once; the Herdr tab or pane holds a root shell.",
        ),
        (
            "--key FILE",
            "Public key file (one key with a comment) to add with addkey.",
        ),
        (
            "--restart",
            "With addkey: restart a running VM without asking. Without it a running VM is never restarted by -y.",
        ),
        (
            "--node NODE",
            "Create on this cluster node; defaults to the configured Gold node.",
        ),
        (
            "--storage STORAGE",
            "Create root and persistent home on an allowed target-node storage instead of its layout default.",
        ),
        (
            "-y, --yes",
            "Accept the plan for create, refresh, resize, addkey, migrate or destroy.",
        ),
        (
            "--json",
            "Return JSON. Commands requiring confirmation return the resolved plan without --yes.",
        ),
        (
            "--profile NAME, --profil NAME",
            "Desktop profile (default when omitted); use profiles to list storage paths.",
        ),
        ("--config PATH", "Client configuration file in the default profile."),
        ("-h, --help", "Show this help screen."),
    ];
    let width = rows
        .iter()
        .chain(options)
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let mut text = String::from("HOMESTACK\n\nCommands\n");
    for (name, description) in rows {
        text.push_str(&format!("  {name:<width$}  {description}\n"));
    }
    text.push_str("\nOptions\n");
    for (name, description) in options {
        text.push_str(&format!("  {name:<width$}  {description}\n"));
    }
    text.push_str(&format!("\nConfiguration: {}\n", config.display()));
    text
}

/// `homestack status` with the configuration at `path`: how an install ends.
pub async fn status_of(path: &Path, ctx: &mut Context) -> Result<Code, AppError> {
    let args = Args {
        help: false,
        config: Some(path.to_owned()),
        json: false,
        yes: false,
        command: Command::Status { target: None },
    };
    let loaded = config::load(path)?;
    let client = Client::new(loaded.client_config(&ctx.home)?).map_err(client_error)?;
    let running = Running::default();
    let mut cli = Cli {
        args: &args,
        ctx,
        loaded: &loaded,
        client: &client,
        running: &running,
    };
    cli.global_status().await
}

async fn execute(args: &Args, ctx: &mut Context, running: &Running) -> Result<Code, AppError> {
    match &args.command {
        Command::Install(install) => return installer::run(args, install, hs_install::Command::Install, ctx).await,
        Command::Uninstall(install) => return installer::run(args, install, hs_install::Command::Uninstall, ctx).await,
        _ => {}
    }
    let path = args
        .config
        .clone()
        .unwrap_or_else(|| config::default_path(&ctx.home, ctx.config_home.as_deref()));
    let loaded = config::load(&expand_home(&path.display().to_string(), &ctx.home))?;
    let client = Client::new(loaded.client_config(&ctx.home)?).map_err(client_error)?;
    let mut cli = Cli {
        args,
        ctx,
        loaded: &loaded,
        client: &client,
        running,
    };
    match &args.command {
        Command::Status { target: None } => cli.global_status().await,
        Command::Status { target: Some(target) } => cli.workspace_status(target).await,
        Command::Transport => cli.transport().await,
        Command::Discover => cli.discover().await,
        Command::Create {
            vmid,
            name,
            home_size,
            storage,
            node,
        } => {
            let mut parameters = json!({"vmid": vmid, "name": name});
            for (key, value) in [("home_size", home_size), ("storage", storage), ("node", node)] {
                if let Some(value) = value {
                    parameters[key] = json!(value);
                }
            }
            cli.create(parameters).await
        }
        Command::Refresh { target } => {
            let vmid = cli.resolve(target).await?;
            cli.refresh(vmid).await
        }
        Command::Resize {
            target,
            root_size,
            home_size,
        } => {
            let vmid = cli.resolve(target).await?;
            let mut parameters = json!({"vmid": vmid});
            if let Some(size) = root_size {
                parameters["root_size"] = json!(size);
            }
            if let Some(size) = home_size {
                parameters["home_size"] = json!(size);
            }
            cli.resize(parameters).await
        }
        Command::Addkey { target, key, restart } => cli.addkey(target, key.as_deref(), *restart).await,
        Command::Destroy { target } => {
            let vmid = cli.resolve(target).await?;
            cli.destroy(vmid).await
        }
        Command::Migrate {
            target,
            target_node,
            target_storage,
        } => {
            let vmid = cli.resolve(target).await?;
            cli.migrate(json!({"vmid": vmid, "target_node": target_node, "target_storage": target_storage}))
                .await
        }
        Command::Setup(setup) => crate::setup::cli::run(args, setup, cli.ctx, cli.loaded, cli.client).await,
        Command::Repo { target, repository } => {
            crate::setup::cli::repo(args, target, repository.as_deref(), cli.ctx, cli.loaded, cli.client).await
        }
        Command::None | Command::Install(_) | Command::Uninstall(_) => Ok(0),
    }
}

/// A server error as the user sees it.
pub fn client_error(error: ClientError) -> AppError {
    match error {
        ClientError::Api { message, code, .. } if !message.is_empty() => {
            if code == "unauthorized" {
                AppError::new(format!("The server refused the client token: {message}"))
            } else {
                AppError::new(message)
            }
        }
        ClientError::Api { status, code, url, .. } => AppError::new(format!("{url} answered {status}: {code}")),
        ClientError::Unreachable(reason) => AppError::new(format!("No HomeStack server instance answered: {reason}")),
        ClientError::Lost { url, reason } => AppError::new(format!(
            "The server instance {url} stopped answering while it held this plan or job ({reason}). \
Nothing was sent to another instance. Read the state with `homestack status` and run the command again."
        )),
        ClientError::Config(message) => AppError::new(format!("Invalid client configuration: {message}")),
    }
}

/// How one operation asks before it changes anything.
struct Confirm {
    operation: &'static str,
    message: &'static str,
    question: &'static str,
    /// The wording when the plan recovers an interrupted refresh.
    recover: Option<(&'static str, &'static str)>,
}

struct Cli<'a> {
    args: &'a Args,
    ctx: &'a mut Context,
    loaded: &'a Loaded,
    client: &'a Client,
    running: &'a Running,
}

impl<'a> Cli<'a> {
    fn out(&mut self, text: &str) {
        self.ctx.console.out(text);
    }

    fn emit(&mut self, value: &Value) {
        emit(self.ctx, value);
    }

    fn style(&self) -> Style {
        self.ctx.style
    }

    fn execution(url: &url::Url) -> Value {
        json!({"type": "server", "url": url.as_str()})
    }

    /// Python's `resolve_workspace_target`: digits are a VMID, a name is
    /// looked up by the server.
    async fn resolve(&mut self, target: &str) -> Result<u32, AppError> {
        let text = target.trim();
        if is_vmid(text) {
            return text
                .parse()
                .map_err(|_| AppError::new(format!("Invalid workspace target {}", python_repr(text))));
        }
        if !valid_name(text) {
            return Err(invalid_target(text));
        }
        let found = self
            .client
            .get(&format!("/v1/workspaces/{text}"))
            .await
            .map_err(client_error)?;
        found["vmid"]
            .as_u64()
            .and_then(|vmid| u32::try_from(vmid).ok())
            .ok_or_else(|| AppError::new(format!("The server gave no VMID for {text}")))
    }

    async fn global_status(&mut self) -> Result<Code, AppError> {
        let (url, mut result) = self.client.get_from("/v1/status").await.map_err(client_error)?;
        let display = &self.loaded.client.display;
        result["summary"]["storage_unit"] = json!(display.storage_unit);
        result["summary"]["storage_decimals"] = json!(display.storage_decimals);
        let mut out = json!({
            "ok": true,
            "command": "status",
            "scope": "global",
            "config": self.loaded.path.display().to_string(),
            "execution": Self::execution(&url),
        });
        merge(&mut out, result);
        if self.args.json {
            self.emit(&out);
        } else {
            let text = view::global_status(&out, &self.style());
            self.out(&text);
        }
        Ok(0)
    }

    async fn workspace_status(&mut self, target: &str) -> Result<Code, AppError> {
        let text = target.trim();
        let numeric = is_vmid(text);
        if !numeric && !valid_name(text) {
            return Err(invalid_target(text));
        }
        let out = match self.client.get_from(&format!("/v1/workspaces/{text}")).await {
            Ok((url, result)) => {
                let mut out = json!({
                    "ok": true,
                    "command": "status",
                    "config": self.loaded.path.display().to_string(),
                    "execution": Self::execution(&url),
                });
                merge(&mut out, result);
                out
            }
            Err(ClientError::Api { code, .. }) if numeric && code == "not_found" => {
                json!({"ok": false, "command": "status", "vmid": text.parse::<u64>().unwrap_or(0), "status": "absent"})
            }
            Err(error) => return Err(client_error(error)),
        };
        if self.args.json {
            self.emit(&out);
        } else {
            let text = view::status_result(&out, &self.style());
            self.out(&text);
        }
        Ok(0)
    }

    async fn transport(&mut self) -> Result<Code, AppError> {
        let out = transport_report(self.client).await?;
        if self.args.json {
            self.emit(&out);
        } else {
            let text = view::transport_result(&out, &self.style());
            self.out(&text);
        }
        Ok(0)
    }

    async fn discover(&mut self) -> Result<Code, AppError> {
        let report = discovery_report(self.client, self.ctx.system.as_ref(), &self.ctx.home).await;
        let ok = report["ok"] == true;
        if self.args.json {
            self.emit(&report);
        } else {
            let text = view::discovery_report(&report, &self.style());
            self.out(&text);
        }
        Ok(if ok { 0 } else { 1 })
    }

    /// Ask "question [y/n] (n): " until the answer is yes or no; end of
    /// input is no.
    fn confirm(&mut self, question: &str) -> bool {
        loop {
            let Some(answer) = self.ctx.console.ask(&format!("{question} [y/n] (n): ")) else {
                return false;
            };
            match answer.trim().to_lowercase().as_str() {
                "" | "n" | "no" => return false,
                "y" | "yes" => return true,
                _ => self.out("Please enter Y or N\n"),
            }
        }
    }

    /// The plan, and whether to go on: `Ok(Err(code))` when the run ends
    /// here (3 for a JSON plan, 0 when declined).
    async fn plan(
        &mut self,
        confirm: &Confirm,
        parameters: Value,
        show: impl FnOnce(&Value, &Style) -> String,
        before_question: impl FnOnce(&Value) -> Option<String>,
    ) -> Result<Result<Session<'a>, Code>, AppError> {
        let client: &'a Client = self.client;
        let session = client.plan(confirm.operation, parameters).await.map_err(client_error)?;
        if self.args.yes {
            return Ok(Ok(session));
        }
        let summary = session.plan["summary"].clone();
        let (message, question) = match confirm.recover {
            Some(recover) if summary["mode"] == "recover" => recover,
            _ => (confirm.message, confirm.question),
        };
        if self.args.json {
            self.emit(&json!({
                "ok": false,
                "confirmation_required": true,
                "message": message,
                "plan": summary,
            }));
            return Ok(Err(3));
        }
        let shown = show(&summary, &self.style());
        self.out(&shown);
        if !self.ctx.stdin_tty {
            return Err(AppError::new(CONFIRM_TTY));
        }
        if let Some(message) = before_question(&summary) {
            self.out(&format!("{message}\n"));
            return Ok(Err(0));
        }
        if !self.confirm(question) {
            let text = self.ctx.style.paint(CANCELLED, Color::Bold);
            self.out(&format!("{text}\n"));
            return Ok(Err(0));
        }
        Ok(Ok(session))
    }

    /// Apply the plan and follow its job to the end, printing each step as
    /// it finishes; the job's result.
    async fn apply(&mut self, session: &Session<'a>, secrets: Option<&Value>) -> Result<Value, AppError> {
        let running = self.running;
        let json_mode = self.args.json;
        let style = self.ctx.style;
        let console = &mut self.ctx.console;
        let mut shown = 0;
        let finished = run_job(session, secrets, self.ctx.poll, |id, job| {
            *running.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("job {id} on {}", session.url()));
            if json_mode {
                return;
            }
            let steps = job["steps"].as_array().cloned().unwrap_or_default();
            while let Some(step) = steps.get(shown) {
                let mark = match step["state"].as_str() {
                    Some("done") => style.paint("✓", Color::Green),
                    Some("failed") => style.paint("✗", Color::Red),
                    _ => break,
                };
                console.out(&format!("  {mark} {}\n", view::text(&step["name"])));
                shown += 1;
            }
        })
        .await;
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = None;
        finished
    }

    async fn create(&mut self, parameters: Value) -> Result<Code, AppError> {
        let confirm = Confirm {
            operation: "workspace.create",
            message: "No changes made. Re-run with --yes to execute this plan.",
            question: "Create this workspace?",
            recover: None,
        };
        let session = match self.plan(&confirm, parameters, view::create_plan, |_| None).await? {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let mut result = self.apply(&session, None).await?;
        after_create(&self.ctx.home, &self.loaded.client.workspace_ssh, &mut result)?;
        if self.args.json {
            self.emit(&result);
        } else {
            let home = self.ctx.home.display().to_string();
            let text = view::create_result(&result, &self.style(), Some(&home));
            self.out(&text);
        }
        Ok(0)
    }

    async fn refresh(&mut self, vmid: u32) -> Result<Code, AppError> {
        let confirm = Confirm {
            operation: "workspace.refresh",
            message: "No changes made. Re-run with --yes to replace the VM root.",
            question: "Refresh this workspace root?",
            recover: Some((
                "No changes made. Re-run with --yes to recover the interrupted refresh.",
                "Recover this interrupted workspace refresh?",
            )),
        };
        let session = match self
            .plan(&confirm, json!({"vmid": vmid}), view::refresh_plan, |_| None)
            .await?
        {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let mut result = self.apply(&session, None).await?;
        after_refresh(
            &self.ctx.home,
            &self.loaded.client.workspace_ssh.user,
            vmid,
            &mut result,
        )?;
        if self.args.json {
            self.emit(&result);
        } else {
            let text = view::refresh_result(&result, &self.style());
            self.out(&text);
        }
        Ok(0)
    }

    async fn resize(&mut self, parameters: Value) -> Result<Code, AppError> {
        let confirm = Confirm {
            operation: "workspace.resize",
            message: "No changes made. Re-run with --yes to resize this disk.",
            question: "Resize this workspace disk?",
            recover: None,
        };
        let session = match self.plan(&confirm, parameters, resize_plan, resize_same_size).await? {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let summary = session.plan["summary"].clone();
        let needs_passphrase = summary["secrets_required"]
            .as_array()
            .is_some_and(|names| names.iter().any(|name| name == "luks_passphrase"));
        let secrets = if needs_passphrase {
            if self.args.json || !self.ctx.stdin_tty {
                return Err(AppError::new(
                    "LUKS requires a passphrase. Run resize interactively without --json; no disk changes made",
                ));
            }
            let prompt = format!("LUKS passphrase for VM {}: ", view::text(&summary["vmid"]));
            let passphrase = self
                .ctx
                .console
                .secret(&prompt)
                .ok_or_else(|| AppError::new("Cannot read a hidden LUKS passphrase; no disk changes made"))?;
            if passphrase.is_empty() {
                return Err(AppError::new("Empty LUKS passphrase; no disk changes made"));
            }
            Some(json!({"luks_passphrase": passphrase}))
        } else {
            None
        };
        let result = self.apply(&session, secrets.as_ref()).await?;
        if self.args.json {
            self.emit(&result);
        } else {
            let line = resize_result_text(&summary);
            self.out(&line);
        }
        Ok(0)
    }

    async fn addkey(&mut self, target: &str, key: Option<&str>, restart: bool) -> Result<Code, AppError> {
        let mut parameters = Map::new();
        let mut private_shown = Vec::new();
        if let Some(key) = key {
            let path = expand_home(key, &self.ctx.home);
            let text = std::fs::read_to_string(&path)
                .map_err(|error| AppError::new(format!("Could not read the public key {key}: {error}")))?;
            private_shown.push(identity::private_key_for(&self.ctx.home, Path::new(key))?.1);
            parameters.insert("key".to_owned(), json!(text.trim()));
        }
        let vmid = self.resolve(target).await?;
        parameters.insert("vmid".to_owned(), json!(vmid));
        parameters.insert("restart".to_owned(), json!(restart));
        let confirm = Confirm {
            operation: "workspace.addkey",
            message: "No changes made. Re-run with --yes to add this key.",
            question: "Prepare this key? The VM is not restarted without a separate question.",
            recover: None,
        };
        let session = match self
            .plan(&confirm, Value::Object(parameters), addkey_plan, |_| None)
            .await?
        {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let mut result = self.apply(&session, None).await?;
        let name = view::text(&result["name"]);
        let ask = !restart && !self.args.json && self.ctx.stdin_tty && result["pending_restart"] == true;
        if ask {
            let notice = format!(
                "The key is prepared. It applies after VM {vmid} ({name}) restarts; everything running on it will stop.\n"
            );
            self.out(&notice);
            if self.confirm(&format!("Restart {name} now?")) {
                let client: &'a Client = self.client;
                let again = client
                    .plan("workspace.addkey", json!({"vmid": vmid, "restart": true}))
                    .await
                    .map_err(client_error)?;
                let restarted = self.apply(&again, None).await?;
                for field in ["restarted", "pending_restart", "verified", "new_instance_at_next_start"] {
                    result[field] = restarted[field].clone();
                }
            }
        }
        after_addkey(&self.ctx.home, self.loaded, vmid, &private_shown, &mut result)?;
        if self.args.json {
            self.emit(&result);
        } else {
            self.addkey_result(vmid, &name, &result);
        }
        Ok(0)
    }

    fn addkey_result(&mut self, vmid: u32, name: &str, result: &Value) {
        let text = addkey_result_text(vmid, name, result, &self.ctx.style);
        self.out(&text);
    }

    async fn destroy(&mut self, vmid: u32) -> Result<Code, AppError> {
        let confirm = Confirm {
            operation: "workspace.destroy",
            message: "No changes made. Re-run with --yes to permanently delete the workspace and persistent home.",
            question: "Permanently destroy this workspace INCLUDING its persistent home?",
            recover: None,
        };
        let session = match self
            .plan(&confirm, json!({"vmid": vmid}), view::destroy_plan, |_| None)
            .await?
        {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let mut result = self.apply(&session, None).await?;
        after_destroy(&self.ctx.home, vmid, &mut result)?;
        if self.args.json {
            self.emit(&result);
        } else {
            let text = view::destroy_result(&result, &self.style());
            self.out(&text);
        }
        Ok(0)
    }

    async fn migrate(&mut self, parameters: Value) -> Result<Code, AppError> {
        let confirm = Confirm {
            operation: "workspace.migrate",
            message: "No changes made. Re-run with --yes to execute migration.",
            question: "Migrate this workspace?",
            recover: None,
        };
        let session = match self.plan(&confirm, parameters, view::migrate_plan, |_| None).await? {
            Ok(session) => session,
            Err(code) => return Ok(code),
        };
        let mut result = self.apply(&session, None).await?;
        result["ok"] = json!(true);
        if self.args.json {
            self.emit(&result);
        } else {
            let line = migrate_result_text(&result);
            self.out(&line);
        }
        Ok(0)
    }
}

/// `addkey`'s plan: the panel and the numbered steps (Python `cli.main`).
pub fn addkey_plan(plan: &Value, style: &Style) -> String {
    let key = if plan["key"].is_object() {
        format!(
            "{} ({})",
            view::text(&plan["key"]["comment"]),
            view::text(&plan["key"]["type"])
        )
    } else {
        format!("existing keys: {}", view::text(&plan["label"]))
    };
    let sections = vec![vec![
        ("VMID".to_owned(), view::text(&plan["vmid"])),
        ("Name".to_owned(), view::text(&plan["name"])),
        ("Node".to_owned(), view::text(&plan["node"])),
        ("State".to_owned(), view::text(&plan["status"])),
        ("Address".to_owned(), view::text(&plan["ip"])),
        ("Key".to_owned(), key),
    ]];
    let mut text = view::kv_panel("ADD KEY PLAN — NO CHANGES MADE YET", &sections, style);
    for (number, step) in plan["steps"].as_array().into_iter().flatten().enumerate() {
        text.push_str(&format!("  {}. {}\n", number + 1, view::text(step)));
    }
    text
}

/// `resize`'s plan panel (Python `cli.main`).
pub fn resize_plan(plan: &Value, style: &Style) -> String {
    let guest = &plan["guest"];
    let current = plan["current_size_gib"]
        .as_f64()
        .map_or_else(|| "—".to_owned(), |gib| format!("{}G", view::compact(gib)));
    let encryption = guest["crypt"]["auth"].as_str().unwrap_or("none").to_owned();
    let lvm = if guest["lvm"].is_object() || guest["lvm"] == true {
        "Grow dedicated logical volume"
    } else {
        "none"
    };
    let sections = vec![vec![
        ("VMID".to_owned(), view::text(&plan["vmid"])),
        ("Name".to_owned(), view::text(&plan["name"])),
        ("Node".to_owned(), view::text(&plan["node"])),
        ("Disk".to_owned(), view::text(&plan["disk"])),
        ("Current size".to_owned(), current),
        ("Target size".to_owned(), view::text(&plan["size"])),
        (
            "Filesystem".to_owned(),
            format!("Grow {} online", view::text(&guest["filesystem"])),
        ),
        ("Encryption".to_owned(), encryption),
        ("LVM".to_owned(), lvm.to_owned()),
    ]];
    view::kv_panel("RESIZE PLAN — NO CHANGES MADE YET", &sections, style)
}

pub fn is_vmid(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Python `models.validate_name`'s pattern: `[A-Za-z][A-Za-z0-9.-]{0,62}`.
pub fn valid_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic())
        && text.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn invalid_target(text: &str) -> AppError {
    AppError::new(format!(
        "Invalid workspace target {}. Use a numeric VMID or a workspace name whose first character is a letter.",
        python_repr(text)
    ))
}

/// Python's `repr` of a string, as its messages quote targets.
fn python_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn vmid_of(result: &Value) -> Result<u32, AppError> {
    result["vmid"]
        .as_u64()
        .and_then(|vmid| u32::try_from(vmid).ok())
        .ok_or_else(|| AppError::new("The job's result has no VMID"))
}

/// Add `extra`'s keys to `target`.
fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) {
        target.extend(extra);
    }
}

/// The BK retrieval key files of `vmid` still on the desktop
/// (`backup.retrieval_key_paths`); their cleanup arrives with BK in phase
/// 14d, so they are listed, not removed.
#[must_use]
pub fn backup_keys_left(home: &Path, vmid: u32) -> Vec<String> {
    let directory = home.join(".ssh").join("homestack").join("backup");
    ["archive", "status"]
        .iter()
        .flat_map(|role| [format!("vm{vmid}-bk-{role}"), format!("vm{vmid}-bk-{role}.pub")])
        .map(|name| directory.join(name))
        .filter(|path| path.symlink_metadata().is_ok())
        .map(|path| path.display().to_string())
        .collect()
}

/// What `homestack transport` reports: every server's state and
/// configuration version, and the Proxmox node the servers are set to.
pub async fn transport_report(client: &Client) -> Result<Value, AppError> {
    let urls: Vec<url::Url> = client.urls().cloned().collect();
    let mut instances = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        instances.push(match client.config_version_on(index).await {
            Ok(version) => json!({"url": url.as_str(), "ok": true, "config_version": version, "error": null}),
            Err(error) => {
                json!({"url": url.as_str(), "ok": false, "config_version": null, "error": client_error(error).0})
            }
        });
    }
    if !instances.iter().any(|instance| instance["ok"] == true) {
        return Err(AppError::new(format!(
            "No HomeStack server instance answered: {}",
            instances
                .iter()
                .map(|i| format!("{}: {}", view::text(&i["url"]), view::text(&i["error"])))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    let (url, stored) = client.get_from("/v1/config").await.map_err(client_error)?;
    let node = stored
        .as_str()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("node")?.get("name")?.as_str().map(str::to_owned))
        .ok_or_else(|| AppError::new("The server's configuration names no [node]"))?;
    let status = client
        .get(&format!("/v1/pve/nodes/{node}/status"))
        .await
        .map_err(client_error)?;
    Ok(json!({
        "ok": true,
        "command": "transport",
        "transport": {"type": "server", "url": url.as_str(), "instances": instances},
        "pve": {
            "node": node,
            "version": status["pveversion"],
            "kernel": status["kversion"],
            "uptime": status["uptime"],
        },
    }))
}

/// What `homestack discover` reports: the local tools and keys, and what
/// the server finds in Proxmox.
pub async fn discovery_report(client: &Client, system: &dyn System, home: &Path) -> Value {
    let mut commands = Map::new();
    for name in ["ssh", "ssh-keygen", "fido2-token", "bwrap"] {
        commands.insert(name.to_owned(), json!(system.which(name).is_some()));
    }
    let mut report = json!({
        "ok": false,
        "config_used": true,
        "read_only": true,
        "local": {
            "commands": commands,
            "hardware_ssh_identities": local_ssh::discover_hardware_identities(home),
        },
        "server": null,
        "errors": [],
    });
    match client.get_from("/v1/discovery").await {
        Ok((url, discovery)) => {
            report["server"] = json!({"url": url.as_str(), "discovery": discovery});
            report["ok"] = json!(true);
        }
        Err(error) => {
            report["errors"] = json!([client_error(error).0]);
        }
    }
    report
}

/// Apply `session`'s plan and read its job until it ends; `each` sees the
/// job's id and every read of it. The job's result, or its error.
pub async fn run_job(
    session: &Session<'_>,
    secrets: Option<&Value>,
    poll: Duration,
    mut each: impl FnMut(&str, &Value),
) -> Result<Value, AppError> {
    let version = session
        .config_version()
        .await
        .map_err(client_error)?
        .ok_or_else(|| AppError::new("The server has no configuration; upload server.toml first."))?;
    let job = session.apply_with(&version, secrets).await.map_err(client_error)?;
    let id = job["id"].as_str().unwrap_or_default().to_owned();
    let deadline = tokio::time::Instant::now() + JOB_LIMIT;
    let job = loop {
        let job = session.job(&id).await.map_err(client_error)?;
        each(&id, &job);
        if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
            break job;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::new(format!(
                "Job {id} did not finish in time; it may still run on {}",
                session.url()
            )));
        }
        tokio::time::sleep(poll).await;
    };
    if job["state"] == "succeeded" {
        return Ok(job["result"].clone());
    }
    let message = job["error"]["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .map_or_else(|| format!("Job {id} failed"), str::to_owned);
    Err(AppError::new(message))
}

/// Remove the workspace's own `known_hosts` file (the paths removed).
pub fn forget_known_hosts(home: &Path, vmid: u32) -> Result<Vec<String>, AppError> {
    local_ssh::forget_known_hosts(home, vmid)
        .map(|removed| removed.iter().map(|path| path.display().to_string()).collect())
}

/// The desktop's part of `create`, after the job: the old `known_hosts`
/// of the VMID goes, the workspace's SSH config is written.
pub fn after_create(home: &Path, ssh: &hs_config::client::WorkspaceSsh, result: &mut Value) -> Result<(), AppError> {
    let vmid = vmid_of(result)?;
    let name = view::text(&result["name"]);
    let ip = view::text(&result["ip"]);
    let removed = forget_known_hosts(home, vmid)?;
    let path = local_ssh::write_local_ssh_config(home, ssh, vmid, &name, &ip).map_err(|error| {
        AppError::new(format!(
            "Workspace VM {vmid} ({name}) was created and verified successfully, but local SSH config generation failed: {error}"
        ))
    })?;
    merge(
        result,
        json!({
            "ok": true,
            "ssh_known_hosts_removed": removed,
            "ssh_config_path": path.display().to_string(),
            "ssh": {"root": format!("ssh root@{name}"), "user": format!("ssh {name}")},
        }),
    );
    Ok(())
}

/// The desktop's part of `refresh`: `known_hosts` goes unless the host
/// key stayed.
pub fn after_refresh(home: &Path, user: &str, vmid: u32, result: &mut Value) -> Result<(), AppError> {
    let ip = view::text(&result["ip"]);
    let removed = if result["host_key_changed"] == false {
        Vec::new()
    } else {
        forget_known_hosts(home, vmid)?
    };
    merge(
        result,
        json!({
            "ok": true,
            "ssh_known_hosts_removed": removed,
            "ssh": {"root": format!("ssh root@{ip}"), "user": format!("ssh {user}@{ip}")},
        }),
    );
    Ok(())
}

/// The desktop's part of `destroy` once the VM is verified absent, in
/// Python's order: BK keys (listed), SSH config, `known_hosts`.
pub fn after_destroy(home: &Path, vmid: u32, result: &mut Value) -> Result<(), AppError> {
    let name = view::text(&result["name"]);
    let local = (|| -> Result<Value, AppError> {
        let left = backup_keys_left(home, vmid);
        let removed = local_ssh::remove_local_ssh_config(home, vmid)?;
        let forgotten = forget_known_hosts(home, vmid)?;
        Ok(json!({
            "backup_keys_left": left,
            "ssh_config_removed": removed.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "ssh_known_hosts_removed": forgotten,
        }))
    })()
    .map_err(|error| {
        AppError::new(format!(
            "Workspace VM {vmid} ({name}) was destroyed successfully, but local SSH cleanup failed: {error}"
        ))
    })?;
    merge(result, local);
    result["ok"] = json!(true);
    Ok(())
}

/// The desktop's part of `addkey`: `known_hosts` goes after a restart, and
/// a key given as a file joins the local SSH settings.
pub fn after_addkey(
    home: &Path,
    loaded: &Loaded,
    vmid: u32,
    private_shown: &[String],
    result: &mut Value,
) -> Result<(), AppError> {
    let removed = if result["restarted"] == true {
        forget_known_hosts(home, vmid)?
    } else {
        Vec::new()
    };
    result["known_hosts_removed"] = json!(removed);
    if !private_shown.is_empty() {
        let name = view::text(&result["name"]);
        let ip = view::text(&result["ip"]);
        let local = identity::sync_local_ssh(
            home,
            &loaded.path,
            &loaded.client.workspace_ssh,
            private_shown,
            vmid,
            &name,
            &ip,
        );
        match local {
            Ok(local) => result["local"] = local,
            Err(error) => result["local_error"] = json!(error.0),
        }
    }
    Ok(())
}

/// What `addkey` says at the end (Python `cli.main`).
#[must_use]
pub fn addkey_result_text(vmid: u32, name: &str, result: &Value, style: &Style) -> String {
    let state = if result["verified"] == true {
        "; restarted and verified in root and user authorized_keys."
    } else if result["pending_restart"] == true {
        "; VM NOT restarted, the key applies after the next restart."
    } else {
        "; applies on the next start."
    };
    let mut text = format!(
        "VM {vmid} ({name}): keys ready: {}{state}\n",
        view::text(&result["key"])
    );
    if result["new_instance_at_next_start"] == true {
        text.push_str(
            "The next start runs Cloud-Init again and the VM gets new SSH host keys; \
remove the old one with `ssh-keygen -R ADDRESS` after that start.\n",
        );
    }
    if let Some(path) = result["local"]["ssh_config"].as_str() {
        text.push_str(&format!("Local ssh config updated: {path}\n"));
    }
    if let Some(error) = result["local_error"].as_str() {
        let warning = style.paint(&format!("WARNING: local ssh config not updated: {error}"), Color::Red);
        text.push_str(&format!("{warning}\n"));
    }
    text
}

/// What `resize` says at the end.
#[must_use]
pub fn resize_result_text(summary: &Value) -> String {
    format!(
        "VM {}: {} resized to {}; filesystem expanded.\n",
        view::text(&summary["vmid"]),
        view::text(&summary["disk"]),
        view::text(&summary["size"])
    )
}

/// What `migrate` says at the end.
#[must_use]
pub fn migrate_result_text(result: &Value) -> String {
    format!(
        "VM {} ({}): migrated to {} on {}; power state {}.\n",
        view::text(&result["vmid"]),
        view::text(&result["name"]),
        view::text(&result["target_node"]),
        view::text(&result["target_storage"]),
        view::text(&result["status"])
    )
}

/// `resize`'s refusal before the question when the size does not change.
#[must_use]
pub fn resize_same_size(plan: &Value) -> Option<String> {
    let target = plan["target_size_gib"].as_f64();
    (target.is_some() && target == plan["current_size_gib"].as_f64()).then(|| {
        let size = view::text(&plan["size"]);
        format!("Disk is already {size}: {size} -> {size} changes nothing.")
    })
}
