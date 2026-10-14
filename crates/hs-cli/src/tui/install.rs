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
use crate::wizard::Answer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
}

impl Mode {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Install => "HomeStack install",
            Self::Uninstall => "HomeStack uninstall",
        }
    }

    /// The steps in the top border.
    #[must_use]
    pub fn steps(self) -> &'static [&'static str] {
        match self {
            Self::Install => &["access", "check", "settings", "review", "install"],
            Self::Uninstall => &["access", "check", "review", "remove"],
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

/// One setting and its choices (`value`, what tells it apart).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub title: String,
    pub note: String,
    pub multiple: bool,
    pub choices: Vec<(String, String)>,
    pub picked: Vec<usize>,
}

impl Setting {
    /// The value in the list: `2 chosen`, or the one chosen.
    #[must_use]
    pub fn value(&self) -> String {
        if self.multiple {
            return format!("{} chosen", self.picked.len());
        }
        self.picked
            .first()
            .and_then(|index| self.choices.get(*index))
            .map(|(value, detail)| {
                let first = detail.split_whitespace().next().unwrap_or_default();
                format!("{value} {first}").trim_end().to_owned()
            })
            .unwrap_or_else(|| "—".to_owned())
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
                self.picked.sort_unstable();
            }
        } else {
            self.picked = vec![index];
        }
    }
}

/// What the worker found once root was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// How root is reached and the Proxmox version, for the top border.
    pub root: String,
    pub check: Vec<(Mark, String, String)>,
    pub settings: Vec<Setting>,
    /// The server domain (the last run's); `None` when no servers are made.
    pub domain: Option<String>,
}

