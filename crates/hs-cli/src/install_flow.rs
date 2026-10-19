//! The install and the uninstall as steps that the TUI's install screen
//! drives (phase 13b step 3): root is reached, the
//! cluster read, the choices answered and reviewed, the plan applied, the
//! servers deployed and `client.toml` written. Nothing here talks to a
//! terminal; what happens is told through `say` and `progress`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hs_install::{Access, Applied, Command as Install, InstallError, Plan, Root, Settings};

use crate::AppError;
use crate::servers::{Files, ServerPlan, Servers, ensure_api_token, plan_servers, write_private};

/// What a setting of the install screen was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The indexes of the chosen items.
    Picked(Vec<usize>),
    Text(String),
}

/// The review: what was chosen and what will change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// Headed groups of lines.
    pub sections: Vec<(String, Vec<String>)>,
    /// What stops the install; Install cannot be pressed while there is one.
    pub problems: Vec<String>,
}

/// The draft's key of the server domain.
const DRAFT_DOMAIN: &str = "server-domain";

fn failure(error: InstallError) -> AppError {
    AppError::new(error.to_string())
}

/// `Root access: <how>; <host> is Proxmox VE <version>, <topology>.`
#[must_use]
pub fn access_line(root: &dyn Root, access: &Access) -> String {
    format!(
        "Root access: {}; {} is Proxmox VE {}, {}.",
        root.describe(),
        access.host,
        access.version,
        access.topology.describe()
    )
}

/// One install or uninstall with root reached.
pub struct Session {
    pub command: Install,
    root: Box<dyn Root>,
    pub access: Access,
    pub access_line: String,
    /// The options given before any question (token directory, test part).
    pub settings: Settings,
    /// What is asked, starting from the kept choices; none for an uninstall.
    pub questions: Vec<hs_install::Question>,
    /// The server domain of the last run.
    pub draft_domain: String,
    pub want_servers: bool,
    pub server_image: Option<String>,
    pub files: Files,
    input: crate::install_config::Input,
    servers: Arc<dyn Servers>,
}

/// A plan made from answers, with what the review shows.
#[derive(Clone)]
pub struct Review {
    pub plan: Option<Plan>,
    pub servers: Option<ServerPlan>,
    pub domain: Option<String>,
    pub summary: Summary,
}

impl Review {
    /// Whether it can be applied: a plan and no problem.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.plan.is_some() && self.summary.problems.is_empty()
    }
}

impl Session {
    /// Check the access and read what can be chosen.
    pub fn start(
        mut root: Box<dyn Root>,
        command: Install,
        settings: Settings,
        want_servers: bool,
        server_image: Option<String>,
        files: Files,
        servers: Arc<dyn Servers>,
    ) -> Result<Self, AppError> {
        let access = hs_install::probe(root.as_mut()).map_err(failure)?;
        let access_line = access_line(root.as_ref(), &access);
        let input = if want_servers {
            crate::install_config::Input::load(&files.install_config())?
        } else {
            crate::install_config::Input::default()
        };
        let mut questions = if command == Install::Install {
            hs_install::questions(root.as_mut(), &access, &settings).map_err(failure)?
        } else {
            Vec::new()
        };
        if want_servers {
            questions.extend(input.questions());
            questions.push(input.key_question(&files.home));
        }
        let kept = load_draft(&draft_path(&settings));
        apply_draft(&mut questions, &kept);
        // Without one in the draft, the domain the servers were deployed
        // with: an install over an installed system starts from it.
        let draft_domain = kept
            .get(DRAFT_DOMAIN)
            .and_then(|values| values.first())
            .filter(|domain| !domain.is_empty())
            .cloned()
            .or_else(|| deployed_domain(&files.deploy_toml()))
            .unwrap_or_default();
        Ok(Self {
            command,
            root,
            access,
            access_line,
            settings,
            questions,
            draft_domain,
            want_servers,
            server_image,
            files,
            input,
            servers,
        })
    }

    /// How root is reached (`ssh root@pve2`).
    #[must_use]
    pub fn describe(&self) -> String {
        self.root.describe()
    }

