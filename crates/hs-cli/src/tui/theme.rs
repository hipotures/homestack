//! Themes of the TUI, as in MyPowers (`frontends/tui/src/client_ui.rs`): a
//! list to choose from, single colours overridden in `[colors]`, and the
//! choice kept in `tui.toml` next to `client.toml`. `HomeStack` follows the
//! owner's reference picture; the other names are the `ratcn` presets
//! MyPowers uses.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

use crate::AppError;

pub const THEMES: [&str; 7] = [
    "HomeStack",
    "Catppuccin",
    "Nord",
    "Gruvbox",
    "Tokyo Night",
    "Solarized",
    "Terminal",
];

/// The roles `[colors]` may override.
pub const ROLES: [&str; 14] = [
    "background",
    "surface",
    "foreground",
    "muted",
    "border",
    "focus",
    "accent",
    "value",
    "ok",
    "info",
    "warning",
    "error",
    "hotkey",
    "selection",
];

/// The colours the TUI draws with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub name: String,
    /// `NO_COLOR`: every colour is the terminal's own; emphasis is bold.
    pub no_color: bool,
    /// Behind everything.
    pub background: Color,
    /// Boxes over the screen (menu, the security key) and sub-boxes.
    pub surface: Color,
    pub foreground: Color,
    /// Secondary text, idle controls.
    pub muted: Color,
    pub border: Color,
    /// The focused panel, the active item, the raised step.
    pub focus: Color,
    pub accent: Color,
    /// Values next to labels (a storage name, an address).
    pub value: Color,
    /// Running, online, done.
    pub ok: Color,
    /// Work in progress.
    pub info: Color,
    pub warning: Color,
    pub error: Color,
    /// The key letter of a control, red as in btop.
    pub hotkey: Color,
    /// The fill of the selected row.
    pub selection: Color,
}

impl Palette {
    /// The theme `name`; with `no_color` every colour is [`Color::Reset`].
    pub fn named(name: &str, no_color: bool) -> Result<Self, AppError> {
        let mut palette = match name {
            "HomeStack" => homestack(),
            "Catppuccin" => from_ratcn(&ratcn::Theme::catppuccin(), Color::Rgb(166, 227, 161), None),
            "Nord" => from_ratcn(&ratcn::Theme::nord(), Color::Rgb(163, 190, 140), None),
            "Gruvbox" => from_ratcn(&ratcn::Theme::gruvbox(), Color::Rgb(184, 187, 38), None),
            "Tokyo Night" => from_ratcn(
                &ratcn::Theme::tokyo_night(),
                Color::Rgb(115, 218, 202),
                Some(Color::Rgb(125, 207, 255)),
            ),
            "Solarized" => from_ratcn(&ratcn::Theme::solarized(), Color::Rgb(133, 153, 0), None),
            "Terminal" => from_ratcn(&ratcn::Theme::terminal(), Color::Green, Some(Color::Cyan)),
            _ => return Err(unknown_theme(name)),
        };
        if no_color {
            palette.no_color = true;
            for role in ROLES {
                palette.set(role, Color::Reset);
            }
        }
        Ok(palette)
    }

    fn set(&mut self, role: &str, color: Color) {
        let slot = match role {
            "background" => &mut self.background,
            "surface" => &mut self.surface,
            "foreground" => &mut self.foreground,
            "muted" => &mut self.muted,
            "border" => &mut self.border,
            "focus" => &mut self.focus,
            "accent" => &mut self.accent,
            "value" => &mut self.value,
            "ok" => &mut self.ok,
            "info" => &mut self.info,
            "warning" => &mut self.warning,
            "error" => &mut self.error,
            "hotkey" => &mut self.hotkey,
            "selection" => &mut self.selection,
            _ => return,
        };
        *slot = color;
    }
}

/// The owner's reference picture: dark blue-grey, a blue accent, teal values.
fn homestack() -> Palette {
    Palette {
        name: "HomeStack".to_owned(),
        no_color: false,
        background: Color::Rgb(13, 17, 23),
        surface: Color::Rgb(20, 25, 35),
        foreground: Color::Rgb(200, 208, 224),
        muted: Color::Rgb(98, 110, 132),
        border: Color::Rgb(52, 62, 82),
        focus: Color::Rgb(122, 162, 247),
        accent: Color::Rgb(122, 162, 247),
        value: Color::Rgb(94, 212, 180),
        ok: Color::Rgb(94, 212, 180),
        info: Color::Rgb(125, 207, 255),
        warning: Color::Rgb(224, 190, 110),
        error: Color::Rgb(240, 98, 112),
        hotkey: Color::Rgb(240, 98, 112),
        selection: Color::Rgb(30, 38, 56),
    }
}

