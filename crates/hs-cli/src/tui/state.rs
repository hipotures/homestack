//! What the TUI shows and what a key does (no drawing, no I/O): the status
//! from the server, the selection, the filter, the panels shown and the
//! boxes over the screen. A key returns a [`Request`] when the loop has to
//! read something.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_model::status::{GlobalStatus, StatusEntry, WorkspaceStatus};
use serde_json::Value;

use super::layout::Shown;
use super::theme::{Palette, Preferences, THEMES};
use crate::AppError;

/// What the loop has to do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Read `/v1/workspaces/{vmid}`.
    Details(u32),
    /// Menu › Check the servers (`homestack transport`).
    Transport,
    /// Menu › Check this desktop (`homestack discover`).
    Discover,
    /// Start an action (its plan first).
    Act(super::action::Start),
    /// The user's answer to the running action.
    Decide(super::action::Decision),
    /// A step of the install screen, for its worker.
    Install(super::installing::Command),
    /// The answer to ssh's question (`None` cancels it).
    Answer(Option<String>),
    /// List the Herdr tabs for the install screen.
    HerdrTabs,
    /// The install ended: read `client.toml` again and show the main screen.
    Installed,
    /// Read the status now (an action ended).
    Reload,
    Quit,
}

/// The presets `p` walks through: their names and panels.
pub const VIEWS: [(&str, Shown); 3] = [
    (
        "auto",
        Shown {
            workspaces: true,
            cluster: true,
            storage: true,
            log: true,
        },
    ),
    (
        "1",
        Shown {
            workspaces: true,
            cluster: true,
            storage: false,
            log: false,
        },
    ),
    (
        "2",
        Shown {
            workspaces: true,
            cluster: false,
            storage: false,
            log: false,
        },
    ),
];

/// The items of the menu, in order.
pub const MENU: [&str; 7] = [
    "Check the servers",
    "Check this desktop",
    "Install or reconfigure",
    "Uninstall",
    "Theme",
    "Help: every key",
    "Quit",
];

/// A box over the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Menu {
        index: usize,
    },
    /// The theme list; `previous` comes back with Esc.
    Themes {
        index: usize,
        previous: Box<Palette>,
    },
    Help,
    /// A read-only report: the server check or the desktop check.
    Report {
        title: String,
        lines: Vec<String>,
    },
}

/// One line of the log panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: String,
    pub text: String,
}

const LOG_LIMIT: usize = 500;

#[derive(Debug, Clone)]
pub struct App {
    pub preferences: Preferences,
    pub palette: Palette,
    no_color: bool,
    pub status: Option<GlobalStatus>,
    /// Every server: its URL and why it did not answer, if it did not.
    pub servers: Vec<(String, Option<String>)>,
    /// Set when no server answered: each URL with its reason.
    pub unreachable: Vec<(String, String)>,
    pub details: Option<WorkspaceStatus>,
    pub shown: Shown,
    pub view: usize,
    /// The selected workspace.
    pub selected: Option<u32>,
    /// Details split the list at narrow widths (Enter).
    pub info: bool,
    pub filter: String,
    pub filtering: bool,
    /// The node carousel: `None` is every node.
    pub node: Option<usize>,
    pub overlay: Option<Overlay>,
    /// Oldest first.
    pub lines: Vec<LogLine>,
    pub clock: String,
    /// Where the list's rows were drawn last: x, width, the first row's y,
    /// the rows shown and the index of the first; for the mouse.
    pub list_rows: std::cell::Cell<(u16, u16, u16, u16, usize)>,
    /// Tenths of a second spent waiting for the first status.
    pub waiting: u32,
    /// Shown instead of the list: a title and lines (no client.toml, …).
    pub notice: Option<(String, Vec<String>)>,
    /// The action box of the workspaces panel.
    pub action: Option<super::action::Action>,
    /// Where the log lines are kept too (`tui.log`); none in tests.
    pub log_file: Option<std::path::PathBuf>,
    /// The install screen, over everything else while it is open.
    pub install: Option<super::install::Install>,
    /// ssh's question, in a box over the screen.
    pub asking: Option<super::install::AskView>,
    /// The desktop's public keys for add key (`~/.ssh/….pub`).
    pub keys: Vec<String>,
}