    /// Read the cluster's choices again (F5, rescan cluster), keeping what
    /// was picked where it is still offered; a guest newly found by its
    /// tags comes ticked, as on the first read. `domain` is the one typed.
    pub fn rescan(&mut self, answers: &[Answer], domain: Option<&str>) -> Result<(), AppError> {
        let mut questions = hs_install::questions(self.root.as_mut(), &self.access, &self.settings).map_err(failure)?;
        if self.want_servers {
            questions.extend(self.input.questions());
            questions.push(self.input.key_question(&self.files.home));
        }
        for question in &mut questions {
            if crate::install_config::is_text(question) {
                if let Some((_, Answer::Text(value))) = self
                    .questions
                    .iter()
                    .zip(answers)
                    .find(|(old, _)| old.option == question.option)
                {
                    question.choices[0].value.clone_from(value);
                }
                continue;
            }
            let Some((old, Answer::Picked(picked))) = self
                .questions
                .iter()
                .zip(answers)
                .find(|(old, _)| old.option == question.option)
            else {
                continue;
            };
            let kept: Vec<&str> = picked
                .iter()
                .filter_map(|index| old.choices.get(*index))
                .map(|choice| choice.value.as_str())
                .collect();
            question.defaults = kept
                .iter()
                .filter_map(|value| question.choices.iter().position(|choice| choice.value == *value))
                .collect();
            if question.option == hs_install::discover::OPTION {
                question.defaults.extend(
                    question
                        .choices
                        .iter()
                        .enumerate()
                        .filter(|(_, choice)| !old.choices.iter().any(|known| known.value == choice.value))
                        .map(|(index, _)| index),
                );
            }
        }
        self.questions = questions;
        if let Some(domain) = domain {
            domain.clone_into(&mut self.draft_domain);
        }
        Ok(())
    }

    /// The plan of `settings` as they are. Nothing is changed.
    pub fn plan(&mut self, settings: &Settings) -> Result<Plan, InstallError> {
        hs_install::make_plan(self.root.as_mut(), &self.access, self.command, settings)
    }

    /// The servers' part of `plan` under `domain`.
    #[must_use]
    pub fn server_plan(&self, plan: &Plan, domain: &str) -> ServerPlan {
        plan_servers(
            plan,
            domain,
            self.server_image.as_deref(),
            &self.files,
            self.servers.as_ref(),
            self.input.text(),
        )
    }

    /// The plan of the answers to [`Session::questions`] and `domain`, and
    /// the review of it. Nothing is changed.
    pub fn review(&mut self, answers: &[Answer], domain: Option<&str>) -> Review {
        for (question, answer) in self.questions.iter().zip(answers) {
            if question.option == crate::install_config::KEY_OPTION {
                if let Answer::Picked(picked) = answer {
                    self.input.identities(
                        picked
                            .iter()
                            .filter_map(|index| question.choices.get(*index))
                            .map(|choice| choice.value.clone())
                            .collect(),
                    );
                }
                continue;
            }
            if let Answer::Text(text) = answer {
                self.input.answer(question.option, text);
            }
        }
        let chosen = answered(&self.settings, &self.questions, answers);
        let domain = domain.filter(|domain| !domain.is_empty()).map(str::to_owned);
        match self.plan(&chosen) {
            Ok(plan) => {
                let mut summary = summary_of(&self.access_line, &plan, &self.files.home);
                let servers = self
                    .want_servers
                    .then(|| self.server_plan(&plan, domain.as_deref().unwrap_or_default()));
                if let Some(server_plan) = &servers {
                    servers_section(&mut summary, server_plan);
                }
                Review {
                    plan: Some(plan),
                    servers,
                    domain,
                    summary,
                }
            }
            Err(error) => Review {
                plan: None,
                servers: None,
                domain,
                summary: Summary {
                    sections: Vec::new(),
                    problems: error.to_string().lines().map(str::to_owned).collect(),
                },
            },
        }
    }

    /// Keep the answers for the next run.
    pub fn keep(&self, answers: &[Answer], domain: Option<&str>) {
        save_draft(&draft_path(&self.settings), &self.questions, answers, domain);
    }

    /// Apply `plan` in Proxmox; `progress` gets a line per step.
    pub fn apply(&mut self, plan: &Plan, progress: &mut dyn FnMut(&str)) -> Result<Option<Applied>, AppError> {
        if plan.steps.is_empty() {
            return Ok(None);
        }
        hs_install::apply(self.root.as_mut(), plan, &plan.settings, progress)
            .map(Some)
            .map_err(failure)
    }