fn from_ratcn(theme: &ratcn::Theme, ok: Color, info: Option<Color>) -> Palette {
    Palette {
        name: theme.name.to_owned(),
        no_color: false,
        background: theme.background,
        surface: theme.surface,
        foreground: theme.foreground,
        muted: theme.muted_foreground,
        border: theme.border,
        focus: theme.primary,
        accent: theme.accent,
        value: ok,
        ok,
        info: info.unwrap_or(theme.primary),
        warning: theme.warning,
        error: theme.destructive,
        hotkey: theme.destructive,
        selection: theme.secondary,
    }
}

fn unknown_theme(name: &str) -> AppError {
    AppError::new(format!("Unknown theme {name:?}; the themes are: {}", THEMES.join(", ")))
}

/// `#RRGGBB`.
fn parse_color(text: &str) -> Option<Color> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// `tui.toml`: the theme, colour overrides and whatever else is kept there.
/// It is never sent to a server.
#[derive(Debug, Clone)]
pub struct Preferences {
    pub path: PathBuf,
    pub theme: String,
    pub colors: BTreeMap<String, Color>,
    /// The whole file, so a save keeps the keys it does not know.
    pub table: toml::Table,
}

impl Preferences {
    /// `tui.toml` next to `client.toml`.
    #[must_use]
    pub fn default_path(home: &Path, config_home: Option<&Path>) -> PathBuf {
        crate::config::default_path(home, config_home).with_file_name("tui.toml")
    }

    /// Read `path`; a missing file is the defaults.
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let table = match std::fs::read_to_string(path) {
            Ok(text) => text
                .parse::<toml::Table>()
                .map_err(|error| AppError::new(format!("{} is not valid TOML: {}", path.display(), error.message())))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(error) => return Err(AppError::new(format!("Cannot read {}: {error}", path.display()))),
        };
        let theme = match table.get("theme") {
            None => THEMES[0].to_owned(),
            Some(toml::Value::String(name)) if THEMES.contains(&name.as_str()) => name.clone(),
            Some(toml::Value::String(name)) => return Err(unknown_theme(name)),
            Some(_) => return Err(AppError::new(format!("theme in {} must be a name", path.display()))),
        };
        let mut colors = BTreeMap::new();
        match table.get("colors") {
            None => {}
            Some(toml::Value::Table(given)) => {
                for (role, value) in given {
                    if !ROLES.contains(&role.as_str()) {
                        return Err(AppError::new(format!(
                            "Unknown colour role {role:?} in {}; the roles are: {}",
                            path.display(),
                            ROLES.join(", ")
                        )));
                    }
                    let color = value
                        .as_str()
                        .and_then(parse_color)
                        .ok_or_else(|| AppError::new(format!("colors.{role} in {} must be #RRGGBB", path.display())))?;
                    colors.insert(role.clone(), color);
                }
            }
            Some(_) => return Err(AppError::new(format!("colors in {} must be a table", path.display()))),
        }
        Ok(Self {
            path: path.to_owned(),
            theme,
            colors,
            table,
        })
    }

    /// The chosen theme with the overrides.
    pub fn palette(&self, no_color: bool) -> Result<Palette, AppError> {
        let mut palette = Palette::named(&self.theme, no_color)?;
        if !no_color {
            for (role, color) in &self.colors {
                palette.set(role, *color);
            }
        }
        Ok(palette)
    }

    /// Choose `name` and write the file; the other keys stay.
    pub fn save_theme(&mut self, name: &str) -> Result<(), AppError> {
        if !THEMES.contains(&name) {
            return Err(unknown_theme(name));
        }
        let mut table = self.table.clone();
        table.insert("theme".to_owned(), toml::Value::String(name.to_owned()));
        self.write(&table)?;
        self.table = table;
        name.clone_into(&mut self.theme);
        Ok(())
    }

    /// Write `table` as MyPowers writes its preferences: a new private
    /// temporary file, renamed over the old one.
    pub fn write(&self, table: &toml::Table) -> Result<(), AppError> {
        let fail = |error: std::io::Error| AppError::new(format!("Cannot write {}: {error}", self.path.display()));
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(fail)?;
        let text = toml::to_string_pretty(table)
            .map_err(|error| AppError::new(format!("Cannot write {}: {error}", self.path.display())))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let temporary = parent.join(format!(".tui-{}-{nanos}.toml", std::process::id()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(fail)
    }
}

impl Preferences {
    /// The defaults for `path`, when the file there cannot be used.
    #[must_use]
    pub fn fallback(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            theme: THEMES[0].to_owned(),
            colors: BTreeMap::new(),
            table: toml::Table::new(),
        }
    }
}
