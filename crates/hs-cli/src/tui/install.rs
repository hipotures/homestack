//! The install screen (phase 13b step 3; mockups 02–08, 18): the steps
//! access › check › settings › review › install (uninstall: access › check
//! › review › remove) in the top border, the buttons of the step in the
//! bottom border, ssh's questions in a box. What it shows comes from the
//! install worker (`installing`) as [`Event`]s; what it asks goes to it as
//! `installing::Command`s. No drawing, no I/O.

use crossterm::event::KeyCode;

use super::installing::Command;
use super::state::Request;
use crate::askpass::Kind;
use crate::install_flow::Answer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
    /// Pangolin Step 8: a Site connector per node (SSH root only).
    Sites,
    /// The first deployment of a manifest-defined service (deploy token).
    Service,
}

impl Mode {
    /// The steps in the top border.
    #[must_use]
    pub fn steps(self) -> &'static [&'static str] {
        match self {
            Self::Install => &["access", "check", "settings", "review", "install"],
            Self::Uninstall => &["access", "check", "review", "remove"],
            Self::Sites => &["access", "settings", "review", "deploy"],
            Self::Service => &["settings", "review", "deploy"],
        }
    }
}

/// How root is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Ssh,
    Herdr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Access,
    Check,
    Settings,
    Review,
    Run,
    Done,
}

/// A line of the check: `✓` fine, `·` the install adds it, `!` to note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ok,
    Add,
    Note,
}

impl Mark {
    #[must_use]
    pub fn symbol(self) -> char {
        match self {
            Self::Ok => '✓',
            Self::Add => '·',
            Self::Note => '!',
        }
    }
}

/// Checks a typed value; the error says what is expected.
pub type Check = fn(&str) -> Result<(), String>;

/// One setting and its choices (`value`, what tells it apart).
#[derive(Debug, Clone)]
pub struct Setting {
    pub title: String,
    pub note: String,
    pub multiple: bool,
    pub choices: Vec<(String, String)>,
    pub picked: Vec<usize>,
    pub text: Option<String>,
    /// An editable list: each choice is an entry; in its box `-` removes
    /// the highlighted one and `+` adds one typed after it.
    pub list: bool,
    /// Checks a typed value (or a list's new entry) when Enter keeps it:
    /// an error keeps the box open and is shown in it.
    pub check: Option<Check>,
}

impl Setting {
    /// The value in the list: what is chosen, a VMID with its name.
    #[must_use]
    pub fn value(&self) -> String {
        if let Some(text) = &self.text {
            return text.clone();
        }
        if self.list {
            return match self.choices.len() {
                0 => "—".to_owned(),
                1 => "1 entry".to_owned(),
                count => format!("{count} entries"),
            };
        }
        // A VMID with the VM's name (the detail's first word), a storage as
        // it is.
        let shown: Vec<String> = self
            .picked
            .iter()
            .take(if self.multiple { usize::MAX } else { 1 })
            .filter_map(|index| self.choices.get(*index))
            .map(|(value, detail)| {
                if value.chars().all(|c| c.is_ascii_digit()) {
                    let first = detail.split_whitespace().next().unwrap_or_default();
                    format!("{value} {first}").trim_end().to_owned()
                } else {
                    value.clone()
                }
            })
            .collect();
        match shown.len() {
            0 => "—".to_owned(),
            1 => shown.join(", "),
            count => format!("{count} · {}", shown.join(", ")),
        }
    }

    fn toggle(&mut self, index: usize) {
        if index >= self.choices.len() {
            return;
        }
        if self.multiple {
            if let Some(at) = self.picked.iter().position(|picked| *picked == index) {
                self.picked.remove(at);
            } else {
                self.picked.push(index);
            }
        } else {
            self.picked = vec![index];
        }
    }
}

/// A setting as it was when its box opened.
#[derive(Debug, Clone)]
pub enum Edited {
    Setting(Setting),
    Domain(String),
}