    /// The servers of this session.
    #[must_use]
    pub fn servers(&self) -> Arc<dyn Servers> {
        Arc::clone(&self.servers)
    }
}

/// After the Proxmox part: the client token, `server.toml` and
/// `deploy.toml`, the deployment, then `client.toml` at `client_path`.
/// `say` gets every line of it.
pub async fn deploy(
    files: &Files,
    server_plan: &ServerPlan,
    client_path: &Path,
    servers: &dyn Servers,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), AppError> {
    let Some(config) = &server_plan.config else {
        return Err(AppError::new(
            "The servers cannot be deployed: the deployment is incomplete",
        ));
    };
    if let Some(kept) = write_private(&files.install_config(), &server_plan.install_config).map_err(AppError::new)? {
        say(&format!(
            "the previous install configuration is kept as {}",
            kept.display()
        ));
    }
    if ensure_api_token(&files.api_token()).map_err(AppError::new)? {
        say(&format!(
            "new client token in {}",
            tilde(&files.api_token(), &files.home)
        ));
    }
    for (path, text) in [
        (files.server_toml(), server_plan.server_toml.as_str()),
        (files.deploy_toml(), server_plan.deploy_toml.as_str()),
    ] {
        if let Some(kept) = write_private(&path, text).map_err(AppError::new)? {
            say(&format!(
                "the old {} is kept as {}",
                tilde(&path, &files.home),
                kept.display()
            ));
        }
    }
    servers.deploy(config, say).await.map_err(|error| {
        AppError::new(format!(
            "Deploying the servers failed: {error}\nThe Proxmox part is installed; run homestack install again to retry."
        ))
    })?;
    let (urls, ca_file) = servers.client_server(config);
    let token_file = tilde(&files.api_token(), &files.home);
    let text = crate::servers::client_toml(&server_plan.install_config, &urls, ca_file, &token_file)
        .map_err(|error| AppError::new(format!("client.toml: {error}")))?;
    if let Some(kept) = write_private(client_path, &text).map_err(AppError::new)? {
        say(&format!("the old client.toml is kept as {}", kept.display()));
    }
    say(&format!("{} written", tilde(client_path, &files.home)));
    Ok(())
}

/// Retire the active connection after a successful production uninstall.
/// Saved settings, deployment files and credentials remain available.
pub fn retire_client(plan: &Plan, path: &Path) -> Result<Option<PathBuf>, AppError> {
    if plan.command != Install::Uninstall || plan.settings.test || !plan.node_errors.is_empty() {
        return Ok(None);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".uninstalled");
    let archived = path.with_file_name(name);
    match std::fs::rename(path, &archived) {
        Ok(()) => Ok(Some(archived)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::new(format!(
            "Cannot retire {} after uninstall: {}",
            path.display(),
            error.kind()
        ))),
    }
}

/// Where `client.toml` is: `--config`, or the default.
#[must_use]
pub fn client_path(given: Option<&Path>, home: &Path, config_home: Option<&Path>) -> PathBuf {
    let path = given.map_or_else(|| crate::config::default_path(home, config_home), Path::to_path_buf);
    crate::expand_home(&path.display().to_string(), home)
}

/// `settings` with the answers to `questions` put in.
pub fn answered(settings: &Settings, questions: &[hs_install::Question], answers: &[Answer]) -> Settings {
    let mut settings = settings.clone();
    for (question, answer) in questions.iter().zip(answers) {
        let Answer::Picked(picked) = answer else { continue };
        let values: Vec<String> = picked
            .iter()
            .filter_map(|index| question.choices.get(*index))
            .map(|choice| choice.value.clone())
            .collect();
        match question.option {
            "--storages" => settings.storages = values,
            "--snippet-storage" => settings.snippet_storage = values.into_iter().next(),
            "--service-storages" => settings.service_storages = values,
            "--template-storage" => settings.template_storage = values.into_iter().next(),
            "--bridge" => settings.bridge = values.into_iter().next(),
            "--gold" => settings.gold = values.first().and_then(|vmid| vmid.parse().ok()),
            "--adopt" => settings.adopt = values.iter().filter_map(|vmid| vmid.parse().ok()).collect(),
            _ => {}
        }
    }
    settings
}

