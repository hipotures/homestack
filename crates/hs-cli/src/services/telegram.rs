//! Local Telegram settings, delivered through ordinary application environment inputs.
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    pub bot_token: String,
    pub chat_id: String,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Telegram {
    notifications: Notifications,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    telegram: Telegram,
}

pub fn load() -> Result<Notifications, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("Cannot locate the HomeStack configuration directory")?;
    let path = base.join("homestack/telegram.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Notifications::default()),
        Err(_) => return Err(format!("Cannot read {}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).map_err(|_| format!("Invalid Telegram configuration in {}", path.display()))?;
    let settings = config.telegram.notifications;
    if settings.enabled
        && (settings.bot_token.trim().is_empty()
            || settings.chat_id.trim().is_empty()
            || settings.bot_token.chars().any(char::is_whitespace)
            || settings.chat_id.chars().any(char::is_whitespace))
    {
        return Err("Enabled Telegram notifications require bot_token and chat_id without whitespace".into());
    }
    Ok(settings)
}