impl App {
    pub fn new(preferences: Preferences, no_color: bool) -> Result<Self, AppError> {
        let palette = preferences.palette(no_color)?;
        let view = preferences
            .table
            // "preset" is the name of before 2026-10-15.
            .get("view")
            .or_else(|| preferences.table.get("preset"))
            .and_then(toml::Value::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value < VIEWS.len())
            .unwrap_or(0);
        Ok(Self {
            preferences,
            palette,
            no_color,
            status: None,
            servers: Vec::new(),
            unreachable: Vec::new(),
            details: None,
            shown: VIEWS[view].1,
            view,
            selected: None,
            info: false,
            filter: String::new(),
            filtering: false,
            node: None,
            overlay: None,
            lines: Vec::new(),
            clock: String::new(),
            waiting: 0,
            list_rows: std::cell::Cell::new((0, 0, 0, 0, 0)),
            notice: None,
            action: None,
            log_file: None,
            install: None,
            asking: None,
            keys: Vec::new(),
        })
    }

    /// The answer of `/v1/status`.
    pub fn set_status(&mut self, status: &Value) {
        match serde_json::from_value::<GlobalStatus>(status.clone()) {
            Ok(status) => {
                self.status = Some(status);
                self.unreachable.clear();
                self.keep_selection();
            }
            Err(error) => self.log("", &format!("The server's status could not be read: {error}")),
        }
    }

    /// Shown instead of the list until cleared with `None`.
    pub fn set_notice(&mut self, notice: Option<(String, Vec<String>)>) {
        self.notice = notice;
    }

    pub fn set_servers(&mut self, servers: Vec<(String, Option<String>)>) {
        self.servers = servers;
    }

    /// No server answered.
    pub fn set_unreachable(&mut self, servers: Vec<(String, String)>) {
        self.servers = servers
            .iter()
            .map(|(url, reason)| (url.clone(), Some(reason.clone())))
            .collect();
        self.unreachable = servers;
    }

    /// Open the install screen; `first_start` when there is no `client.toml`.
    pub fn open_install(&mut self, mode: super::install::Mode, first_start: bool) {
        self.overlay = None;
        let mut install = super::install::Install::new(mode, first_start);
        // The last access that connected (tui.toml).
        if let Some(host) = self.preferences.table.get("install_host").and_then(toml::Value::as_str) {
            host.clone_into(&mut install.host);
            install.caret = install.host.chars().count();
        }
        self.install = Some(install);
    }

    /// What the install worker tells; it all goes to the log too.
    pub fn install_event(&mut self, event: super::install::Event) {
        use super::install::Event;
        let time = self.clock.clone();
        match &event {
            Event::Line(line) => self.log(&time, line.trim_start_matches("- ")),
            Event::Busy(text) => self.log(&time, &format!("install: {text}")),
            Event::Failed(error) => self.log(&time, &format!("install: {error}")),
            Event::Connected(found) => {
                self.keep_access();
                self.log(&time, &format!("install: connected, {}", found.root));
                let titles: Vec<String> = found
                    .settings
                    .iter()
                    .map(|setting| format!("{} = {}", setting.title, setting.value()))
                    .collect();
                self.log(&time, &format!("settings: {}", titles.join("; ")));
                for (mark, label, text) in &found.check {
                    self.log(&time, &format!("check {} {label}: {text}", mark.symbol()));
                }
            }
            Event::Reviewed { problems, .. } => {
                self.log(&time, &format!("review: {} problems", problems.len()));
                for problem in problems {
                    self.log(&time, &format!("problem: {problem}"));
                }
            }
            Event::Done { ok, lines } => {
                self.log(
                    &time,
                    &format!("install: {}", if *ok { "done" } else { "not finished" }),
                );
                for line in lines {
                    self.log(&time, line);
                }
            }
        }
        if let Some(install) = self.install.as_mut() {
            install.event(event);
        }
    }

    pub fn set_herdr_tabs(&mut self, tabs: Vec<(String, String)>) {
        if let Some(install) = self.install.as_mut() {
            install.tabs = tabs;
            install.tab = 0;
        }
    }

    /// ssh asks something.
    pub fn ask(&mut self, view: super::install::AskView) {
        self.asking = Some(view);
    }

    /// ssh's question `id` is over (the key was touched).
    pub fn ask_ended(&mut self, id: u64) {
        if self.asking.as_ref().is_some_and(|asking| asking.id == id) {
            self.asking = None;
        }
    }

