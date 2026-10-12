//! Where the Python CLI looks for its configuration.

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/homestack/config.toml`, else
/// `~/.config/homestack/config.toml`, as Python's `default_config_path`.
/// `None` when neither variable is usable.
pub fn default_config_path(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = var("HOME").filter(|home| !home.is_empty()).map(PathBuf::from);
    let base = match var("XDG_CONFIG_HOME").filter(|xdg| !xdg.is_empty()) {
        Some(xdg) => expand_home(&xdg, home.as_ref())?,
        None => home?.join(".config"),
    };
    Some(base.join("homestack").join("config.toml"))
}

/// `Path.expanduser` for `~` and `~/...`.
fn expand_home(path: &str, home: Option<&PathBuf>) -> Option<PathBuf> {
    if path == "~" {
        return home.cloned();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.map(|home| home.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}