/// What the worker tells the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            _ => {}
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Access: 0 the host (or tab), 2 the method.
    pub field: usize,
    /// What is being waited for.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub found: Option<Found>,
    pub settings: Vec<Setting>,
    pub domain: Option<String>,
    pub row: usize,
    pub chooser: bool,
    pub cursor: usize,
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
    #[must_use]
    pub fn new(mode: Mode, first_start: bool) -> Self {
        Self {
            mode,
            stage: Stage::Access,
            first_start,
            method: Method::Ssh,
            host: String::new(),
            caret: 0,
            tabs: Vec::new(),
            tab: 0,
            field: 0,
            busy: None,
            error: None,
            found: None,
            settings: Vec::new(),
            domain: None,
            row: 0,
            chooser: false,
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
            (Stage::Access, _) => 0,
            (Stage::Check, _) => 1,
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

    /// The access target: the host, or the chosen Herdr tab.
    fn target(&self) -> Option<String> {
        match self.method {
            Method::Ssh => {
                let host = self.host.trim();
                (!host.is_empty()).then(|| host.to_owned())
            }
            Method::Herdr => self.tabs.get(self.tab).map(|(name, _)| name.clone()),
        }
    }

    pub fn event(&mut self, event: Event) {
        self.busy = None;
        match event {
            Event::Connected(found) => {
                self.error = None;
                self.settings.clone_from(&found.settings);
                self.domain.clone_from(&found.domain);
                self.found = Some(found);
                // The install goes straight to its settings (the check stays
                // in the log and one Esc back); the uninstall shows the check.
                self.stage = match self.mode {
                    Mode::Install => Stage::Settings,
                    Mode::Uninstall => Stage::Check,
                };
            }
            Event::Failed(error) => {
                self.error = Some(error);
                if self.stage != Stage::Access {
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
            // Method is the row above the host.
            KeyCode::Up => self.field = 2,
            KeyCode::Down => self.field = 0,
            KeyCode::Enter => {
                let Some(target) = self.target() else {
                    self.error = Some(match self.method {
                        Method::Ssh => "Give the node as USER@HOST, for example root@pve1.".to_owned(),
                        Method::Herdr => "No Herdr tab to use: open one with a root shell on a node.".to_owned(),
                    });
                    return (false, None);
                };
                self.error = None;
                self.busy = Some(format!("Connecting to {target}…"));
                return (
                    false,
                    Some(Request::Install(Command::Connect {
                        method: self.method,
                        target,
                    })),
                );
            }
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
                let count = self.tabs.len().max(1);
                self.tab = if matches!(code, KeyCode::Right | KeyCode::Down) {
                    (self.tab + 1) % count
                } else {
                    (self.tab + count - 1) % count
                };
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

    /// The byte index of character `index` of the host.
    fn byte_at(&self, index: usize) -> usize {
        self.host
            .char_indices()
            .nth(index)
            .map_or(self.host.len(), |(at, _)| at)
    }

    fn after_check(&mut self) -> Option<Request> {
        match self.mode {
            Mode::Install => {
                self.stage = Stage::Settings;
                None
            }
            Mode::Uninstall => {
                self.stage = Stage::Review;
                self.reviewed = false;
                self.remove_focused = false;
                self.busy = Some("Reading what would be removed…".to_owned());
                Some(Request::Install(Command::Review {
                    answers: Vec::new(),
                    domain: None,
                }))
            }
        }
    }

    fn settings_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        // ? opens the help (a domain has no ?); any key closes it.
        if self.help {
            self.help = false;
            return (false, None);
        }
        if code == KeyCode::Char('?') {
            self.help = true;
            return (false, None);
        }
        let on_domain = self.domain.is_some() && self.row == self.settings.len();
        match code {
            KeyCode::Esc if !self.chooser => self.stage = Stage::Check,
            KeyCode::Esc => self.chooser = false,
            KeyCode::Char('r') if !on_domain => return self.settings_key(KeyCode::Enter),
            KeyCode::Enter => {
                self.stage = Stage::Review;
                self.reviewed = false;
                self.busy = Some("Reading the plan…".to_owned());
                let answers = self
                    .settings
                    .iter()
                    .map(|setting| Answer::Picked(setting.picked.clone()))
                    .collect();
                return (
                    false,
                    Some(Request::Install(Command::Review {
                        answers,
                        domain: self.domain.clone(),
                    })),
                );
            }
            KeyCode::Tab | KeyCode::BackTab if !on_domain => {
                self.chooser = !self.chooser;
                self.cursor = self
                    .settings
                    .get(self.row)
                    .and_then(|setting| setting.picked.first().copied())
                    .unwrap_or(0);
            }
            KeyCode::Down | KeyCode::Up if self.chooser => {
                let count = self
                    .settings
                    .get(self.row)
                    .map_or(1, |setting| setting.choices.len().max(1));
                self.cursor = if code == KeyCode::Down {
                    (self.cursor + 1).min(count - 1)
                } else {
                    self.cursor.saturating_sub(1)
                };
            }
            KeyCode::Char(' ') if self.chooser => {
                let cursor = self.cursor;
                if let Some(setting) = self.settings.get_mut(self.row) {
                    setting.toggle(cursor);
                }
            }
            KeyCode::Down => self.row = (self.row + 1).min(self.rows().saturating_sub(1)),
            KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Char(c) if on_domain => {
                if let Some(domain) = self.domain.as_mut() {
                    domain.push(c);
                }
            }
            KeyCode::Backspace if on_domain => {
                if let Some(domain) = self.domain.as_mut() {
                    domain.pop();
                }
            }
            KeyCode::Char('b') if !self.chooser => self.stage = Stage::Check,
            _ => {}
        }
        (false, None)
    }

    fn review_key(&mut self, code: KeyCode) -> (bool, Option<Request>) {
        let ready = self.reviewed && self.problems.is_empty();
        match (self.mode, code) {
            (Mode::Install, KeyCode::Esc) => self.stage = Stage::Settings,
            (Mode::Uninstall, KeyCode::Esc) => self.stage = Stage::Check,
            (_, KeyCode::Down) => self.scroll += 1,
            (_, KeyCode::Up) => self.scroll = self.scroll.saturating_sub(1),
            (Mode::Install, KeyCode::Enter | KeyCode::Char('i')) if ready => {
                self.stage = Stage::Run;
                self.lines.clear();
                return (false, Some(Request::Install(Command::Apply)));
            }
            (Mode::Install, KeyCode::Char('b')) => self.stage = Stage::Settings,
            (Mode::Uninstall, KeyCode::Left | KeyCode::Right | KeyCode::Tab) => {
                self.remove_focused = !self.remove_focused;
            }
            (Mode::Uninstall, KeyCode::Enter) if self.remove_focused && ready => {
                self.stage = Stage::Run;
                self.lines.clear();
                return (false, Some(Request::Install(Command::Apply)));
            }
            (Mode::Uninstall, KeyCode::Enter) if !self.remove_focused => {
                return (true, Some(Request::Install(Command::Cancel)));
            }
            (Mode::Uninstall, KeyCode::Char('b')) => self.stage = Stage::Check,
            _ => {}
        }
        (false, None)
    }
}
