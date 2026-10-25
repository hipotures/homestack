//! What the TUI shows and what a key does (no drawing, no I/O): the status
//! from the server, the selection, the filter, the panels shown and the
//! boxes over the screen. A key returns a [`Request`] when the loop has to
//! read something.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hs_model::status::{GlobalStatus, StatusEntry, WorkspaceStatus};
use serde_json::Value;
use std::time::{Duration, Instant};

use super::layout::Shown;
use super::theme::{Palette, Preferences, THEMES};
use crate::AppError;

/// What the loop has to do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
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
    /// Open the setup box of the workspace (`None`: this desktop).
    Setup(Option<u32>),
    /// What the setup box asks its worker.
    SetupAsk(super::setup::Ask),
    /// Give the terminal to ssh on the running workspace (step 4).
    Ssh(u32),
    /// Read the images of these server containers and the registry's.
    Images(Vec<u32>),
    /// Update existing API instances from the saved deployment, without root.
    UpdateApi,
    /// Copy the already loaded API responses to the terminal's clipboard.
    CopyJson,
    TestIntegration(u64, Box<super::integrations::Editor>),
    TestPangolin(u64, String),
    DiscoverPangolin(u64, String),
    PangolinApi(
        u64,
        String,
        super::pangolin::Discovery,
        Option<Box<super::pangolin_api::Plan>>,
    ),
    PangolinTunnel(u64, super::pangolin_tunnel::Spec),
    StopPangolinTunnel(bool),
    /// Verify the bootstrap key of this run; the secret stays in the wizard.
    VerifyPangolinKey(u64),
    PangolinPermanent(u64),
    /// Open Step 8's deployment screen with the wizard's live tunnel.
    PangolinSites,
    Quit,
}

/// The views `0`–`4` select and `v` walks through: their names and panels.
pub const VIEWS: [(&str, Shown); 5] = [
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
            cluster: false,
            storage: false,
            log: false,
        },
    ),
    (
        "2",
        Shown {
            workspaces: false,
            cluster: true,
            storage: true,
            log: false,
        },
    ),
    (
        "3",
        Shown {
            workspaces: false,
            cluster: false,
            storage: true,
            log: false,
        },
    ),
    (
        "4",
        Shown {
            workspaces: false,
            cluster: false,
            storage: false,
            log: true,
        },
    ),
];

/// The items of the menu, in order.
pub const MENU: [&str; 7] = [
    "Check the servers",
    "Check this desktop",
    "Settings",
    "Help: every key",
    "Set up this desktop (backup)",
    "Update API containers",
    "Quit",
];

/// The machines the list shows by their state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    All,
    Online,
    Offline,
}

impl Power {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }

    /// The next place of the carousel, or the previous one.
    #[must_use]
    pub fn turn(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::All, true) | (Self::Offline, false) => Self::Online,
            (Self::Online, true) | (Self::All, false) => Self::Offline,
            (Self::Offline, true) | (Self::Online, false) => Self::All,
        }
    }
}

/// The sections of the settings box, in order.
pub const SETTINGS: [&str; 6] = ["General", "Appearance", "Machines", "System", "Refresh", "Integrations"];
/// The section of [`SETTINGS`] that holds the sorting.
pub const GENERAL: usize = 0;
/// The section of [`SETTINGS`] that holds the theme.
pub const APPEARANCE: usize = 1;
/// The section of [`SETTINGS`] that holds the list's columns.
pub const MACHINES: usize = 2;
/// The section of [`SETTINGS`] that holds how the servers read Proxmox.
pub const SYSTEM: usize = 3;
/// The section of [`SETTINGS`] that holds the integrations.
pub const INTEGRATIONS: usize = 5;

/// The choices of how many Proxmox requests a server reads at a time.
pub const PARALLEL_CHOICES: [usize; 6] = [1, 2, 4, 8, 16, 32];
/// How many Proxmox requests a server reads at a time for the status, as
/// `tui.toml` keeps it; the status reader asks with it.
pub static PARALLEL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(8);

/// The keys the machine list sorts by.
pub const SORT_KEYS: [&str; 7] = ["NAME", "NODE", "VMID", "TYPE", "STATE", "POOL", "SERVICE"];
/// The machine list's first and second key of [`SORT_KEYS`] without a
/// choice in `tui.toml`.
const SORT_DEFAULT: [usize; 2] = [0, 1];

/// The list's columns in their order, and whether one is shown without a
/// choice in `tui.toml`.
pub const COLUMNS: [(&str, bool); 21] = [
    ("TAGS", true),
    ("VMID", true),
    ("TYPE", true),
    ("NAME", true),
    ("NODE", true),
    ("STATE", true),
    ("ADDRESS", true),
    ("HOME", true),
    ("HOME USED", false),
    ("HOME FREE", true),
    ("ROOT", true),
    ("ROOT USED", false),
    ("ROOT FREE", true),
    ("STORAGE", false),
    ("POOL", false),
    ("CPU", false),
    ("MEM", false),
    ("IO", false),
    ("SERVICE", false),
    ("IMAGE", false),
    ("TAG", false),
];

/// A box over the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Quit,
    Menu {
        index: usize,
    },
    /// The settings under the cluster panel; `section` is one of [`SETTINGS`].
    Settings {
        section: usize,
    },
    Integrations {
        index: usize,
    },
    /// The column list in the Machines section; `index` is one of [`COLUMNS`].
    Columns {
        index: usize,
    },
    /// The open list of the System section's parallel reads; `index` is one
    /// of [`PARALLEL_CHOICES`].
    Parallel {
        index: usize,
    },
    System {
        index: usize,
    },
    /// The sort rows of the General section; `index` is the first (0) or
    /// the second (1) key.
    General {
        index: usize,
    },
    /// The open list of a sort row; `level` is the row, `index` one of
    /// [`SORT_KEYS`].
    Sort {
        level: usize,
        index: usize,
    },
    Integration(Box<super::integrations::Editor>),
    Pangolin(Box<super::pangolin::Wizard>),
    /// The theme list in the Appearance section; `previous` comes back with Esc.
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
    /// An explicit action waiting for the selected VM's details.
    ReadDetails {
        vmid: u32,
        action: DetailsAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsAction {
    Resize,
}

impl DetailsAction {
    pub fn word(self) -> &'static str {
        match self {
            Self::Resize => "resize",
        }
    }
}

#[derive(Debug, Clone)]
struct Details {
    vmid: u32,
    read_at: Instant,
    result: Result<WorkspaceStatus, String>,
    json: Option<Value>,
}

const DETAILS_TTL: Duration = Duration::from_secs(10);

/// One line of the log panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: String,
    pub text: String,
    /// From an earlier run (`tui.log`): drawn muted.
    pub earlier: bool,
}

const LOG_LIMIT: usize = 500;
/// The lines of the earlier runs shown at the start.
const EARLIER_LINES: usize = 200;
/// How much of the end of `tui.log` is read for them.
const EARLIER_BYTES: u64 = 256 * 1024;

/// The last `most` bytes of `path` as text, without the line they cut.
fn read_tail(path: &std::path::Path, most: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(most);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(if start > 0 {
        text.split_once('\n')
            .map_or_else(String::new, |(_, rest)| rest.to_owned())
    } else {
        text.into_owned()
    })
}

/// The order of two machines by `key` of [`SORT_KEYS`].
fn compare(key: &str, a: &StatusEntry, b: &StatusEntry) -> std::cmp::Ordering {
    match key {
        "NAME" => a.name.cmp(&b.name),
        "NODE" => a.node.cmp(&b.node),
        "TYPE" => (a.role == "CT").cmp(&(b.role == "CT")),
        "STATE" => a.status.cmp(&b.status),
        "POOL" => a.pool.cmp(&b.pool),
        "SERVICE" => a
            .service
            .as_ref()
            .map(|service| &service.id)
            .cmp(&b.service.as_ref().map(|service| &service.id)),
        _ => a.vmid.cmp(&b.vmid),
    }
}