/// `path` with the home directory written as `~`.
pub fn tilde(path: &std::path::Path, home: &std::path::Path) -> String {
    path.strip_prefix(home)
        .map_or_else(|_| path.display().to_string(), |rest| format!("~/{}", rest.display()))
}

/// What an ACL path is: its kind (in the order shown) and its name.
fn place(path: &str) -> (u8, &'static str, String) {
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "nodes"] => (0, "whole cluster", "nodes".to_owned()),
        ["", "sdn"] => (0, "whole cluster", "networks".to_owned()),
        ["", "storage"] => (0, "whole cluster", "storages".to_owned()),
        ["", "pool", pool] => (1, "pools", (*pool).to_owned()),
        ["", "storage", id] => (2, "storages", (*id).to_owned()),
        ["", "sdn", "zones", _, bridge] => (3, "networks", (*bridge).to_owned()),
        _ => (4, "other", path.to_owned()),
    }
}

/// The plan's steps grouped by what they change, a short line each.
fn grouped_changes(plan: &Plan, home: &std::path::Path) -> Vec<(String, Vec<String>)> {
    use hs_install::Action;
    let mut account = Vec::new();
    let mut roles: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    let mut pools: std::collections::BTreeMap<&str, Vec<String>> = std::collections::BTreeMap::new();
    let mut gold = Vec::new();
    let mut members = Vec::new();
    // Who -> (places given, places taken).
    let mut permissions: std::collections::BTreeMap<String, Permissions> = std::collections::BTreeMap::new();
    let mut entries = 0usize;
    let mut snippets: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for step in &plan.steps {
        match &step.action {
            Action::Pveum(args) => {
                let words: Vec<&str> = args.iter().map(String::as_str).collect();
                match words.as_slice() {
                    ["user", "add", user, ..] => account.push(format!("new user {user}")),
                    ["user", "delete", user] => account.push(format!("remove user {user}")),
                    ["user", "token", "delete", _, token] => account.push(format!("remove token {token}")),
                    ["user", "token", "modify", _, token, ..] => {
                        account.push(format!("token {token}: separate its privileges"));
                    }
                    ["role", "add", role, ..] => roles.entry("new").or_default().push((*role).to_owned()),
                    ["role", "modify", role, ..] => roles.entry("changed").or_default().push((*role).to_owned()),
                    ["role", "delete", role] => roles.entry("removed").or_default().push((*role).to_owned()),
                    ["pool", "add", pool, ..] => pools.entry("new").or_default().push((*pool).to_owned()),
                    ["pool", "delete", pool] => pools.entry("removed").or_default().push((*pool).to_owned()),
                    ["pool", "modify", ..] => {
                        // The step says it with the VM's name.
                        let line = step.description.trim_end_matches(", as its tags say").to_owned();
                        // Gold's pool is homestack-gold, or the test pool for
                        // the test part's Gold.
                        if step.description.contains("Gold ") {
                            gold.push(line);
                        } else {
                            members.push(line);
                        }
                    }
                    ["acl", action, path, _, who, "--roles", roles_given] => {
                        let short = who
                            .strip_prefix("homestack@pve!")
                            .map_or_else(|| "the user homestack@pve".to_owned(), |token| format!("token {token}"));
                        let count = roles_given.split(',').count();
                        entries += count;
                        let slot = permissions.entry(short).or_default();
                        let (order, kind, name) = place(path);
                        let list = if *action == "modify" {
                            slot.given.entry((order, kind)).or_default()
                        } else {
                            slot.taken.entry((order, kind)).or_default()
                        };
                        if !list.contains(&name) {
                            list.push(name);
                        }
                    }
                    _ => account.push(step.description.clone()),
                }
            }
            Action::Pvesh(_) => gold.push(step.description.replace("tag Gold", "tag VM")),
            Action::CreateToken {
                token, file, replaced, ..
            } => {
                let mut line = format!("new token {token}, secret to {}", tilde(file, home));
                if let Some(old) = replaced {
                    let name = old
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    account.push(line.clone());
                    line = format!("old file kept as {name}");
                }
                account.push(line);
            }
            Action::WriteSnippet { node, name, .. } => {
                snippets.entry(format!("write {name}")).or_default().push(node.clone());
            }
            Action::RemoveSnippet { node, name, .. } => {
                snippets.entry(format!("remove {name}")).or_default().push(node.clone());
            }
        }
    }
    let mut sections = Vec::new();
    if !account.is_empty() {
        sections.push(("Account and tokens".to_owned(), account));
    }
    for (title, group) in [("Roles", roles), ("Pools", pools)] {
        if group.is_empty() {
            continue;
        }
        let counts: Vec<String> = group
            .iter()
            .map(|(kind, names)| format!("{} {kind}", names.len()))
            .collect();
        let lines = group
            .iter()
            .flat_map(|(kind, names)| wrap_names(&format!("{kind}: "), names))
            .collect();
        sections.push((format!("{title} ({})", counts.join(", ")), lines));
    }
    if !gold.is_empty() {
        let heading = plan
            .gold
            .as_ref()
            .map_or_else(|| "Gold".to_owned(), |g| format!("Gold {g}"));
        sections.push((heading, gold));
    }
    if !members.is_empty() {
        sections.push(("Pool members".to_owned(), members));
    }
    if !permissions.is_empty() {
        let lines = permissions
            .iter()
            .flat_map(|(who, slot)| {
                let mut lines = vec![format!("{who}:")];
                for (heading, map) in [("", &slot.given), ("taken away from ", &slot.taken)] {
                    for ((_, kind), names) in map {
                        lines.extend(wrap_names(&format!("    {heading}{kind:<14} "), names));
                    }
                }
                lines
            })
            .collect();
        sections.push((format!("Permissions ({entries} entries)"), lines));
    }
    if !snippets.is_empty() {
        let lines = snippets
            .iter()
            .map(|(what, nodes)| format!("{what} on {}", nodes.join(", ")))
            .collect();
        sections.push(("Cloud-Init snippets".to_owned(), lines));
    }
    sections
}