    /// One second more on ssh's question.
    pub fn tick(&mut self) {
        if let Some(asking) = self.asking.as_mut() {
            asking.seconds += 1;
        }
    }

    /// The answer of `/v1/workspaces/{vmid}`; kept only for the selection.
    pub fn set_details(&mut self, vmid: u32, details: &Value) {
        if self.selected != Some(vmid) {
            return;
        }
        if let Ok(details) = serde_json::from_value::<WorkspaceStatus>(details.clone()) {
            self.details = Some(details);
        }
    }

    pub fn log(&mut self, time: &str, text: &str) {
        if let Some(path) = &self.log_file {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path);
            if let Ok(mut file) = file {
                let _ = writeln!(file, "{time}  {text}");
            }
        }
        self.lines.push(LogLine {
            time: time.to_owned(),
            text: text.to_owned(),
        });
        if self.lines.len() > LOG_LIMIT {
            self.lines.remove(0);
        }
    }

    /// The nodes of the cluster, for the carousel.
    #[must_use]
    pub fn nodes(&self) -> Vec<String> {
        self.status
            .as_ref()
            .map(|status| status.summary.nodes.iter().map(|node| node.node.clone()).collect())
            .unwrap_or_default()
    }

    /// The node the carousel shows, `None` for all.
    #[must_use]
    pub fn node_name(&self) -> Option<String> {
        self.node.and_then(|index| self.nodes().get(index).cloned())
    }

    /// The workspaces the list shows: the filter and the node applied.
    #[must_use]
    pub fn rows(&self) -> Vec<&StatusEntry> {
        let Some(status) = &self.status else {
            return Vec::new();
        };
        let node = self.node_name();
        let filter = self.filter.to_lowercase();
        status
            .workspaces
            .iter()
            .filter(|entry| node.as_ref().is_none_or(|node| &entry.node == node))
            .filter(|entry| {
                filter.is_empty()
                    || entry.vmid.to_string().contains(&filter)
                    || entry
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&filter))
            })
            .collect()
    }

    /// The selected row's position in [`App::rows`].
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let vmid = self.selected?;
        self.rows().iter().position(|entry| entry.vmid == vmid)
    }

    /// The selected workspace's row.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&StatusEntry> {
        let vmid = self.selected?;
        self.rows().into_iter().find(|entry| entry.vmid == vmid)
    }

    /// The details of the selection, when they were read.
    #[must_use]
    pub fn selected_details(&self) -> Option<&WorkspaceStatus> {
        self.details
            .as_ref()
            .filter(|details| Some(details.vmid) == self.selected)
    }

    /// Drop a selection the list no longer shows.
    fn keep_selection(&mut self) {
        if self.selected_index().is_none() {
            self.selected = None;
            self.info = false;
        }
    }

    fn select_at(&mut self, index: usize) -> Option<Request> {
        let vmid = self.rows().get(index).map(|entry| entry.vmid)?;
        if self.selected == Some(vmid) {
            return None;
        }
        self.selected = Some(vmid);
        Some(Request::Details(vmid))
    }

    fn move_by(&mut self, step: isize) -> Option<Request> {
        let count = self.rows().len();
        if count == 0 {
            return None;
        }
        let index = match self.selected_index() {
            None => 0,
            Some(at) => at.saturating_add_signed(step).min(count - 1),
        };
        self.select_at(index)
    }

    fn preview(&mut self, index: usize) {
        let mut preferences = self.preferences.clone();
        if let Some(name) = THEMES.get(index) {
            (*name).clone_into(&mut preferences.theme);
            if let Ok(palette) = preferences.palette(self.no_color) {
                self.palette = palette;
            }
        }
    }

    /// Keep the access that connected for the next install screen.
    fn keep_access(&mut self) {
        let Some(install) = &self.install else { return };
        if install.method != super::install::Method::Ssh {
            return;
        }
        let mut table = self.preferences.table.clone();
        table.insert("install_host".to_owned(), toml::Value::String(install.host.clone()));
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => {
                let time = self.clock.clone();
                self.log(&time, &error.0);
            }
        }
    }

    fn save_view(&mut self) {
        let mut table = self.preferences.table.clone();
        table.remove("preset");
        table.insert(
            "view".to_owned(),
            toml::Value::Integer(i64::try_from(self.view).unwrap_or(0)),
        );
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => self.log("", &error.0),
        }
    }

    /// A mouse event: a click on a workspace selects it, the wheel moves the
    /// selection.
    pub fn mouse(&mut self, event: crossterm::event::MouseEvent) -> Option<Request> {
        use super::chrome::Hit;
        use crossterm::event::{MouseButton, MouseEventKind};
        let press = |app: &mut Self, code: KeyCode| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        let in_box = self.install.is_some() || self.overlay.is_some() || self.asking.is_some();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A button in a border is its key; an item of the menu or
                // the themes is chosen and opened.
                match super::chrome::hit_at(event.column, event.row) {
                    Some(Hit::Key(code)) => return press(self, code),
                    // A setting of the install, and a choice in its box.
                    Some(Hit::Setting(row)) => {
                        if let Some(install) = self.install.as_mut() {
                            install.row = row;
                            install.chooser = false;
                        }
                        return None;
                    }
                    Some(Hit::Choice(choice)) => {
                        if let Some(install) = self.install.as_mut() {
                            install.chooser = true;
                            install.cursor = choice;
                        }
                        return press(self, KeyCode::Char(' '));
                    }
                    // A field of a form: focus it; its ‹ or › is a key on it.
                    Some(Hit::Field(field, key)) => {
                        if let Some(action) = self.action.as_mut()
                            && let super::action::Stage::Form(form) = &mut action.stage
                        {
                            form.focus = field;
                        }
                        return key.and_then(|code| press(self, code));
                    }
                    Some(Hit::Item(item)) => {
                        match &mut self.overlay {
                            Some(Overlay::Menu { index }) => *index = item,
                            Some(Overlay::Themes { index, .. }) => *index = item,
                            _ => return None,
                        }
                        if matches!(self.overlay, Some(Overlay::Themes { .. })) {
                            self.preview(item);
                        }
                        return press(self, KeyCode::Enter);
                    }
                    None => {}
                }
                if in_box {
                    return None;
                }
                let (x, width, first, shown, offset) = self.list_rows.get();
                let inside = event.column >= x
                    && event.column < x.saturating_add(width)
                    && event.row >= first
                    && event.row < first.saturating_add(shown);
                if !inside {
                    return None;
                }
                self.select_at(offset + usize::from(event.row - first))
            }
            MouseEventKind::ScrollDown if in_box => press(self, KeyCode::Down),
            MouseEventKind::ScrollUp if in_box => press(self, KeyCode::Up),
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            _ => None,
        }
    }

    /// Handle one key.
    pub fn key(&mut self, key: KeyEvent) -> Option<Request> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Request::Quit);
        }
        if let Some(asking) = self.asking.as_mut() {
            if let Some(answer) = asking.key(key.code) {
                self.asking = None;
                return Some(Request::Answer(answer));
            }
            return None;
        }
        if let Some(install) = self.install.as_mut() {
            let before = install.stage;
            let (close, request) = install.key(key.code);
            let after = install.stage;
            if close {
                self.install = None;
            }
            if before != after {
                let time = self.clock.clone();
                self.log(&time, &format!("install step: {after:?}"));
            }
            return request;
        }
        if let Some(overlay) = self.overlay.take() {
            return self.overlay_key(overlay, key.code);
        }
        if self.filtering {
            match key.code {
                KeyCode::Char(c) => self.filter.push(c),
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                _ => {}
            }
            self.keep_selection();
            return None;
        }
        if self.action.as_ref().is_some_and(|action| !action.hidden) {
            return self.action_key(key.code);
        }
        if let KeyCode::Char(c @ ('n' | 'r' | 'z' | 'k' | 'g' | 'd' | 't')) = key.code {
            return self.open_action(c);
        }
        if key.code == KeyCode::Enter
            && let Some(action) = self.action.as_mut()
        {
            action.hidden = false;
            return None;
        }
        match key.code {
            KeyCode::Char('q') => return Some(Request::Quit),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.keep_selection();
            }
            KeyCode::Char('m') => self.overlay = Some(Overlay::Menu { index: 0 }),
            // Esc leaves, as q does (the owner, 2026-10-14).
            KeyCode::Esc => return Some(Request::Quit),
            KeyCode::Char(digit @ '1'..='4') => {
                self.shown
                    .toggle(u8::try_from(digit.to_digit(10).unwrap_or(0)).unwrap_or(0));
            }
            KeyCode::Char('v') => {
                self.view = (self.view + 1) % VIEWS.len();
                self.shown = VIEWS[self.view].1;
                self.save_view();
            }
            // The standard view: every panel, every node, no filter.
            KeyCode::Char('0') => {
                self.view = 0;
                self.shown = VIEWS[0].1;
                self.node = None;
                self.filter.clear();
                self.keep_selection();
                self.save_view();
            }
            KeyCode::Char('f') => self.filtering = true,
            KeyCode::Right | KeyCode::Left => {
                let count = self.nodes().len();
                if count > 0 {
                    // None, 0, 1, …, count-1, None, …
                    let position = self.node.map_or(0, |index| index + 1);
                    let next = if key.code == KeyCode::Right {
                        (position + 1) % (count + 1)
                    } else {
                        (position + count) % (count + 1)
                    };
                    self.node = next.checked_sub(1);
                    self.keep_selection();
                }
            }
            KeyCode::Down => return self.move_by(1),
            KeyCode::Up => return self.move_by(-1),
            KeyCode::PageDown => return self.move_by(10),
            KeyCode::PageUp => return self.move_by(-10),
            KeyCode::Home => return self.select_at(0),
            KeyCode::End => return self.select_at(self.rows().len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('i') if self.selected.is_some() => self.info = !self.info,
            _ => {}
        }
        None
    }

    fn overlay_key(&mut self, overlay: Overlay, code: KeyCode) -> Option<Request> {
        match overlay {
            Overlay::Menu { index } => match code {
                KeyCode::Down => {
                    self.overlay = Some(Overlay::Menu {
                        index: (index + 1) % MENU.len(),
                    })
                }
                KeyCode::Up => {
                    self.overlay = Some(Overlay::Menu {
                        index: (index + MENU.len() - 1) % MENU.len(),
                    });
                }
                KeyCode::Enter => match index {
                    0 => {
                        self.overlay = Some(Overlay::Report {
                            title: "servers".to_owned(),
                            lines: vec!["Asking every server…".to_owned()],
                        });
                        return Some(Request::Transport);
                    }
                    1 => {
                        self.overlay = Some(Overlay::Report {
                            title: "this desktop".to_owned(),
                            lines: vec!["Checking this desktop…".to_owned()],
                        });
                        return Some(Request::Discover);
                    }
                    2 | 3 => {
                        let mode = if index == 2 {
                            super::install::Mode::Install
                        } else {
                            super::install::Mode::Uninstall
                        };
                        self.open_install(mode, false);
                    }
                    4 => {
                        let current = THEMES.iter().position(|name| *name == self.palette.name).unwrap_or(0);
                        self.overlay = Some(Overlay::Themes {
                            index: current,
                            previous: Box::new(self.palette.clone()),
                        });
                    }
                    5 => self.overlay = Some(Overlay::Help),
                    _ => return Some(Request::Quit),
                },
                KeyCode::Char('q') => return Some(Request::Quit),
                KeyCode::Esc | KeyCode::Char('m') => {}
                _ => self.overlay = Some(Overlay::Menu { index }),
            },
            Overlay::Themes { index, previous } => match code {
                KeyCode::Down | KeyCode::Up => {
                    let index = if code == KeyCode::Down {
                        (index + 1) % THEMES.len()
                    } else {
                        (index + THEMES.len() - 1) % THEMES.len()
                    };
                    self.preview(index);
                    self.overlay = Some(Overlay::Themes { index, previous });
                }
                KeyCode::Enter => {
                    let name = THEMES.get(index).copied().unwrap_or(THEMES[0]);
                    if let Err(error) = self.preferences.save_theme(name) {
                        self.palette = *previous;
                        self.log("", &error.0);
                    }
                }
                KeyCode::Esc => {
                    self.palette = *previous;
                    self.overlay = Some(Overlay::Menu { index: 4 });
                }
                _ => self.overlay = Some(Overlay::Themes { index, previous }),
            },
            Overlay::Help | Overlay::Report { .. } => match code {
                KeyCode::Esc | KeyCode::Enter => {}
                KeyCode::Char('m') => self.overlay = Some(Overlay::Menu { index: 0 }),
                KeyCode::Char('q') => return Some(Request::Quit),
                _ => self.overlay = Some(overlay),
            },
        }
        None
    }
}