#[derive(Debug, Clone)]
pub struct App {
    pub profile: String,
    /// The isolated lab's Gold shell can also start its stopped source VM.
    pub lab_gold_vmid: Option<u32>,
    pub preferences: Preferences,
    pub ssh_sounds: bool,
    pub palette: Palette,
    no_color: bool,
    pub status: Option<GlobalStatus>,
    status_json: Option<Value>,
    storage_json: Option<Value>,
    pub storage_overview: Option<Result<hs_model::status::StorageOverview, String>>,
    pub api_updates: Option<Result<super::reinstall::Images, String>>,
    pub api_updates_checking: bool,
    pub startup_message: Option<&'static str>,
    /// Every server: its URL and why it did not answer, if it did not.
    pub servers: Vec<(String, Option<String>)>,
    /// Set when no server answered: each URL with its reason.
    pub unreachable: Vec<(String, String)>,
    details: Option<Details>,
    pub shown: Shown,
    pub view: usize,
    /// The selected workspace.
    pub selected: Option<u32>,
    /// Show the selected VM's details (`i`).
    pub info: bool,
    pub filter: String,
    pub filtering: bool,
    /// The machines shown by their state (`o`).
    pub power: Power,
    /// Whether each of [`COLUMNS`] is shown.
    pub columns: [bool; COLUMNS.len()],
    /// The machine list's first and second key of [`SORT_KEYS`].
    pub sort: [usize; 2],
    /// The node carousel: `None` is every node.
    pub node: Option<usize>,
    /// The group carousel: `None` is all, else a pool's ID.
    pub group: Option<String>,
    /// The carousel shows the favorites (its first place, the start).
    pub starred: bool,
    /// The VMIDs starred with `f`, kept in `tui.toml`.
    pub favorites: std::collections::BTreeSet<u32>,
    /// ← → turn the group carousel (the default) or, after Tab, the node one.
    pub on_group: bool,
    pub overlay: Option<Overlay>,
    /// Oldest first.
    pub lines: Vec<LogLine>,
    /// Lines the log is scrolled back from the newest.
    pub log_scroll: usize,
    pub storage_scroll: std::cell::Cell<usize>,
    pub storage_scroll_max: std::cell::Cell<usize>,
    /// The highlighted line of the log, lines back from the newest.
    pub log_cursor: Option<usize>,
    /// New lines that came above the view while the log was scrolled back:
    /// the log's border shows ┐*┌ until it is scrolled to the newest.
    pub log_unread: usize,
    /// The scrollbar whose thumb the mouse holds, and the row of the thumb
    /// it holds.
    pub dragging: Option<(super::chrome::Bar, u16)>,
    pub clock: String,
    /// Local UTC offset shared with the existing TUI clock.
    pub utc_offset: i64,
    /// The local date `YYYY-MM-DD` for `tui.log`; empty in tests.
    pub today: String,
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
    /// The setup box, over the workspaces panel while it is open.
    pub setup: Option<super::setup::SetupBox>,
    /// The panels before the setup box opened (it hides storage and log
    /// for its width); they come back when it closes.
    pub before_setup: Option<Shown>,
    /// The desktop's public keys for add key (`~/.ssh/….pub`).
    pub keys: Vec<super::action::KeyFile>,
    /// The server's workspace pool (`/v1/capabilities`), for new.
    pub pool: Option<String>,
    /// Gold's pool (`/v1/capabilities`), for detail reads.
    pub gold_pool: Option<String>,
    /// The server's default home size in MiB (`/v1/config`), for new.
    pub home_default: Option<u64>,
    /// The server's configured workspace network (`/v1/config`).
    pub network: Option<hs_model::config::Network>,
    /// The server containers marked with Space, for one reinstall.
    pub marked: std::collections::BTreeSet<u32>,
    /// What the reinstall box shows, once read.
    pub images: Option<Result<super::reinstall::Images, String>>,
    /// The CTIDs `deploy.toml` saves as HomeStack API instances.
    pub api_instances: std::collections::BTreeSet<u32>,
}