/// The summary of a plan: its warnings and what changes.
pub fn summary_of(access_line: &str, plan: &Plan, home: &std::path::Path) -> Summary {
    let _ = access_line;
    let mut sections = Vec::new();
    let mut notices = plan.warnings.clone();
    notices.extend(plan.node_errors.iter().cloned());
    if !notices.is_empty() {
        sections.push(("Warnings (they do not stop the install)".to_owned(), notices));
    }
    if plan.steps.is_empty() {
        sections.push((
            "Changes in Proxmox".to_owned(),
            vec!["Nothing to change: HomeStack is installed as chosen.".to_owned()],
        ));
    } else {
        sections.extend(grouped_changes(plan, home));
    }
    Summary {
        sections,
        problems: Vec::new(),
    }
}

/// The confirmation's section of the servers, and their problems.
pub fn servers_section(summary: &mut Summary, servers: &ServerPlan) {
    if let Ok(config) = hs_model::config::Config::parse(&servers.server_toml) {
        summary.sections.push((
            "Workspace configuration".to_owned(),
            vec![
                format!(
                    "User: {} (UID {}, GID {})",
                    config.user.name, config.user.uid, config.user.gid
                ),
                format!(
                    "Network: {}.VMID/{}, gateway {}, DNS {}",
                    config.network.prefix,
                    config.network.cidr,
                    config.network.gateway,
                    config.network.dns.join(", ")
                ),
                format!(
                    "Root: {}; home: {}, default {}",
                    config.root_disk, config.home_disk, config.default_home_size
                ),
            ],
        ));
    }
    let mut lines = servers.lines.clone();
    lines.extend(servers.warnings.iter().map(|warning| format!("warning: {warning}")));
    summary.sections.push(("Servers".to_owned(), lines));
    summary.problems.extend(servers.problems.iter().cloned());
}

/// Where the choices of the form are kept between runs.
pub fn draft_path(settings: &Settings) -> std::path::PathBuf {
    settings.token_dir.join("install-draft.toml")
}

/// The kept choices, by option name without dashes; nothing when the file
/// is missing or cannot be read.
pub fn load_draft(path: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return std::collections::BTreeMap::new();
    };
    table
        .into_iter()
        .filter_map(|(key, value)| {
            let values: Vec<String> = value
                .as_array()?
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            Some((key, values))
        })
        .collect()
}