/// What the worker found once root was reached.
#[derive(Debug, Clone)]
pub struct Found {
    /// How root is reached and the Proxmox version, for the top border.
    pub root: String,
    pub check: Vec<(Mark, String, String)>,
    pub settings: Vec<Setting>,
    /// The server domain (the last run's); `None` when no servers are made.
    pub domain: Option<String>,
}

/// What the worker tells the screen.
#[derive(Debug, Clone)]
pub enum Event {
    Connected(Found),
    /// Root could not be reached, or the cluster read.
    Failed(String),
    Reviewed {
        sections: Vec<(String, Vec<String>)>,
        problems: Vec<String>,
    },
    /// What is being waited for now (connected, reading the cluster, …).
    Busy(String),
    /// One more thing done in the run.
    Line(String),
    Done {
        ok: bool,
        lines: Vec<String>,
    },
}

/// ssh's question in the box over the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskView {
    pub id: u64,
    pub kind: Kind,
    pub prompt: String,
    /// What is typed (a PIN), shown as dots.
    pub input: String,
    /// Seconds since it was asked.
    pub seconds: u64,
    /// The touch: the names of the key in `~/.ssh` (its comment).
    pub key_names: Vec<String>,
}

impl AskView {
    #[must_use]
    pub fn new(id: u64, kind: Kind, prompt: &str) -> Self {
        Self {
            id,
            kind,
            prompt: prompt.to_owned(),
            input: String::new(),
            seconds: 0,
            key_names: Vec::new(),
        }
    }