impl App {
    pub fn new(preferences: Preferences, no_color: bool) -> Result<Self, AppError> {
        let palette = preferences.palette(no_color)?;
        let client_path = preferences.path.with_file_name("client.toml");
        let ssh_sounds = crate::config::load(&client_path)
            .map(|loaded| crate::touch::Sounds::from_table(loaded.client.sounds.as_ref()).enabled)
            .unwrap_or(false);
        let view = preferences
            .table
            // "preset" is the name of before 2026-10-15.
            .get("view")
            .or_else(|| preferences.table.get("preset"))
            .and_then(toml::Value::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value < VIEWS.len())
            .unwrap_or(0);
        let chosen: Option<Vec<&str>> = preferences
            .table
            .get("columns")
            .and_then(toml::Value::as_array)
            .map(|keys| keys.iter().filter_map(toml::Value::as_str).collect());
        if let Some(parallel) = preferences
            .table
            .get("parallel")
            .and_then(toml::Value::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| PARALLEL_CHOICES.contains(value))
        {
            PARALLEL.store(parallel, std::sync::atomic::Ordering::SeqCst);
        }
        let columns = COLUMNS.map(|(key, shown)| chosen.as_ref().map_or(shown, |chosen| chosen.contains(&key)));
        let sort = preferences
            .table
            .get("sort")
            .and_then(toml::Value::as_table)
            .and_then(|sort| sort.get("machines"))
            .and_then(toml::Value::as_array)
            .and_then(|keys| {
                let keys: Vec<usize> = keys
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .filter_map(|key| SORT_KEYS.iter().position(|known| *known == key))
                    .collect();
                <[usize; 2]>::try_from(keys).ok()
            })
            .filter(|[first, second]| first != second)
            .unwrap_or(SORT_DEFAULT);
        let favorites = preferences
            .table
            .get("favorites")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_integer)
            .filter_map(|vmid| u32::try_from(vmid).ok())
            .collect();
        Ok(Self {
            favorites,
            starred: true,
            lab_gold_vmid: None,
            profile: "default".to_owned(),
            preferences,
            ssh_sounds,
            palette,
            no_color,
            status: None,
            status_json: None,
            storage_json: None,
            storage_overview: None,
            api_updates: None,
            api_updates_checking: false,
            api_instances: std::collections::BTreeSet::new(),
            startup_message: None,
            servers: Vec::new(),
            unreachable: Vec::new(),
            details: None,
            shown: VIEWS[view].1,
            view,
            selected: None,
            info: false,
            filter: String::new(),
            filtering: false,
            power: Power::All,
            columns,
            sort,
            node: None,
            group: None,
            on_group: true,
            overlay: None,
            lines: Vec::new(),
            log_scroll: 0,
            storage_scroll: std::cell::Cell::new(0),
            storage_scroll_max: std::cell::Cell::new(0),
            log_cursor: None,
            log_unread: 0,
            dragging: None,
            clock: String::new(),
            utc_offset: 0,
            today: String::new(),
            waiting: 0,
            list_rows: std::cell::Cell::new((0, 0, 0, 0, 0)),
            notice: None,
            action: None,
            log_file: None,
            install: None,
            asking: None,
            keys: Vec::new(),
            pool: None,
            gold_pool: None,
            home_default: None,
            network: None,
            marked: std::collections::BTreeSet::new(),
            images: None,
            setup: None,
            before_setup: None,
        })
    }

    /// The answer of `/v1/status`.
    pub fn set_status(&mut self, status: &Value) {
        let json = status;
        match serde_json::from_value::<GlobalStatus>(status.clone()) {
            Ok(mut status) => {
                self.status_json = Some(json.clone());
                // Guest power state cannot be confirmed while its node is
                // unavailable, even if cluster inventory retains the last value.
                for entry in status
                    .gold
                    .iter_mut()
                    .chain(&mut status.workspaces)
                    .chain(&mut status.others)
                {
                    if entry.node_status != "online" {
                        entry.status = "unknown".to_owned();
                    }
                }
                self.status = Some(status);
                self.apply_api_updates();
                self.share_services();
                self.unreachable.clear();
                self.keep_selection();
            }
            Err(error) => self.log("", &format!("The server's status could not be read: {error}")),
        }
    }

    /// The service containers the status lists, for the Services group.
    pub fn share_services(&mut self) {
        let services: Vec<StatusEntry> = self
            .status
            .iter()
            .flat_map(|status| status.others.iter())
            .filter(|entry| entry.service.is_some())
            .cloned()
            .collect();
        if let Some(setup) = self.setup.as_mut() {
            setup.services = services;
        }
    }

    /// Preserve the API response before normalizing storage for display.
    pub fn set_storage_json(&mut self, result: Result<Value, String>) -> bool {
        self.storage_json = None;
        let result = result.and_then(|json| {
            let overview =
                serde_json::from_value(json.clone()).map_err(|error| format!("Invalid storage data: {error}"))?;
            self.storage_json = Some(json);
            Ok(overview)
        });
        self.set_storage_overview(result)
    }

    /// Original responses behind the current view; never performs I/O.
    pub fn displayed_json(&self, area: ratatui::layout::Rect) -> Option<Value> {
        let screen = super::layout::arrange(area, self.shown);
        if screen.too_small || self.install.is_some() {
            return None;
        }
        let mut responses = serde_json::Map::new();
        if (screen.workspaces.is_some() || screen.cluster.is_some() || screen.storage.is_some())
            && let Some(status) = &self.status_json
        {
            responses.insert("/v1/status".to_owned(), status.clone());
        }
        if matches!(self.view, 2 | 3)
            && screen.storage.is_some()
            && let Some(storage) = &self.storage_json
        {
            responses.insert("/v1/storage/overview".to_owned(), storage.clone());
        }
        if self.info
            && screen.workspaces.is_some()
            && !self.action.as_ref().is_some_and(|action| !action.hidden)
            && let Some(details) = &self.details
            && self.selected == Some(details.vmid)
            && let Some(json) = &details.json
        {
            responses.insert(format!("/v1/workspaces/{}", details.vmid), json.clone());
        }
        if screen.log.is_some() {
            let logs = self.lines[self.lines.len().saturating_sub(100)..]
                .iter()
                .map(|line| serde_json::json!({"time": line.time, "text": line.text}))
                .collect();
            responses.insert("logs".to_owned(), Value::Array(logs));
            return Some(Value::Object(responses));
        }
        match responses.len() {
            0 => None,
            1 => responses.into_values().next(),
            _ => Some(Value::Object(responses)),
        }
    }

    /// Treat inventory order as presentation data, not a storage change.
    pub fn set_storage_overview(&mut self, mut result: Result<hs_model::status::StorageOverview, String>) -> bool {
        if let Ok(overview) = &mut result {
            for storage in &mut overview.storages {
                let mut content: Vec<_> = storage
                    .content
                    .split(',')
                    .map(str::trim)
                    .filter(|kind| !kind.is_empty())
                    .collect();
                content.sort_unstable();
                content.dedup();
                storage.content = content.join(",");
                storage.nodes.sort();
                storage.nodes.dedup();
            }
            overview
                .storages
                .sort_by(|a, b| (&a.storage, &a.nodes).cmp(&(&b.storage, &b.nodes)));
            overview
                .latest_backups
                .sort_by(|a, b| (a.vmid, &a.storage, a.created_at).cmp(&(b.vmid, &b.storage, b.created_at)));
            overview.warnings.sort();
            overview.warnings.dedup();
        }
        if self.storage_overview.as_ref() == Some(&result) {
            return false;
        }
        self.storage_overview = Some(result);
        true
    }

    /// The deployment and registry are read directly, independently of API status.
    pub fn set_api_updates(&mut self, result: Result<super::reinstall::Images, String>) {
        self.api_updates_checking = false;
        let message = match &result {
            Ok(images) => {
                let updates: Vec<_> = images
                    .servers
                    .iter()
                    .filter_map(|server| {
                        images
                            .update_for(server)
                            .map(|image| format!("{} → {}", server.name, image.tag()))
                    })
                    .collect();
                if updates.is_empty() {
                    "API image check complete: no newer image found".to_owned()
                } else {
                    format!(
                        "API update available: {}. Menu › Update API containers.",
                        updates.join(", ")
                    )
                }
            }
            Err(error) => format!("Cannot check API image updates: {error}"),
        };
        self.api_updates = Some(result);
        self.apply_api_updates();
        let clock = self.clock.clone();
        self.log(&clock, &message);
    }

    /// A compact indicator remains visible even if the main status is invalid.
    #[must_use]
    pub fn api_update_label(&self) -> Option<String> {
        if self.api_updates_checking {
            return Some("API …".to_owned());
        }
        match self.api_updates.as_ref()? {
            Err(_) => Some("API ?".to_owned()),
            Ok(images) => {
                let count = images
                    .servers
                    .iter()
                    .filter(|server| images.update_for(server).is_some())
                    .count();
                if count > 0 {
                    Some(format!("API ↑{count}"))
                } else if images
                    .servers
                    .iter()
                    .any(|server| server.image.as_ref().is_none_or(|image| image.created.is_none()))
                    || !images.published.iter().any(|image| image.created.is_some())
                {
                    Some("API ?".to_owned())
                } else {
                    None
                }
            }
        }
    }

    fn apply_api_updates(&mut self) {
        let (Some(status), Some(Ok(images))) = (&mut self.status, &self.api_updates) else {
            return;
        };
        for entry in &mut status.others {
            let Some(server) = images.servers.iter().find(|server| server.vmid == entry.vmid) else {
                continue;
            };
            // A later status may already describe a container updated elsewhere.
            if let (Some(reported), Some(installed)) = (&entry.image, &server.image)
                && reported != &installed.image
            {
                continue;
            }
            let available = images.update_for(server).map(|image| image.image.clone());
            let known = server.image.as_ref().is_some_and(|image| image.created.is_some())
                && images.published.iter().any(|image| image.created.is_some());
            entry.image_update = Some(hs_model::status::ImageUpdate {
                state: if available.is_some() {
                    hs_model::status::ImageUpdateState::Available
                } else if known {
                    hs_model::status::ImageUpdateState::UpToDate
                } else {
                    hs_model::status::ImageUpdateState::Unknown
                },
                available,
                checked_at: None,
                error: (!known).then(|| "Installed or published image metadata is unavailable".to_owned()),
            });
            if let Some(image) = &server.image {
                entry.image = Some(image.image.clone());
            }
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
    /// The access that connected last (tui.toml) is chosen again; for Herdr
    /// its tabs are to be read.
    pub fn open_install(&mut self, mode: super::install::Mode, first_start: bool) -> Option<Request> {
        self.overlay = None;
        let mut install = super::install::Install::new(mode, first_start);
        let kept = |key: &str| self.preferences.table.get(key).and_then(toml::Value::as_str);
        if let Some(host) = kept("install_host") {
            host.clone_into(&mut install.host);
            install.caret = install.host.chars().count();
        }
        if let Some(filter) = kept("install_tab_filter") {
            filter.clone_into(&mut install.tab_filter);
            install.tab_caret = install.tab_filter.chars().count();
        }
        let herdr = kept("install_method") == Some("herdr");
        if herdr {
            install.method = super::install::Method::Herdr;
        }
        self.install = Some(install);
        herdr.then_some(Request::HerdrTabs)
    }

    /// Step 8 uses the saved access host and keeps the live Pangolin tunnel.
    pub fn sites_install(&self) -> super::install::Install {
        let mut install = super::install::Install::new(super::install::Mode::Sites, false);
        if let Some(host) = self.preferences.table.get("install_host").and_then(toml::Value::as_str) {
            host.clone_into(&mut install.host);
            install.caret = install.host.chars().count();
        }
        install
    }

    pub fn open_sites(&mut self) {
        self.install = Some(self.sites_install());
    }

    /// Setup of this desktop; with `services`, straight on the Services tab
    /// (F9), where services are deployed and maintained.
    fn open_desktop_setup(&mut self, services: bool) -> Option<Request> {
        if self.action.is_some() || self.setup.is_some() {
            return None;
        }
        let mut setup = super::setup::SetupBox::loading(
            crate::setup::Settings::default(),
            crate::setup::Target {
                name: "this desktop".to_owned(),
                local: true,
                ..crate::setup::Target::default()
            },
            "Reading this desktop",
        );
        setup.services_first = services;
        self.open_setup_box(setup);
        Some(Request::Setup(None))
    }

    /// The deployment of a service: its settings come from the cluster.
    pub fn open_service_deploy(&mut self, name: &str) {
        let mut install = super::install::Install::new(super::install::Mode::Service, false);
        install.subject = Some(name.to_owned());
        install.stage = super::install::Stage::Settings;
        install.busy = Some("Reading the cluster and the published releases".to_owned());
        self.install = Some(install);
    }

    /// Clear cached cluster state after an install or uninstall finishes; when
    /// `has_client_config` is false (a production uninstall retired
    /// `client.toml`), open the first-time install screen instead of leaving a
    /// missing-file notice on the main screen.
    pub fn after_installed(&mut self, has_client_config: bool) -> Option<Request> {
        self.clear_details();
        self.status = None;
        self.status_json = None;
        self.storage_json = None;
        self.storage_overview = None;
        self.servers.clear();
        self.unreachable.clear();
        self.pool = None;
        self.gold_pool = None;
        self.set_notice(None);
        if has_client_config {
            None
        } else {
            self.open_install(super::install::Mode::Install, true)
        }
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
            let kept = self.preferences.table.get("install_tab").and_then(toml::Value::as_str);
            install.set_tabs(tabs, kept);
        }
    }

    /// ssh asks something.
    pub fn ask(&mut self, view: super::install::AskView) {
        self.asking = Some(view);
    }

    /// An ssh connection is being opened in the install or the setup box.
    #[must_use]
    pub fn connecting(&self) -> bool {
        self.install.as_ref().is_some_and(super::install::Install::connecting)
            || self
                .setup
                .as_ref()
                .is_some_and(|setup| matches!(setup.stage, super::setup::Stage::Loading { .. }))
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

    /// The answer of `/v1/workspaces/{vmid}`; resume an action waiting for it.
    pub fn set_details(&mut self, vmid: u32, result: Result<Value, String>) -> Option<Request> {
        if self.selected != Some(vmid) {
            return None;
        }
        let json = result.as_ref().ok().cloned();
        let result = result.and_then(|value| {
            let details: WorkspaceStatus = serde_json::from_value(value)
                .map_err(|error| format!("The server's details could not be read: {error}"))?;
            if details.vmid != vmid {
                return Err("The server returned details of another VM.".to_owned());
            }
            Ok(details)
        });
        self.details = Some(Details {
            vmid,
            read_at: Instant::now(),
            json: result.is_ok().then_some(json).flatten(),
            result,
        });
        if let Some(Overlay::ReadDetails { vmid: waiting, action }) = self.overlay
            && waiting == vmid
        {
            self.overlay = None;
            if self.wait_for_details(action) {
                return None;
            }
            return match action {
                DetailsAction::Resize => self.open_action('z'),
            };
        }
        None
    }

    /// An operation or a new connection invalidates both results and errors.
    pub fn clear_details(&mut self) {
        self.details = None;
    }

    pub fn details_fresh(&self, vmid: u32) -> bool {
        self.details
            .as_ref()
            .is_some_and(|details| details.vmid == vmid && details.read_at.elapsed() < DETAILS_TTL)
    }

    /// Why the selected entry cannot have a detail read yet.
    pub fn details_unavailable(&self, entry: &StatusEntry) -> Option<&'static str> {
        if entry.role == "CT" {
            return Some("Workspace details are unavailable for containers.");
        }
        let (Some(pool), Some(gold_pool)) = (&self.pool, &self.gold_pool) else {
            return Some("Waiting for the server's pools");
        };
        if entry
            .pool
            .as_ref()
            .is_none_or(|entry_pool| entry_pool != pool && entry_pool != gold_pool)
        {
            return Some("Details are unavailable outside this server's pools.");
        }
        None
    }

    /// The current demand: explicit actions read immediately, visible info
    /// waits until navigation settles. Fresh results (including errors) are reused.
    pub fn details_target(&self) -> Option<(u32, bool)> {
        if self.install.is_some() || self.setup.is_some() || self.notice.is_some() || !self.unreachable.is_empty() {
            return None;
        }
        let entry = self.selected_entry()?;
        let debounce = match self.overlay {
            Some(Overlay::ReadDetails { vmid, .. }) if vmid == entry.vmid => false,
            None if self.info && self.shown.workspaces && self.action.as_ref().is_none_or(|action| action.hidden) => {
                true
            }
            _ => return None,
        };
        (self.details_unavailable(entry).is_none() && !self.details_fresh(entry.vmid)).then_some((entry.vmid, debounce))
    }

    pub fn details_error(&self) -> Option<&str> {
        let vmid = self.selected?;
        self.details
            .as_ref()
            .filter(|_| self.details_fresh(vmid))?
            .result
            .as_ref()
            .err()
            .map(String::as_str)
    }

    /// Return true while loading, or after showing why the action cannot read details.
    pub(super) fn wait_for_details(&mut self, action: DetailsAction) -> bool {
        let Some(entry) = self.selected_entry() else {
            return true;
        };
        let vmid = entry.vmid;
        // Capabilities can still be loading when the user starts an action.
        let pools_known = self.pool.is_some() && self.gold_pool.is_some();
        let unavailable = (pools_known || entry.role == "CT")
            .then(|| self.details_unavailable(entry))
            .flatten();
        if let Some(reason) = unavailable.or_else(|| self.details_error()) {
            self.overlay = Some(Overlay::Report {
                title: format!("{} {vmid}", action.word()),
                lines: vec![reason.to_owned()],
            });
            return true;
        }
        if self.details_fresh(vmid) {
            return false;
        }
        self.overlay = Some(Overlay::ReadDetails { vmid, action });
        true
    }

    /// The last lines of `tui.log` from the earlier runs, muted, under a
    /// separator line: the log does not start empty. A line of another day
    /// shows its date (`10-14 20:32:21`).
    pub fn load_earlier(&mut self) {
        let Some(path) = &self.log_file else { return };
        let Some(tail) = read_tail(path, EARLIER_BYTES) else {
            return;
        };
        let rows: Vec<&str> = tail.lines().filter(|line| !line.trim().is_empty()).collect();
        let mut earlier: Vec<LogLine> = rows[rows.len().saturating_sub(EARLIER_LINES)..]
            .iter()
            .map(|line| {
                // `YYYY-MM-DD HH:MM:SS  text`, or without the date (before
                // 2026-10-15), or `  text` without a time.
                let (date, rest) = match line.get(..11) {
                    Some(head) if head.as_bytes()[4] == b'-' && head.ends_with(' ') => (&head[..10], &line[11..]),
                    _ => ("", *line),
                };
                let (time, text) = rest.split_once("  ").unwrap_or(("", rest));
                let time = if date.is_empty() || date == self.today {
                    time.to_owned()
                } else {
                    format!("{} {time}", &date[5..])
                };
                LogLine {
                    time,
                    text: text.to_owned(),
                    earlier: true,
                }
            })
            .collect();
        if earlier.is_empty() {
            return;
        }
        earlier.push(LogLine {
            time: String::new(),
            text: "── earlier runs ──".to_owned(),
            earlier: true,
        });
        earlier.append(&mut self.lines);
        self.lines = earlier;
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
                // The date too, for the lines read back at a later start.
                if self.today.is_empty() || time.is_empty() {
                    let _ = writeln!(file, "{time}  {text}");
                } else {
                    let _ = writeln!(file, "{} {time}  {text}", self.today);
                }
            }
        }
        self.lines.push(LogLine {
            time: time.to_owned(),
            text: text.to_owned(),
            earlier: false,
        });
        // Scrolled back, the lines read stay in place; the highlight stays
        // on its line.
        if self.log_scroll > 0 {
            self.log_scroll += 1;
            self.log_unread += 1;
        }
        if let Some(at) = self.log_cursor.as_mut() {
            *at += 1;
        }
        if self.lines.len() > LOG_LIMIT {
            self.lines.remove(0);
            self.log_cursor = self.log_cursor.map(|at| at.min(LOG_LIMIT - 1));
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

    /// The groups the list can show: the pools' IDs and names (their
    /// comments in lower case).
    #[must_use]
    pub fn groups(&self) -> Vec<(String, String)> {
        self.status
            .iter()
            .flat_map(|status| &status.pools)
            .map(|pool| {
                let name = pool.comment.as_deref().filter(|c| !c.is_empty()).unwrap_or(&pool.id);
                (pool.id.clone(), name.to_lowercase())
            })
            .collect()
    }

    /// The group shown: `⭐` (the favorites), `all` or a pool's name.
    #[must_use]
    pub fn group_name(&self) -> String {
        if self.starred {
            return "⭐".to_owned();
        }
        let groups = self.groups();
        self.group
            .as_ref()
            .and_then(|id| groups.iter().find(|(pool, _)| pool == id))
            .map_or_else(|| "all".to_owned(), |(_, name)| name.clone())
    }

    /// The workspaces the list shows: the filter and the node applied.
    #[must_use]
    pub fn rows(&self) -> Vec<&StatusEntry> {
        let Some(status) = &self.status else {
            return Vec::new();
        };
        let node = self.node_name();
        let filter = self.filter.to_lowercase();
        // Everything of the HomeStack pools: Gold, the workspaces and the
        // others, by the chosen keys and then by VMID.
        let mut entries: Vec<&StatusEntry> = status
            .gold
            .iter()
            .chain(&status.workspaces)
            .chain(&status.others)
            .collect();
        entries.sort_by(|a, b| {
            self.sort
                .iter()
                .fold(std::cmp::Ordering::Equal, |order, key| {
                    order.then_with(|| compare(SORT_KEYS[*key], a, b))
                })
                .then(a.vmid.cmp(&b.vmid))
        });
        entries
            .into_iter()
            .filter(|entry| !self.starred || self.favorites.contains(&entry.vmid))
            .filter(|entry| {
                self.group
                    .as_ref()
                    .is_none_or(|group| entry.pool.as_ref() == Some(group))
            })
            .filter(|entry| node.as_ref().is_none_or(|node| &entry.node == node))
            .filter(|entry| match self.power {
                Power::All => true,
                Power::Online => entry.status == "running",
                Power::Offline => entry.status != "running",
            })
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
        let entry = self.selected_entry()?;
        if self.details_unavailable(entry).is_some() {
            return None;
        }
        self.details
            .as_ref()
            .filter(|details| details.vmid == entry.vmid)?
            .result
            .as_ref()
            .ok()
    }

    /// Drop a selection the list no longer shows.
    /// A selection the list no longer shows moves to its first row; the
    /// first row is selected from the start (the owner, 2026-10-15).
    fn keep_selection(&mut self) {
        if self.selected_index().is_none() {
            self.selected = self.rows().first().map(|entry| entry.vmid);
            self.info = false;
            if matches!(self.overlay, Some(Overlay::ReadDetails { .. })) {
                self.overlay = None;
            }
        }
    }

    fn select_at(&mut self, index: usize) {
        if let Some(entry) = self.rows().get(index) {
            self.selected = Some(entry.vmid);
        }
    }

    fn move_by(&mut self, step: isize) {
        let count = self.rows().len();
        if count == 0 {
            return;
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
        let Some(install) = self
            .install
            .as_ref()
            .filter(|install| install.mode != super::install::Mode::Service)
        else {
            return;
        };
        let mut table = self.preferences.table.clone();
        let method = match install.method {
            super::install::Method::Ssh => {
                table.insert("install_host".to_owned(), toml::Value::String(install.host.clone()));
                "ssh"
            }
            super::install::Method::Herdr => {
                if let Some((name, _)) = install.selected_tab() {
                    table.insert("install_tab".to_owned(), toml::Value::String(name.clone()));
                }
                table.insert(
                    "install_tab_filter".to_owned(),
                    toml::Value::String(install.tab_filter.clone()),
                );
                "herdr"
            }
        };
        table.insert("install_method".to_owned(), toml::Value::String(method.to_owned()));
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => {
                let time = self.clock.clone();
                self.log(&time, &error.0);
            }
        }
    }

    /// `f`: star the selection, or unstar it; kept in `tui.toml`.
    fn toggle_favorite(&mut self) {
        let Some(vmid) = self.selected_entry().map(|entry| entry.vmid) else {
            return;
        };
        if !self.favorites.remove(&vmid) {
            self.favorites.insert(vmid);
        }
        self.save_favorites();
        self.keep_selection();
    }

    fn save_favorites(&mut self) {
        let mut table = self.preferences.table.clone();
        table.insert(
            "favorites".to_owned(),
            toml::Value::Array(
                self.favorites
                    .iter()
                    .map(|vmid| toml::Value::Integer(i64::from(*vmid)))
                    .collect(),
            ),
        );
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => self.log("", &error.0),
        }
    }

    /// Whether the list shows the column `key` of [`COLUMNS`].
    #[must_use]
    pub fn column_shown(&self, key: &str) -> bool {
        COLUMNS
            .iter()
            .position(|(column, _)| *column == key)
            .is_some_and(|at| self.columns[at])
    }

    /// Show or hide the column `index` of [`COLUMNS`], kept in `tui.toml`.
    fn toggle_column(&mut self, index: usize) {
        let Some(shown) = self.columns.get_mut(index) else {
            return;
        };
        *shown = !*shown;
        let mut table = self.preferences.table.clone();
        table.insert(
            "columns".to_owned(),
            toml::Value::Array(
                COLUMNS
                    .iter()
                    .zip(self.columns)
                    .filter(|(_, shown)| *shown)
                    .map(|((key, _), _)| toml::Value::String((*key).to_owned()))
                    .collect(),
            ),
        );
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => self.log("", &error.0),
        }
    }

    /// Read `parallel` Proxmox requests at a time, kept in `tui.toml`.
    fn save_parallel(&mut self, parallel: usize) {
        PARALLEL.store(parallel, std::sync::atomic::Ordering::SeqCst);
        let mut table = self.preferences.table.clone();
        table.insert(
            "parallel".to_owned(),
            toml::Value::Integer(i64::try_from(parallel).unwrap_or(8)),
        );
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => self.log("", &error.0),
        }
    }

    /// Sort the machine list's `level` row by `key` of [`SORT_KEYS`], kept
    /// in `tui.toml`. The key the other row had swaps rows instead, so the
    /// two keys never repeat.
    fn save_sort(&mut self, level: usize, key: usize) {
        let other = 1 - level;
        if self.sort[other] == key {
            self.sort[other] = self.sort[level];
        }
        self.sort[level] = key;
        let mut table = self.preferences.table.clone();
        let mut sort = table
            .get("sort")
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();
        sort.insert(
            "machines".to_owned(),
            toml::Value::Array(
                self.sort
                    .iter()
                    .map(|key| toml::Value::String(SORT_KEYS[*key].to_owned()))
                    .collect(),
            ),
        );
        table.insert("sort".to_owned(), toml::Value::Table(sort));
        match self.preferences.write(&table) {
            Ok(()) => self.preferences.table = table,
            Err(error) => self.log("", &error.0),
        }
    }

    fn toggle_ssh_sounds(&mut self) {
        let path = self.preferences.path.with_file_name("client.toml");
        match crate::config::save_ssh_sounds(&path, !self.ssh_sounds) {
            Ok(()) => self.ssh_sounds = !self.ssh_sounds,
            Err(error) => {
                self.overlay = Some(Overlay::Report {
                    title: "SSH sounds".to_owned(),
                    lines: vec![error.0],
                });
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

    /// Scroll what `bar` scrolls to `offset`. A list scrolled by its
    /// selection moves the selection: the bottom row shown when scrolled.
    fn scroll_to(&mut self, bar: super::chrome::Bar, offset: usize) -> Option<Request> {
        use super::chrome::Scrolls;
        // The row a list scrolled to `offset` selects, if it must move.
        let row = |at: Option<usize>| {
            if offset > 0 {
                Some(offset + bar.visible - 1)
            } else {
                at.filter(|at| *at >= bar.visible).map(|_| bar.visible - 1)
            }
        };
        match bar.scrolls {
            Scrolls::Storage => self.storage_scroll.set(offset),
            // The highlight moves with the view, onto the nearest line shown.
            Scrolls::Log => {
                self.log_scroll = offset;
                self.log_cursor = self
                    .log_cursor
                    .map(|at| at.clamp(offset, offset + bar.visible.max(1) - 1));
            }
            Scrolls::List => {
                if let Some(index) = row(self.selected_index()) {
                    self.select_at(index);
                }
            }
            Scrolls::Action => {
                if let Some(action) = self.action.as_mut()
                    && let super::action::Stage::Plan { scroll, .. } | super::action::Stage::Done { scroll, .. } =
                        &mut action.stage
                {
                    *scroll = offset;
                }
            }
            Scrolls::SetupTree => {
                if let Some(setup) = self.setup.as_mut()
                    && matches!(setup.stage, super::setup::Stage::Tree)
                {
                    let rows = setup.rows();
                    let at = rows.iter().position(|row| row.id == setup.cursor);
                    if let Some(id) = row(at).and_then(|index| rows.get(index)).map(|row| row.id.clone()) {
                        setup.highlight(&id);
                    }
                }
            }
            Scrolls::SetupDetails => {
                if let Some(setup) = self.setup.as_mut() {
                    setup.details_scroll = offset;
                }
            }
            Scrolls::SetupReview => {
                if let Some(setup) = self.setup.as_mut()
                    && let super::setup::Stage::Review { scroll, .. } = &mut setup.stage
                {
                    *scroll = offset;
                }
            }
            Scrolls::InstallReview => {
                if let Some(install) = self.install.as_mut() {
                    install.scroll = offset;
                }
            }
        }
        None
    }

    /// A mouse event: a click on a workspace selects it, the wheel moves the
    /// selection; a scrollbar is pressed, held and dragged.
    pub fn mouse(&mut self, event: crossterm::event::MouseEvent) -> Option<Request> {
        let request = self.mouse_event(event);
        // The new lines the log was scrolled up to are read.
        self.log_unread = self.log_unread.min(self.log_scroll);
        request
    }

    fn mouse_event(&mut self, event: crossterm::event::MouseEvent) -> Option<Request> {
        use super::chrome::{Carousel, Hit};
        use crossterm::event::{MouseButton, MouseEventKind};
        let press = |app: &mut Self, code: KeyCode| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        let in_box = self.install.is_some() || self.overlay.is_some() || self.asking.is_some() || self.setup.is_some();
        // A scrollbar's thumb held: the drag moves it wherever the pointer
        // goes, until the button is let go.
        match event.kind {
            MouseEventKind::Drag(MouseButton::Left) if self.dragging.is_some() => {
                let (bar, grip) = self.dragging?;
                return self.scroll_to(bar, bar.drag(event.row, grip));
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging.is_some() => {
                self.dragging = None;
                return None;
            }
            _ => {}
        }
        // A scrollbar and the log under no box drawn over them: a press on
        // the bar, the wheel over it or over the log.
        let hit = if self.overlay.is_none() && self.asking.is_none() {
            super::chrome::hit_at(event.column, event.row)
        } else {
            None
        };
        match (event.kind, hit) {
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Bar(bar))) => {
                let (offset, grip) = bar.press(event.row);
                self.dragging = grip.map(|grip| (bar, grip));
                return self.scroll_to(bar, offset);
            }
            (MouseEventKind::ScrollDown, Some(Hit::Bar(bar))) => return self.scroll_to(bar, bar.wheel(3)),
            (MouseEventKind::ScrollUp, Some(Hit::Bar(bar))) => return self.scroll_to(bar, bar.wheel(-3)),
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, Some(Hit::ActionDetails { offset, most })) => {
                if let Some(action) = self.action.as_mut()
                    && let super::action::Stage::Plan { scroll, .. } | super::action::Stage::Done { scroll, .. } =
                        &mut action.stage
                {
                    let step = if event.kind == MouseEventKind::ScrollDown {
                        3
                    } else {
                        -3
                    };
                    *scroll = offset.saturating_add_signed(step).min(most);
                }
                return None;
            }
            // A line of the log clicked: it is highlighted.
            (MouseEventKind::Down(MouseButton::Left), Some(Hit::Log { line: Some(line), .. })) => {
                self.log_cursor = Some(line);
                return None;
            }
            // The wheel over the log moves the highlight a line and the view
            // with it; with no highlight it scrolls three lines.
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, Some(Hit::Log { most, visible, .. })) => {
                let down = event.kind == MouseEventKind::ScrollDown;
                match self.log_cursor {
                    Some(at) => {
                        let at = if down {
                            (at + 1).min(self.lines.len().saturating_sub(1))
                        } else {
                            at.saturating_sub(1)
                        };
                        self.log_cursor = Some(at);
                        let scroll = self.log_scroll.min(most);
                        self.log_scroll = scroll.clamp((at + 1).saturating_sub(visible.max(1)), at).min(most);
                    }
                    None if down => self.log_scroll = (self.log_scroll + 3).min(most),
                    None => self.log_scroll = self.log_scroll.min(most).saturating_sub(3),
                }
                return None;
            }
            _ => {}
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A button in a border is its key; an item of the menu or
                // the themes is chosen and opened.
                match super::chrome::hit_at(event.column, event.row) {
                    Some(Hit::Key(code)) => return press(self, code),
                    Some(Hit::Turn(Carousel::Power, forward)) => {
                        self.power = self.power.turn(forward);
                        self.keep_selection();
                        return None;
                    }
                    Some(Hit::Turn(carousel, forward)) => {
                        self.on_group = carousel == Carousel::Group;
                        return press(self, if forward { KeyCode::Right } else { KeyCode::Left });
                    }
                    // A setting of the install, and a choice in its box.
                    Some(Hit::Setting(row)) => {
                        if let Some(install) = self.install.as_mut() {
                            install.row = row;
                            install.chooser = false;
                            install.before = None;
                            install.adding = None;
                        }
                        return None;
                    }
                    Some(Hit::Choice(choice)) => {
                        let mut list = false;
                        if let Some(install) = self.install.as_mut() {
                            install.begin_edit();
                            install.cursor = choice;
                            list = install.settings.get(install.row).is_some_and(|setting| setting.list);
                        }
                        // A list's entry is highlighted; a choice is ticked.
                        return if list { None } else { press(self, KeyCode::Char(' ')) };
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
                    // A part of an amount: chosen; its - or + a step of it.
                    Some(Hit::Amount(field, part, delta)) => {
                        self.choose_part(field, Some(part));
                        return match delta {
                            -1 => press(self, KeyCode::Left),
                            1 => press(self, KeyCode::Right),
                            _ => None,
                        };
                    }
                    Some(Hit::SetupRow(index, part)) => return self.setup_click(index, part),
                    Some(Hit::SetupDetails | Hit::ActionDetails { .. }) => return None,
                    Some(Hit::ManifestSection(index)) => {
                        if let Some(setup) = self.setup.as_mut() {
                            setup.toggle_manifest_section(index);
                        }
                        return None;
                    }
                    Some(Hit::Log { .. } | Hit::Bar(_)) => return None,
                    Some(Hit::SystemOption(index)) => {
                        self.overlay = Some(Overlay::System { index });
                        return press(self, KeyCode::Enter);
                    }
                    Some(Hit::GeneralOption(level)) => {
                        self.overlay = Some(Overlay::General { index: level });
                        return press(self, KeyCode::Enter);
                    }
                    Some(Hit::SortKey(item)) => {
                        if let Some(Overlay::Sort { level, .. }) = self.overlay {
                            self.save_sort(level, item);
                            self.overlay = Some(Overlay::General { index: level });
                        }
                        return None;
                    }
                    Some(Hit::Parallel(item)) => {
                        if let Some(parallel) = PARALLEL_CHOICES.get(item) {
                            self.save_parallel(*parallel);
                        }
                        self.overlay = Some(Overlay::Settings { section: SYSTEM });
                        return None;
                    }
                    Some(Hit::Column(item)) => {
                        self.overlay = Some(Overlay::Columns { index: item });
                        self.toggle_column(item);
                        return None;
                    }
                    Some(Hit::Integration(item)) => {
                        self.overlay = Some(Overlay::Integrations { index: item });
                        return press(self, KeyCode::Enter);
                    }
                    Some(Hit::PangolinTunnel(control)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut()
                            && wizard.selected_step == 4
                            && !wizard.help_open
                            && !wizard.report_open
                        {
                            wizard.focus = control;
                            return press(self, if control == 1 { KeyCode::F(6) } else { KeyCode::F(5) });
                        }
                        return None;
                    }
                    Some(Hit::PangolinPermanent(control)) => {
                        if let Some(Overlay::Pangolin(w)) = self.overlay.as_mut()
                            && w.selected_step == 6
                            && !w.help_open
                            && !w.report_open
                            && w.permanent.running == 0
                        {
                            if w.permanent.confirm {
                                w.permanent.confirm_yes = control == 9;
                            } else {
                                w.focus = control;
                            }
                            w.permanent.replace = control == 2;
                            return press(self, KeyCode::Enter);
                        }
                        return None;
                    }
                    Some(Hit::PangolinBootstrap(control)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut()
                            && wizard.selected_step == 5
                            && wizard.bootstrap_available()
                            && wizard.bootstrap.running == 0
                            && !wizard.help_open
                            && !wizard.report_open
                        {
                            wizard.focus = control;
                            if control >= 3 {
                                return press(self, if control == 3 { KeyCode::F(6) } else { KeyCode::F(5) });
                            }
                        }
                        return None;
                    }
                    Some(Hit::PangolinApi(control)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut() {
                            if wizard.api_confirm {
                                wizard.api_confirm_yes = control == 5;
                                return press(self, KeyCode::Enter);
                            }
                            if wizard.selected_step == 3 && !wizard.help_open && !wizard.report_open {
                                wizard.focus = control;
                                return press(
                                    self,
                                    match control {
                                        1 => KeyCode::F(6),
                                        2 => KeyCode::F(7),
                                        _ => KeyCode::F(5),
                                    },
                                );
                            }
                        }
                        return None;
                    }
                    Some(Hit::PangolinDiscovery(control)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut()
                            && wizard.selected_step == 2
                            && wizard.discovery_available()
                            && !wizard.help_open
                            && !wizard.report_open
                        {
                            wizard.focus = control;
                            return press(self, if control == 1 { KeyCode::F(6) } else { KeyCode::F(7) });
                        }
                        return None;
                    }
                    Some(Hit::PangolinPrerequisite(control)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut() {
                            wizard.prerequisite_control(control);
                        }
                        return None;
                    }
                    Some(Hit::PangolinStep(step)) => {
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut() {
                            wizard.select_step(step);
                        }
                        return None;
                    }
                    Some(Hit::IntegrationField(field)) => {
                        if let Some(Overlay::Integration(editor)) = self.overlay.as_mut() {
                            editor.select(field);
                        }
                        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut()
                            && !wizard.help_open
                            && !wizard.report_open
                            && wizard.selected_step == 0
                        {
                            wizard.focus = field.min(2);
                        }
                        return None;
                    }
                    Some(Hit::Theme(item)) => {
                        let previous = match self.overlay.take() {
                            Some(Overlay::Themes { previous, .. }) => previous,
                            Some(Overlay::Settings { .. }) => Box::new(self.palette.clone()),
                            other => {
                                self.overlay = other;
                                return None;
                            }
                        };
                        self.overlay = Some(Overlay::Themes { index: item, previous });
                        self.preview(item);
                        return press(self, KeyCode::Enter);
                    }
                    Some(Hit::Item(item)) => {
                        match self.overlay.take() {
                            Some(Overlay::Menu { .. }) => {
                                self.overlay = Some(Overlay::Menu { index: item });
                                return press(self, KeyCode::Enter);
                            }
                            // A section of the settings is chosen from wherever the
                            // settings are; its choices are clicked in turn. A theme
                            // being previewed is given up, as with Esc.
                            Some(
                                overlay @ (Overlay::Settings { .. }
                                | Overlay::Integrations { .. }
                                | Overlay::Columns { .. }
                                | Overlay::Parallel { .. }
                                | Overlay::System { .. }
                                | Overlay::General { .. }
                                | Overlay::Sort { .. }
                                | Overlay::Themes { .. }),
                            ) => {
                                if let Overlay::Themes { previous, .. } = overlay {
                                    self.palette = *previous;
                                }
                                self.overlay = Some(Overlay::Settings {
                                    section: item.min(SETTINGS.len() - 1),
                                });
                                return None;
                            }
                            other => {
                                self.overlay = other;
                                return None;
                            }
                        }
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
                self.select_at(offset + usize::from(event.row - first));
                None
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                if self.setup.is_some()
                    && matches!(
                        super::chrome::hit_at(event.column, event.row),
                        Some(Hit::SetupDetails | Hit::ManifestSection(_))
                    ) =>
            {
                if let Some(setup) = self.setup.as_mut() {
                    setup.details_scroll = if event.kind == MouseEventKind::ScrollDown {
                        setup.details_scroll + 3
                    } else {
                        setup.details_scroll.saturating_sub(3)
                    };
                }
                None
            }
            // The wheel over an amount steps it: the part under the pointer,
            // or the chosen one.
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                if let Some((field, part)) = self.amount_at(event.column, event.row) =>
            {
                self.choose_part(field, part);
                let code = if event.kind == MouseEventKind::ScrollUp {
                    KeyCode::Right
                } else {
                    KeyCode::Left
                };
                press(self, code)
            }
            MouseEventKind::ScrollDown if in_box => press(self, KeyCode::Down),
            MouseEventKind::ScrollUp if in_box => press(self, KeyCode::Up),
            MouseEventKind::ScrollDown => {
                self.move_by(1);
                None
            }
            MouseEventKind::ScrollUp => {
                self.move_by(-1);
                None
            }
            _ => None,
        }
    }

    /// The amount of the open form at `x`, `y`, and its part there.
    fn amount_at(&self, x: u16, y: u16) -> Option<(usize, Option<super::action::Part>)> {
        let Some(super::action::Action {
            stage: super::action::Stage::Form(form),
            ..
        }) = &self.action
        else {
            return None;
        };
        let (field, part) = match super::chrome::hit_at(x, y)? {
            super::chrome::Hit::Amount(field, part, _) => (field, Some(part)),
            super::chrome::Hit::Field(field, _) => (field, None),
            _ => return None,
        };
        matches!(form.fields.get(field)?.input, super::action::Input::Amount { .. }).then_some((field, part))
    }

    /// Focus field `field` of the open form and choose `part` of its amount.
    fn choose_part(&mut self, field: usize, part: Option<super::action::Part>) {
        if let Some(action) = self.action.as_mut()
            && let super::action::Stage::Form(form) = &mut action.stage
        {
            form.focus = field;
            if let Some(chosen) = part
                && let Some(super::action::Field {
                    input: super::action::Input::Amount { part, .. },
                    ..
                }) = form.fields.get_mut(field)
            {
                *part = chosen;
            }
        }
    }

    pub fn integration_test_result(&mut self, id: u64, report: super::integrations::TestReport) {
        let saved = report.save(self.preferences.path.parent().unwrap_or(std::path::Path::new(".")));
        if let Some(Overlay::Integration(editor)) = self.overlay.as_mut() {
            if editor.testing && editor.test_id == id {
                editor.testing = false;
                let mut errors: Vec<String> = saved.err().into_iter().collect();
                if report.passed
                    && editor.has_changes()
                    && let Err(error) = editor.save()
                {
                    errors.push(format!("Connection verified, but automatic save failed: {error}"));
                }
                editor.message = errors.join("; ");
                editor.report = Some(report);
                editor.report_open = true;
                editor.report_scroll = 0;
            } else if editor.provider == report.provider {
                editor.report = Some(report);
            }
        }
    }

    /// Handle one key.
    pub fn key(&mut self, key: KeyEvent) -> Option<Request> {
        // Ctrl+C and Ctrl+Q leave at once, wherever the user is (Ctrl+Q: the
        // owner, 2026-10-15; not in the help).
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'q')) {
            return Some(Request::Quit);
        }
        if key.code == KeyCode::F(3) {
            return Some(Request::CopyJson);
        }
        if let Some(asking) = self.asking.as_mut() {
            let touch = asking.kind == crate::askpass::Kind::Touch;
            let answer = asking.key(key.code)?;
            self.asking = None;
            // Esc on the touch is Esc on the connection under it: one way
            // to cancel, and the connection's own end.
            if !(touch && self.connecting()) {
                return Some(Request::Answer(answer));
            }
        }
        if let Some(install) = self.install.as_mut() {
            let before = install.stage;
            let sites = install.mode == super::install::Mode::Sites;
            let (close, request) = install.key(key.code);
            let after = install.stage;
            if close {
                self.install = None;
                if sites && let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut() {
                    wizard.reload_sites();
                    wizard.focus = 0;
                }
            }
            if before != after {
                let time = self.clock.clone();
                self.log(&time, &format!("install step: {after:?}"));
            }
            return request;
        }
        if let Some(Overlay::Pangolin(wizard)) = self.overlay.as_mut() {
            if wizard.selected_step == 6 && !wizard.help_open && !wizard.report_open {
                let was_confirm = wizard.permanent.confirm;
                let operation = wizard.permanent_action(key.code);
                if !was_confirm && wizard.permanent.confirm {
                    return None;
                }
                if let Some(operation) = operation {
                    return wizard.begin_permanent(operation).map(Request::PangolinPermanent);
                }
            }
            if !wizard.report_open && !wizard.help_open && !wizard.api_confirm {
                if wizard.selected_step == 7 {
                    if wizard.sites_summary().is_some() {
                        if key.code == KeyCode::Char('f') || (key.code == KeyCode::Enter && wizard.focus != 2) {
                            self.overlay = Some(Overlay::Integrations { index: 3 });
                            return None;
                        }
                        if key.code == KeyCode::F(6) || (key.code == KeyCode::Enter && wizard.focus == 2) {
                            return wizard.sites_available().then_some(Request::PangolinSites);
                        }
                    } else if matches!(key.code, KeyCode::Enter | KeyCode::Right | KeyCode::Tab | KeyCode::F(6)) {
                        return wizard.sites_available().then_some(Request::PangolinSites);
                    }
                }
                if key.code == KeyCode::Esc && wizard.tunnel.busy() {
                    return Some(Request::StopPangolinTunnel(true));
                }
                if wizard.selected_step == 5
                    && (key.code == KeyCode::F(6) || (key.code == KeyCode::Enter && wizard.focus == 3))
                {
                    return wizard.begin_bootstrap().map(Request::VerifyPangolinKey);
                }
                if wizard.selected_step == 4 {
                    if (key.code == KeyCode::F(5) || (key.code == KeyCode::Enter && wizard.focus == 2))
                        && wizard.tunnel.current.state == super::pangolin_tunnel::State::Connected
                    {
                        return Some(Request::StopPangolinTunnel(false));
                    }
                    if key.code == KeyCode::F(6) || (key.code == KeyCode::Enter && wizard.focus == 1) {
                        return wizard
                            .begin_tunnel()
                            .map(|(id, spec)| Request::PangolinTunnel(id, spec));
                    }
                }
            }
            if wizard.api_confirm && key.code == KeyCode::Enter && wizard.api_confirm_yes {
                let plan = wizard.api.as_ref().and_then(|a| a.plan.clone());
                let discovery = wizard.discovery.clone();
                if let (Some(plan), Some(discovery)) = (plan, discovery) {
                    return wizard
                        .begin_api(true)
                        .map(|id| Request::PangolinApi(id, wizard.target.clone(), discovery, Some(Box::new(plan))));
                }
                wizard.api_confirm = false;
                return None;
            }
            if !wizard.api_confirm
                && !wizard.report_open
                && !wizard.help_open
                && wizard.selected_step == 3
                && (key.code == KeyCode::F(6) || (wizard.focus == 1 && key.code == KeyCode::Enter))
            {
                if let Some(discovery) = wizard.discovery.clone() {
                    return wizard
                        .begin_api(false)
                        .map(|id| Request::PangolinApi(id, wizard.target.clone(), discovery, None));
                }
                return None;
            }
            if !wizard.report_open
                && !wizard.help_open
                && wizard.selected_step == 2
                && (key.code == KeyCode::F(6) || (wizard.focus == 1 && key.code == KeyCode::Enter))
            {
                return wizard
                    .begin_discovery()
                    .map(|id| Request::DiscoverPangolin(id, wizard.target.clone()));
            }
            if !wizard.report_open
                && !wizard.help_open
                && wizard.selected_step == 0
                && (key.code == KeyCode::F(6)
                    || (wizard.focus == 2 && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))))
            {
                return wizard
                    .begin_test()
                    .map(|id| Request::TestPangolin(id, wizard.target.clone()));
            }
            if wizard.key(key) {
                self.overlay = Some(Overlay::Integrations { index: 3 });
            }
            return None;
        }
        if let Some(Overlay::Integration(editor)) = self.overlay.as_mut() {
            if editor.report_open {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::F(7) => editor.report_open = false,
                    KeyCode::Down => editor.report_scroll = editor.report_scroll.saturating_add(1),
                    KeyCode::PageDown => editor.report_scroll = editor.report_scroll.saturating_add(8),
                    KeyCode::Up => editor.report_scroll = editor.report_scroll.saturating_sub(1),
                    KeyCode::PageUp => editor.report_scroll = editor.report_scroll.saturating_sub(8),
                    KeyCode::Home => editor.report_scroll = 0,
                    _ => {}
                }
                return None;
            }
            if key.code == KeyCode::F(7) {
                editor.report_open = editor.report.is_some();
                editor.report_scroll = 0;
                return None;
            }
            if key.code == KeyCode::F(6)
                || (editor.on_test_button() && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')))
            {
                let id = editor.begin_test();
                return id.map(|id| Request::TestIntegration(id, editor.clone()));
            }
            if key.code == KeyCode::F(5) {
                editor.key(key);
                return None;
            }
            let close = editor.key(key);
            let provider = editor.provider;
            if close {
                self.overlay = Some(Overlay::Integrations { index: provider });
            }
            return None;
        }
        if let Some(overlay) = self.overlay.take() {
            return self.overlay_key(overlay, key.code);
        }
        if self.setup.is_some() {
            return self.setup_key(key);
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
        if self.shown.workspaces {
            if let KeyCode::Char(c @ ('n' | 'r' | 'z' | 'k' | 'g' | 'd' | 't')) = key.code {
                return self.open_action(c);
            }
            if key.code == KeyCode::Char('e') {
                return self.open_setup();
            }
            if key.code == KeyCode::Char('s') {
                return self
                    .selected_entry()
                    .filter(|entry| super::shell::available(self, entry.vmid))
                    .map(|entry| Request::Ssh(entry.vmid));
            }
            if key.code == KeyCode::Enter
                && let Some(action) = self.action.as_mut()
            {
                action.hidden = false;
                return None;
            }
            // On a server container: Space marks it, Enter reinstalls it (or
            // every marked one).
            if key.code == KeyCode::Char(' ') {
                self.toggle_mark();
                return None;
            }
            if key.code == KeyCode::Enter {
                return if self.selected_entry().is_some_and(|entry| self.image_container(entry)) {
                    self.open_reinstall()
                } else {
                    self.open_setup()
                };
            }
        }
        match key.code {
            KeyCode::Char('q') => return Some(Request::Quit),
            KeyCode::Char('m') => self.overlay = Some(Overlay::Menu { index: 0 }),
            KeyCode::F(10) => self.overlay = Some(Overlay::Settings { section: 0 }),
            KeyCode::F(9) => return self.open_desktop_setup(true),
            KeyCode::Esc if self.view != 0 => {
                self.view = 0;
                self.shown = VIEWS[0].1;
                self.save_view();
            }
            KeyCode::Esc => self.overlay = Some(Overlay::Quit),
            KeyCode::Char(digit @ '1'..='4') => {
                let index = usize::try_from(digit.to_digit(10).unwrap_or(0)).unwrap_or(0);
                let next = if self.view == index { 0 } else { index };
                if let Some((_, shown)) = VIEWS.get(next) {
                    self.view = next;
                    self.shown = *shown;
                    self.save_view();
                }
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
            KeyCode::Char('/') if self.shown.workspaces => self.filtering = true,
            KeyCode::Char('f') if self.shown.workspaces => self.toggle_favorite(),
            KeyCode::Char('o') if self.shown.workspaces => {
                self.power = self.power.turn(true);
                self.keep_selection();
            }
            KeyCode::Tab | KeyCode::BackTab if self.shown.workspaces => self.on_group = !self.on_group,
            KeyCode::Right | KeyCode::Left if self.shown.workspaces && self.on_group => {
                // Favorites, all, the pools by ID, favorites, …
                let groups = self.groups();
                let places = groups.len() + 2;
                let position = if self.starred {
                    0
                } else {
                    self.group
                        .as_ref()
                        .and_then(|id| groups.iter().position(|(pool, _)| pool == id))
                        .map_or(1, |index| index + 2)
                };
                let next = if key.code == KeyCode::Right {
                    (position + 1) % places
                } else {
                    (position + places - 1) % places
                };
                self.starred = next == 0;
                self.group = next
                    .checked_sub(2)
                    .and_then(|index| groups.get(index))
                    .map(|(id, _)| id.clone());
                self.keep_selection();
            }
            KeyCode::Right | KeyCode::Left if self.shown.workspaces => {
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
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home | KeyCode::End
                if matches!(self.view, 2 | 3) =>
            {
                let current = self.storage_scroll.get();
                let most = self.storage_scroll_max.get();
                let next = match key.code {
                    KeyCode::Down => current.saturating_add(1),
                    KeyCode::Up => current.saturating_sub(1),
                    KeyCode::PageDown => current.saturating_add(10),
                    KeyCode::PageUp => current.saturating_sub(10),
                    KeyCode::End => most,
                    _ => 0,
                };
                self.storage_scroll.set(next.min(most));
            }
            KeyCode::Down if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = (self.log_scroll + 1).min(self.lines.len().saturating_sub(1));
                self.log_unread = self.log_unread.min(self.log_scroll);
            }
            KeyCode::Up if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                self.log_unread = self.log_unread.min(self.log_scroll);
            }
            KeyCode::PageDown if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = (self.log_scroll + 10).min(self.lines.len().saturating_sub(1));
                self.log_unread = self.log_unread.min(self.log_scroll);
            }
            KeyCode::PageUp if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = self.log_scroll.saturating_sub(10);
                self.log_unread = self.log_unread.min(self.log_scroll);
            }
            KeyCode::Home if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = 0;
                self.log_unread = 0;
            }
            KeyCode::End if !self.shown.workspaces && self.shown.log => {
                self.log_scroll = self.lines.len().saturating_sub(1);
                self.log_unread = self.log_unread.min(self.log_scroll);
            }
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home => self.select_at(0),
            KeyCode::End => self.select_at(self.rows().len().saturating_sub(1)),
            // The details box opens on i only (as info), at every width (the
            // owner, 2026-10-15).
            KeyCode::Char('i') if self.shown.workspaces && self.selected.is_some() => {
                self.info = !self.info;
            }
            _ => {}
        }
        None
    }

    fn overlay_key(&mut self, overlay: Overlay, code: KeyCode) -> Option<Request> {
        match overlay {
            Overlay::Quit => match code {
                KeyCode::Enter => return Some(Request::Quit),
                KeyCode::Esc => {}
                _ => self.overlay = Some(Overlay::Quit),
            },
            Overlay::Menu { index } => match code {
                KeyCode::F(10) => self.overlay = Some(Overlay::Settings { section: 0 }),
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
                            lines: vec!["Asking every server".to_owned()],
                        });
                        return Some(Request::Transport);
                    }
                    1 => {
                        self.overlay = Some(Overlay::Report {
                            title: "this desktop".to_owned(),
                            lines: vec!["Checking this desktop".to_owned()],
                        });
                        return Some(Request::Discover);
                    }
                    2 => self.overlay = Some(Overlay::Settings { section: 0 }),
                    3 => self.overlay = Some(Overlay::Help),
                    4 => return self.open_desktop_setup(false),
                    5 => return Some(Request::UpdateApi),
                    _ => return Some(Request::Quit),
                },
                KeyCode::Char('q') => return Some(Request::Quit),
                KeyCode::Esc | KeyCode::Char('m') => {}
                _ => self.overlay = Some(Overlay::Menu { index }),
            },
            Overlay::Settings { section } => match code {
                KeyCode::Down => {
                    self.overlay = Some(Overlay::Settings {
                        section: (section + 1) % SETTINGS.len(),
                    });
                }
                KeyCode::Up => {
                    self.overlay = Some(Overlay::Settings {
                        section: (section + SETTINGS.len() - 1) % SETTINGS.len(),
                    });
                }
                KeyCode::Enter | KeyCode::Right if section == GENERAL => {
                    self.overlay = Some(Overlay::General { index: 0 });
                }
                KeyCode::Enter | KeyCode::Right if section == APPEARANCE => {
                    let current = THEMES.iter().position(|name| *name == self.palette.name).unwrap_or(0);
                    self.overlay = Some(Overlay::Themes {
                        index: current,
                        previous: Box::new(self.palette.clone()),
                    });
                }
                KeyCode::Enter | KeyCode::Right if section == MACHINES => {
                    self.overlay = Some(Overlay::Columns { index: 0 });
                }
                KeyCode::Enter | KeyCode::Right if section == SYSTEM => {
                    self.overlay = Some(Overlay::System { index: 0 });
                }
                KeyCode::Enter | KeyCode::Right if section == INTEGRATIONS => {
                    self.overlay = Some(Overlay::Integrations { index: 0 });
                }
                KeyCode::Esc => self.overlay = Some(Overlay::Menu { index: 2 }),
                _ => self.overlay = Some(Overlay::Settings { section }),
            },
            Overlay::Integration(_) | Overlay::Pangolin(_) => self.overlay = Some(overlay),
            Overlay::System { mut index } => {
                match code {
                    KeyCode::Down | KeyCode::Up | KeyCode::Tab => index = 1 - index,
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                        self.overlay = Some(Overlay::System { index });
                        if index == 1 {
                            self.toggle_ssh_sounds();
                        } else {
                            let current = PARALLEL.load(std::sync::atomic::Ordering::SeqCst);
                            self.overlay = Some(Overlay::Parallel {
                                index: PARALLEL_CHOICES
                                    .iter()
                                    .position(|choice| *choice == current)
                                    .unwrap_or(3),
                            });
                        }
                        return None;
                    }
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::Settings { section: SYSTEM });
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::System { index });
            }
            // The two sort levels, Reinstall, then Uninstall.
            Overlay::General { mut index } => {
                match code {
                    KeyCode::Down | KeyCode::Tab => index = (index + 1) % 4,
                    KeyCode::Up => index = (index + 3) % 4,
                    KeyCode::Enter if index == 2 => return self.open_install(super::install::Mode::Install, false),
                    KeyCode::Enter if index == 3 => return self.open_install(super::install::Mode::Uninstall, false),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') if index < 2 => {
                        self.overlay = Some(Overlay::Sort {
                            level: index,
                            index: self.sort[index],
                        });
                        return None;
                    }
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::Settings { section: GENERAL });
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::General { index });
            }
            Overlay::Sort { level, mut index } => {
                match code {
                    KeyCode::Down => index = (index + 1).min(SORT_KEYS.len() - 1),
                    KeyCode::Up => index = index.saturating_sub(1),
                    KeyCode::Enter => {
                        self.save_sort(level, index);
                        self.overlay = Some(Overlay::General { index: level });
                        return None;
                    }
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::General { index: level });
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Sort { level, index });
            }
            Overlay::Parallel { mut index } => {
                match code {
                    KeyCode::Down => index = (index + 1).min(PARALLEL_CHOICES.len() - 1),
                    KeyCode::Up => index = index.saturating_sub(1),
                    KeyCode::Enter => {
                        self.save_parallel(PARALLEL_CHOICES[index]);
                        self.overlay = Some(Overlay::System { index: 0 });
                        return None;
                    }
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::System { index: 0 });
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Parallel { index });
            }
            Overlay::Columns { mut index } => {
                match code {
                    KeyCode::Down => index = (index + 1) % COLUMNS.len(),
                    KeyCode::Up => index = (index + COLUMNS.len() - 1) % COLUMNS.len(),
                    KeyCode::Enter | KeyCode::Char(' ') => self.toggle_column(index),
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::Settings { section: MACHINES });
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Columns { index });
            }
            Overlay::Integrations { mut index } => {
                match code {
                    KeyCode::Down => index = (index + 1) % 4,
                    KeyCode::Up => index = (index + 3) % 4,
                    KeyCode::Esc | KeyCode::Left => {
                        self.overlay = Some(Overlay::Settings { section: INTEGRATIONS });
                        return None;
                    }
                    KeyCode::Enter | KeyCode::Right if index == 3 => {
                        let dir = self.preferences.path.parent().unwrap_or(std::path::Path::new("."));
                        self.overlay = Some(match super::pangolin::Wizard::open(dir) {
                            Ok(wizard) => Overlay::Pangolin(Box::new(wizard)),
                            Err(error) => Overlay::Report {
                                title: "Pangolin".to_owned(),
                                lines: vec![error],
                            },
                        });
                        return None;
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        let dir = self.preferences.path.parent().unwrap_or(std::path::Path::new("."));
                        match super::integrations::Editor::open(dir, index) {
                            Ok(editor) => self.overlay = Some(Overlay::Integration(Box::new(editor))),
                            Err(error) => {
                                self.overlay = Some(Overlay::Report {
                                    title: "Integration".to_owned(),
                                    lines: vec![error],
                                })
                            }
                        }
                        return None;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Integrations { index });
            }
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
                    self.overlay = Some(Overlay::Settings { section: APPEARANCE });
                }
                KeyCode::Esc => {
                    self.palette = *previous;
                    self.overlay = Some(Overlay::Settings { section: APPEARANCE });
                }
                _ => self.overlay = Some(Overlay::Themes { index, previous }),
            },
            Overlay::Help | Overlay::Report { .. } | Overlay::ReadDetails { .. } => match code {
                KeyCode::Esc | KeyCode::Enter => {}
                KeyCode::Char('m') => self.overlay = Some(Overlay::Menu { index: 0 }),
                KeyCode::Char('q') => return Some(Request::Quit),
                _ => self.overlay = Some(overlay),
            },
        }
        None
    }
}