/// Start each question from the kept choices that are still possible.
pub fn apply_draft(questions: &mut [hs_install::Question], draft: &std::collections::BTreeMap<String, Vec<String>>) {
    for question in questions {
        let Some(values) = draft.get(question.option.trim_start_matches('-')) else {
            continue;
        };
        if crate::install_config::is_text(question) {
            if let (Some(choice), Some(value)) = (question.choices.first_mut(), values.first()) {
                choice.value.clone_from(value);
            }
            continue;
        }
        // Keep click order (the first storage is the layout default), and
        // distinguish an explicitly empty selection from no saved answer.
        question.defaults = values
            .iter()
            .filter_map(|value| question.choices.iter().position(|choice| &choice.value == value))
            .collect();
    }
}

/// `domain` of the `deploy.toml` at `path`; nothing when it is missing or
/// cannot be read.
fn deployed_domain(path: &std::path::Path) -> Option<String> {
    let table = std::fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()?;
    table
        .get("domain")
        .and_then(toml::Value::as_str)
        .filter(|domain| !domain.is_empty())
        .map(str::to_owned)
}

/// `snippet_storage` recorded by an earlier install in `token_dir`
/// (`server.toml`, `install.toml`, `config.toml`, or `install-draft.toml`).
#[must_use]
pub fn configured_snippet_storage(token_dir: &std::path::Path) -> Option<String> {
    for name in ["server.toml", "install.toml", "config.toml"] {
        let Ok(text) = std::fs::read_to_string(token_dir.join(name)) else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        if let Some(storage) = table
            .get("cloud_init")
            .and_then(toml::Value::as_table)
            .and_then(|cloud_init| cloud_init.get("snippet_storage"))
            .or_else(|| table.get("snippet_storage"))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|storage| !storage.is_empty())
        {
            return Some(storage.to_owned());
        }
    }
    load_draft(&token_dir.join("install-draft.toml"))
        .get("snippet-storage")
        .and_then(|values| values.first())
        .map(|storage| storage.trim().to_owned())
        .filter(|storage| !storage.is_empty())
}

/// Keep the choices of the form (mode 0600); a failure only loses them.
pub fn save_draft(
    path: &std::path::Path,
    questions: &[hs_install::Question],
    answers: &[Answer],
    domain: Option<&str>,
) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut table = toml::Table::new();
    for (question, answer) in questions.iter().zip(answers) {
        // What goes back in a pool is found anew each time.
        if question.option == hs_install::discover::OPTION {
            continue;
        }
        let values: Vec<toml::Value> = match answer {
            Answer::Picked(picked) => picked
                .iter()
                .filter_map(|index| question.choices.get(*index))
                .map(|choice| toml::Value::String(choice.value.clone()))
                .collect(),
            Answer::Text(text) => vec![toml::Value::String(text.clone())],
        };
        table.insert(
            question.option.trim_start_matches('-').to_owned(),
            toml::Value::Array(values),
        );
    }
    // A run without a domain (no servers, the test part, the field left
    // empty) keeps the one the draft had.
    let domain = domain
        .filter(|domain| !domain.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            load_draft(path)
                .get(DRAFT_DOMAIN)
                .and_then(|values| values.first())
                .filter(|domain| !domain.is_empty())
                .cloned()
        });
    if let Some(domain) = domain {
        table.insert(
            DRAFT_DOMAIN.to_owned(),
            toml::Value::Array(vec![toml::Value::String(domain)]),
        );
    }
    let text = format!("# The last choices of `homestack install`; the next run starts from them.\n{table}");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        let _ = file.write_all(text.as_bytes());
    }
}

/// `names` after `lead`, comma-separated, in lines of at most 80 characters.
fn wrap_names(lead: &str, names: &[String]) -> Vec<String> {
    let indent = " ".repeat(lead.chars().count());
    let mut lines = vec![lead.to_owned()];
    for (index, name) in names.iter().enumerate() {
        let piece = if index + 1 < names.len() {
            format!("{name}, ")
        } else {
            name.clone()
        };
        let current = lines.last().map_or(0, |line| line.chars().count());
        if current + piece.chars().count() > 80 && current > indent.len() {
            lines.push(indent.clone());
        }
        if let Some(line) = lines.last_mut() {
            line.push_str(&piece);
        }
    }
    lines.into_iter().map(|line| line.trim_end().to_owned()).collect()
}

/// What one account or token is given and loses, by kind of place.
#[derive(Default)]
struct Permissions {
    given: std::collections::BTreeMap<(u8, &'static str), Vec<String>>,
    taken: std::collections::BTreeMap<(u8, &'static str), Vec<String>>,
}