    /// A key on the box: the answer when it is given.
    pub fn key(&mut self, code: KeyCode) -> Option<Option<String>> {
        match (self.kind, code) {
            (Kind::Secret, KeyCode::Char(c)) => self.input.push(c),
            (Kind::Secret, KeyCode::Backspace) => {
                self.input.pop();
            }
            (Kind::Secret, KeyCode::Enter) => return Some(Some(self.input.clone())),
            (Kind::Secret, KeyCode::Esc) => return Some(None),
            (Kind::HostKey, KeyCode::Enter | KeyCode::Char('t')) => return Some(Some("yes".to_owned())),
            (Kind::HostKey, KeyCode::Esc) => return Some(Some("no".to_owned())),
            // Cancelled, ssh is stopped (`askpass::Question::answer`).
            (Kind::Touch, KeyCode::Esc) => return Some(None),
            _ => {}
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Install {
    pub mode: Mode,
    pub stage: Stage,
    /// Opened because there is no `client.toml`: Esc quits.
    pub first_start: bool,
    pub method: Method,
    pub host: String,
    /// Where typing goes in `host`, in characters.
    pub caret: usize,
    /// Herdr tabs: name and title.
    pub tabs: Vec<(String, String)>,
    pub tab: usize,
    /// Case-insensitive substring filter for Herdr `WORKSPACE/TAB` names (defaults to `pve`).
    pub tab_filter: String,
    /// Where typing goes in `tab_filter`, in characters.
    pub tab_caret: usize,
    /// Whether the user has edited `tab_filter` in this session.
    pub tab_filter_edited: bool,
    /// Access: 2 the method (first), 0 the host (or tab).
    pub field: usize,
    /// What is being waited for.
    pub busy: Option<String>,
    pub error: Option<String>,
    /// The connection failed: its box, closed by Enter or Esc.
    pub failed: Option<String>,
    /// Esc while connecting: ssh is being stopped.
    pub cancelling: bool,
    /// The spinner's frame.
    pub frame: usize,
    pub found: Option<Found>,
    pub settings: Vec<Setting>,
    pub domain: Option<String>,
    pub row: usize,
    pub chooser: bool,
    /// The value the open box started from, restored by Esc.
    pub before: Option<Edited>,
    /// The review is of removing the selected instances (Del).
    pub removing: bool,
    /// Why the value in the open box was not kept.
    pub invalid: Option<String>,
    pub cursor: usize,
    /// The entry typed after `+` in a list's box.
    pub adding: Option<String>,
    pub sections: Vec<(String, Vec<String>)>,
    pub problems: Vec<String>,
    pub reviewed: bool,
    pub scroll: usize,
    /// The settings' help box is open.
    pub help: bool,
    /// Uninstall review: Remove is focused (Cancel otherwise).
    pub remove_focused: bool,
    /// The run, oldest first.
    pub lines: Vec<String>,
    pub ok: bool,
    pub result: Vec<String>,
}

impl Install {
    pub fn title(&self) -> &'static str {
        match self.mode {
            Mode::Install if self.first_start => "HomeStack install: first setup",
            Mode::Install => "HomeStack reinstall",
            Mode::Uninstall => "HomeStack uninstall",
            Mode::Sites => "Pangolin Site connectors",
            Mode::Service => "Service",
        }
    }
    #[must_use]
    pub fn new(mode: Mode, first_start: bool) -> Self {
        let tab_filter = crate::root_herdr::DEFAULT_TAB_FILTER.to_owned();
        let tab_caret = tab_filter.chars().count();
        Self {
            mode,
            stage: Stage::Access,
            first_start,
            method: Method::Ssh,
            host: String::new(),
            caret: 0,
            tabs: Vec::new(),
            tab: 0,
            tab_filter,
            tab_caret,
            tab_filter_edited: false,
            field: 2,
            busy: None,
            error: None,
            failed: None,
            cancelling: false,
            frame: 0,
            found: None,
            settings: Vec::new(),
            domain: None,
            row: 0,
            chooser: false,
            before: None,
            removing: false,
            invalid: None,
            adding: None,
            cursor: 0,
            sections: Vec::new(),
            problems: Vec::new(),
            reviewed: false,
            scroll: 0,
            remove_focused: false,
            help: false,
            lines: Vec::new(),
            ok: false,
            result: Vec::new(),
        }
    }

    /// The step shown raised, and how many there are.
    #[must_use]
    pub fn step(&self) -> (usize, usize) {
        let steps = self.mode.steps().len();
        let at = match (self.stage, self.mode) {
            (Stage::Access | Stage::Check | Stage::Settings, Mode::Service) => 0,
            (Stage::Review, Mode::Service) => 1,
            (Stage::Access, _) => 0,
            (Stage::Check, _) => 1,
            (Stage::Settings, Mode::Sites) => 1,
            (Stage::Review, Mode::Sites) => 2,
            (Stage::Settings, _) => 2,
            (Stage::Review, Mode::Install) => 3,
            (Stage::Review, Mode::Uninstall) => 2,
            (Stage::Run | Stage::Done, _) => steps - 1,
        };
        (at, steps)
    }

    /// The rows of the settings: each setting, then the server domain.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.settings.len() + usize::from(self.domain.is_some())
    }

    /// The Herdr tabs matching `tab_filter`.
    #[must_use]
    pub fn matching_tabs(&self) -> Vec<&(String, String)> {
        self.tabs
            .iter()
            .filter(|(name, _)| crate::root_herdr::matches_tab(name, &self.tab_filter))
            .collect()
    }

    /// The currently selected Herdr tab among `matching_tabs`.
    #[must_use]
    pub fn selected_tab(&self) -> Option<&(String, String)> {
        self.matching_tabs().get(self.tab).copied()
    }

    /// Populate available Herdr tabs and select `kept` if present.
    pub fn set_tabs(&mut self, tabs: Vec<(String, String)>, kept: Option<&str>) {
        self.tabs = tabs;
        if let Some(kept) = kept
            && self.tabs.iter().any(|(name, _)| name == kept)
            && !crate::root_herdr::matches_tab(kept, &self.tab_filter)
        {
            self.tab_filter.clear();
            self.tab_caret = 0;
        } else if !self.tab_filter_edited
            && !self.tab_filter.is_empty()
            && !self.tabs.is_empty()
            && self.matching_tabs().is_empty()
        {
            self.tab_filter.clear();
            self.tab_caret = 0;
        }
        self.tab = kept
            .and_then(|kept| self.matching_tabs().iter().position(|(name, _)| name == kept))
            .unwrap_or(0);
    }

    /// The access target: the host, or the chosen Herdr tab.
    fn target(&self) -> Option<String> {
        match self.method {
            Method::Ssh => {
                let host = self.host.trim();
                (!host.is_empty()).then(|| host.to_owned())
            }
            Method::Herdr => self.selected_tab().map(|(name, _)| name.clone()),
        }
    }

    /// Connecting: the box with the spinner over the access, Esc cancels.
    #[must_use]
    pub fn connecting(&self) -> bool {
        self.stage == Stage::Access && self.busy.is_some()
    }

    /// Esc while connecting: the loop stops ssh; the worker's last word
    /// (failed, or connected after all) brings back the access.
    fn cancel(&mut self) -> Option<Request> {
        if self.cancelling {
            return None;
        }
        self.cancelling = true;
        self.busy = Some("Cancelling".to_owned());
        Some(Request::Install(Command::Cancel))
    }

    pub fn event(&mut self, event: Event) {
        if self.cancelling {
            match event {
                Event::Busy(_) | Event::Line(_) | Event::Reviewed { .. } => {}
                Event::Connected(_) | Event::Failed(_) | Event::Done { .. } => {
                    self.cancelling = false;
                    self.busy = None;
                    self.found = None;
                    self.stage = Stage::Access;
                    self.error = Some("Connection cancelled.".to_owned());
                }
            }
            return;
        }
        self.busy = None;
        match event {
            Event::Connected(found) => {
                self.error = None;
                self.settings.clone_from(&found.settings);
                self.domain.clone_from(&found.domain);
                // A rescan may offer a row less.
                self.row = self.row.min(self.rows().saturating_sub(1));
                self.found = Some(found);
                // The install goes straight to its settings (the check stays
                // in the log and one Esc back); the uninstall shows the check.
                self.stage = match self.mode {
                    Mode::Install | Mode::Sites | Mode::Service => Stage::Settings,
                    Mode::Uninstall => Stage::Check,
                };
            }
            Event::Failed(error) => {
                // In its box, as the setup's failure.
                self.error = None;
                self.failed = Some(error);
                if self.stage != Stage::Access && self.mode != Mode::Service {
                    self.stage = Stage::Access;
                }
            }
            Event::Reviewed { sections, problems } => {
                self.sections = sections;
                self.problems = problems;
                self.reviewed = true;
                self.scroll = 0;
            }
            Event::Busy(text) => self.busy = Some(text),
            Event::Line(line) => self.lines.push(line),
            Event::Done { ok, lines } => {
                self.ok = ok;
                self.result = lines;
                self.stage = Stage::Done;
            }
        }
    }

    /// A key; whether the screen closes, and what the loop does.
    pub fn key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        if self.failed.is_some() {
            if matches!(code, KeyCode::Enter | KeyCode::Esc) {
                self.failed = None;
                // Nothing was read: the service screen has nothing to show.
                if self.mode == Mode::Service && self.found.is_none() {
                    return (true, Some(Request::Install(Command::Cancel)));
                }
            }
            return (false, None);
        }
        if self.mode == Mode::Service && self.found.is_none() && code == KeyCode::Esc {
            return (true, Some(Request::Install(Command::Cancel)));
        }
        if self.connecting() {
            return (false, if code == KeyCode::Esc { self.cancel() } else { None });
        }
        if self.busy.is_some() && self.stage != Stage::Run {
            return (false, None);
        }
        match self.stage {
            Stage::Access => self.access_key(code),
            Stage::Check => match code {
                KeyCode::Enter | KeyCode::Char('s' | 'r') => (false, self.after_check()),
                // Back (b or Esc) lets root go and starts at access again.
                KeyCode::Char('b') | KeyCode::Esc => {
                    self.stage = Stage::Access;
                    self.found = None;
                    (false, Some(Request::Install(Command::Cancel)))
                }
                _ => (false, None),
            },
            Stage::Settings => self.settings_key(code),
            Stage::Review => self.review_key(code),
            Stage::Run => (false, None),
            Stage::Done => match code {
                KeyCode::Enter | KeyCode::Esc if self.mode == Mode::Service && self.ok => {
                    (true, Some(Request::Install(Command::Cancel)))
                }
                // A stopped deployment is reviewed again: the registry now
                // holds what it made.
                KeyCode::Enter | KeyCode::Esc if self.mode == Mode::Service => {
                    self.stage = Stage::Settings;
                    self.removing = false;
                    self.lines.clear();
                    (false, None)
                }
                // Back to the Pangolin wizard; root is let go.
                KeyCode::Enter | KeyCode::Esc if self.ok && self.mode == Mode::Sites => {
                    (true, Some(Request::Install(Command::Cancel)))
                }
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('o') if self.ok => (true, Some(Request::Installed)),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('b') => {
                    self.stage = Stage::Review;
                    self.lines.clear();
                    (false, None)
                }
                KeyCode::Char('q') => (true, Some(Request::Quit)),
                _ => (false, None),
            },
        }
    }

    fn access_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        match code {
            KeyCode::Esc => {
                if self.first_start {
                    return (true, Some(Request::Quit));
                }
                return (true, None);
            }
            // The host (or tab) and the method.
            KeyCode::Tab | KeyCode::BackTab => self.field = if self.field == 0 { 2 } else { 0 },
            // Method is the row above the host/pane.
            KeyCode::Up => {
                if self.method == Method::Herdr && self.field == 0 && self.tab > 0 {
                    self.tab -= 1;
                } else {
                    self.field = 2;
                }
            }
            KeyCode::Down => {
                if self.method == Method::Herdr && self.field == 0 {
                    let last = self.matching_tabs().len().saturating_sub(1);
                    self.tab = (self.tab + 1).min(last);
                } else {
                    self.field = 0;
                }
            }
            KeyCode::Enter => {
                let Some(target) = self.target() else {
                    self.error = Some(match self.method {
                        Method::Ssh => "Give the node as USER@HOST, for example root@pve1.".to_owned(),
                        Method::Herdr => "No Herdr tab to use: open one with a root shell on a node.".to_owned(),
                    });
                    return (false, None);
                };
                self.error = None;
                self.busy = Some(format!("Connecting to {target}"));
                return (
                    false,
                    Some(Request::Install(Command::Connect {
                        method: self.method,
                        target,
                    })),
                );
            }
            // Site credentials travel on SSH's standard input: no Herdr pane.
            KeyCode::Left | KeyCode::Right if self.field == 2 && self.mode == Mode::Sites => {}
            KeyCode::Left | KeyCode::Right if self.field == 2 => {
                self.method = match self.method {
                    Method::Ssh => Method::Herdr,
                    Method::Herdr => Method::Ssh,
                };
                if self.method == Method::Herdr {
                    return (false, Some(Request::HerdrTabs));
                }
            }
            KeyCode::Left | KeyCode::Right if self.method == Method::Herdr && self.field == 0 => {
                let count = self.matching_tabs().len().max(1);
                self.tab = if matches!(code, KeyCode::Right | KeyCode::Down) {
                    (self.tab + 1) % count
                } else {
                    (self.tab + count - 1) % count
                };
            }
            KeyCode::Char(c) if self.method == Method::Herdr => {
                self.field = 0;
                self.tab_filter_edited = true;
                let at = Self::char_byte_at(&self.tab_filter, self.tab_caret);
                self.tab_filter.insert(at, c);
                self.tab_caret += 1;
                self.tab = 0;
            }
            KeyCode::Backspace if self.method == Method::Herdr => {
                self.field = 0;
                self.tab_filter_edited = true;
                if self.tab_caret > 0 {
                    self.tab_caret -= 1;
                    let at = Self::char_byte_at(&self.tab_filter, self.tab_caret);
                    self.tab_filter.remove(at);
                    self.tab = 0;
                }
            }
            KeyCode::Delete if self.method == Method::Herdr => {
                self.field = 0;
                self.tab_filter_edited = true;
                if self.tab_caret < self.tab_filter.chars().count() {
                    let at = Self::char_byte_at(&self.tab_filter, self.tab_caret);
                    self.tab_filter.remove(at);
                    self.tab = 0;
                }
            }
            KeyCode::Home if self.method == Method::Herdr && self.field == 0 => {
                self.tab_caret = 0;
            }
            KeyCode::End if self.method == Method::Herdr && self.field == 0 => {
                self.tab_caret = self.tab_filter.chars().count();
            }
            KeyCode::Char(c) if self.field == 0 && self.method == Method::Ssh => {
                let at = self.byte_at(self.caret);
                self.host.insert(at, c);
                self.caret += 1;
            }
            KeyCode::Backspace if self.field == 0 && self.method == Method::Ssh => {
                if self.caret > 0 {
                    self.caret -= 1;
                    let at = self.byte_at(self.caret);
                    self.host.remove(at);
                }
            }
            KeyCode::Delete if self.field == 0 && self.method == Method::Ssh => {
                if self.caret < self.host.chars().count() {
                    let at = self.byte_at(self.caret);
                    self.host.remove(at);
                }
            }
            KeyCode::Left if self.field == 0 && self.method == Method::Ssh => {
                self.caret = self.caret.saturating_sub(1);
            }
            KeyCode::Right if self.field == 0 && self.method == Method::Ssh => {
                self.caret = (self.caret + 1).min(self.host.chars().count());
            }
            KeyCode::Home if self.field == 0 => self.caret = 0,
            KeyCode::End if self.field == 0 => self.caret = self.host.chars().count(),
            _ => {}
        }
        (false, None)
    }

    /// The byte index of character `index` of `text`.
    fn char_byte_at(text: &str, index: usize) -> usize {
        text.char_indices().nth(index).map_or(text.len(), |(at, _)| at)
    }

    /// The byte index of character `index` of the host.
    fn byte_at(&self, index: usize) -> usize {
        Self::char_byte_at(&self.host, index)
    }

    fn after_check(&mut self) -> Option<Request> {
        match self.mode {
            Mode::Install => {
                self.stage = Stage::Settings;
                None
            }
            Mode::Uninstall | Mode::Sites | Mode::Service => {
                self.stage = Stage::Review;
                self.reviewed = false;
                self.remove_focused = false;
                self.busy = Some("Reading what would be removed".to_owned());
                Some(Request::Install(Command::Review {
                    answers: Vec::new(),
                    domain: None,
                }))
            }
        }
    }

    /// The settings as answers, in their order.
    fn answers(&self) -> Vec<Answer> {
        self.settings
            .iter()
            .map(|setting| {
                if setting.list {
                    let entries: Vec<&str> = setting.choices.iter().map(|(entry, _)| entry.as_str()).collect();
                    return Answer::Text(entries.join(" "));
                }
                setting.text.as_ref().map_or_else(
                    || Answer::Picked(setting.picked.clone()),
                    |text| Answer::Text(text.clone()),
                )
            })
            .collect()
    }

    /// Del removes ticked instances: a service deployment with instances.
    #[must_use]
    pub fn removable(&self) -> bool {
        self.mode == Mode::Service && self.settings.iter().any(|setting| setting.title == "Instances")
    }

    /// Whether the highlighted row is the server domain.
    fn on_domain(&self) -> bool {
        self.domain.is_some() && self.row == self.settings.len()
    }

    /// Enter the highlighted setting's box; Esc there restores this value.
    pub fn begin_edit(&mut self) {
        if self.chooser {
            return;
        }
        self.chooser = true;
        self.adding = None;
        self.before = if self.on_domain() {
            self.domain.clone().map(Edited::Domain)
        } else {
            self.settings.get(self.row).cloned().map(Edited::Setting)
        };
        self.cursor = self
            .settings
            .get(self.row)
            .and_then(|setting| setting.picked.first().copied())
            .unwrap_or(0);
    }

    /// Leave the box: keep the value, or restore the one it had.
    fn end_edit(&mut self, keep: bool) {
        self.chooser = false;
        self.invalid = None;
        self.adding = None;
        match self.before.take() {
            Some(Edited::Setting(before)) if !keep => {
                if let Some(setting) = self.settings.get_mut(self.row) {
                    *setting = before;
                }
            }
            Some(Edited::Domain(before)) if !keep => self.domain = Some(before),
            _ => {}
        }
    }

    /// Why the open text box's value cannot be kept.
    fn value_error(&self) -> Option<String> {
        let setting = self.settings.get(self.row).filter(|_| !self.on_domain())?;
        let text = setting.text.as_ref()?;
        setting.check.and_then(|check| check(text).err())
    }

    /// Plan the answers: the review.
    fn review(&mut self) -> (bool, Option<Request>) {
        // A missing or wrong value is fixed in its box first.
        let wrong = self.settings.iter().enumerate().find_map(|(row, setting)| {
            let text = setting.text.as_ref()?;
            setting
                .check
                .and_then(|check| check(text).err())
                .map(|error| (row, error))
        });
        if let Some((row, error)) = wrong {
            self.row = row;
            self.begin_edit();
            self.invalid = Some(error);
            return (false, None);
        }
        self.stage = Stage::Review;
        self.reviewed = false;
        self.busy = Some("Reading the plan".to_owned());
        (
            false,
            Some(Request::Install(Command::Review {
                answers: self.answers(),
                domain: self.domain.clone(),
            })),
        )
    }

    /// The settings: ↑↓ a setting, Enter into its box, Tab the next one, R
    /// the review. In a box Enter keeps the value and Esc restores it.
    fn settings_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        // ? opens the help; any key closes it.
        if self.help {
            self.help = false;
            return (false, None);
        }
        if self.chooser {
            return self.edit_key(code);
        }
        let last = self.rows().saturating_sub(1);
        match code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => self.row = (self.row + 1).min(last),
            KeyCode::BackTab => self.row = self.row.saturating_sub(1),
            KeyCode::Enter => self.begin_edit(),
            KeyCode::Char('r' | 'R') => return self.review(),
            KeyCode::Delete if self.removable() => {
                self.stage = Stage::Review;
                self.reviewed = false;
                self.removing = true;
                self.busy = Some("Reading what would be removed".to_owned());
                return (
                    false,
                    Some(Request::Install(Command::Remove {
                        answers: self.answers(),
                    })),
                );
            }
            // Rescan cluster: what changed in Proxmox since the connection
            // (a VM taken out of its pool) is offered, the choices kept.
            KeyCode::F(5) => {
                self.busy = Some("Rescanning the cluster".to_owned());
                return (
                    false,
                    Some(Request::Install(Command::Rescan {
                        answers: self.answers(),
                        domain: self.domain.clone(),
                    })),
                );
            }
            KeyCode::Esc if self.mode == Mode::Sites => {
                self.stage = Stage::Access;
                self.found = None;
                return (false, Some(Request::Install(Command::Cancel)));
            }
            KeyCode::Esc if self.mode == Mode::Service => {
                return (true, Some(Request::Install(Command::Cancel)));
            }
            KeyCode::Esc => self.stage = Stage::Check,
            _ => {}
        }
        (false, None)
    }

    /// A key in the highlighted setting's box.
    fn edit_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        match code {
            KeyCode::Esc if self.adding.is_some() => {
                self.adding = None;
                return (false, None);
            }
            KeyCode::Esc => {
                self.end_edit(false);
                return (false, None);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if let Some(error) = self.value_error() {
                    self.invalid = Some(error);
                    return (false, None);
                }
                self.end_edit(true);
                let last = self.rows().saturating_sub(1);
                self.row = if code == KeyCode::Tab {
                    (self.row + 1).min(last)
                } else {
                    self.row.saturating_sub(1)
                };
                return (false, None);
            }
            _ => {}
        }
        if self.on_domain() {
            let domain = self.domain.get_or_insert_with(String::new);
            match code {
                KeyCode::Char(c) => domain.push(c),
                KeyCode::Backspace => {
                    domain.pop();
                }
                KeyCode::Enter => self.end_edit(true),
                _ => {}
            }
            return (false, None);
        }
        let cursor = self.cursor;
        let Some(setting) = self.settings.get_mut(self.row) else {
            self.end_edit(true);
            return (false, None);
        };
        if let Some(typed) = self.adding.as_mut() {
            match code {
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Enter => {
                    let entry = typed.trim().to_owned();
                    if let Some(error) = setting.check.and_then(|check| check(&entry).err()) {
                        self.invalid = Some(error);
                        return (false, None);
                    }
                    self.adding = None;
                    self.invalid = None;
                    if !entry.is_empty() {
                        setting.choices.push((entry, String::new()));
                        self.cursor = setting.choices.len() - 1;
                    }
                }
                _ => {}
            }
            return (false, None);
        }
        if let Some(text) = setting.text.as_mut() {
            match code {
                KeyCode::Char(c) => {
                    text.push(c);
                    self.invalid = None;
                }
                KeyCode::Backspace => {
                    text.pop();
                    self.invalid = None;
                }
                KeyCode::Enter => match setting.check.and_then(|check| check(text).err()) {
                    Some(error) => self.invalid = Some(error),
                    None => self.end_edit(true),
                },
                _ => {}
            }
            return (false, None);
        }
        let count = setting.choices.len().max(1);
        match code {
            KeyCode::Down => self.cursor = (cursor + 1).min(count - 1),
            KeyCode::Up => self.cursor = cursor.saturating_sub(1),
            KeyCode::Char('+') if setting.list => self.adding = Some(String::new()),
            KeyCode::Char('-') if setting.list && cursor < setting.choices.len() => {
                setting.choices.remove(cursor);
                self.cursor = cursor.min(setting.choices.len().saturating_sub(1));
            }
            KeyCode::Char(' ') if !setting.list => setting.toggle(cursor),
            // A single choice is the one under the cursor.
            KeyCode::Enter => {
                if !setting.list && !setting.multiple {
                    setting.toggle(cursor);
                }
                self.end_edit(true);
            }
            _ => {}
        }
        (false, None)
    }

    /// The review: a letter applies it (D deploy, I install), never Enter;
    /// a removal is confirmed with Del again.
    fn review_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        let ready = self.reviewed && self.problems.is_empty();
        let apply = |install: &mut Self| {
            install.stage = Stage::Run;
            install.lines.clear();
            (false, Some(Request::Install(Command::Apply)))
        };
        match (self.mode, code) {
            (Mode::Install | Mode::Sites | Mode::Service, KeyCode::Esc) => {
                self.stage = Stage::Settings;
                self.removing = false;
            }
            (Mode::Uninstall, KeyCode::Esc) => self.stage = Stage::Check,
            (_, KeyCode::Down) => self.scroll += 1,
            (_, KeyCode::Up) => self.scroll = self.scroll.saturating_sub(1),
            (Mode::Service, KeyCode::Delete) if self.removing && ready => return apply(self),
            (Mode::Service | Mode::Sites, KeyCode::Char('d' | 'D')) if !self.removing && ready => return apply(self),
            (Mode::Install, KeyCode::Char('i' | 'I')) if ready => return apply(self),
            (Mode::Uninstall, KeyCode::Left | KeyCode::Right | KeyCode::Tab) => {
                self.remove_focused = !self.remove_focused;
            }
            (Mode::Uninstall, KeyCode::Enter) if self.remove_focused && ready => return apply(self),
            (Mode::Uninstall, KeyCode::Enter) if !self.remove_focused => {
                return (true, Some(Request::Install(Command::Cancel)));
            }
            (Mode::Uninstall, KeyCode::Char('b')) => self.stage = Stage::Check,
            _ => {}
        }
        (false, None)
    }
}
