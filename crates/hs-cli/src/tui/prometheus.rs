//! The Prometheus the API reads container telemetry from, kept as
//! `[server] prometheus_url` in `deploy.toml` and passed to every API
//! instance it initializes.
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq)]
pub struct Editor {
    pub path: PathBuf,
    pub value: String,
    pub cursor: usize,
    pub message: String,
    saved: String,
    confirm_discard: bool,
}

/// The saved URL, or why there is none to edit.
pub fn current(dir: &Path) -> Result<Option<url::Url>, String> {
    let path = dir.join("deploy.toml");
    if !path.exists() {
        return Err("Install the HomeStack server first".to_owned());
    }
    hs_deploy::config::DeployConfig::load(&path).map(|config| config.server.prometheus_url)
}

pub fn summary(dir: &Path) -> String {
    match current(dir) {
        Ok(Some(url)) => url.as_str().trim_end_matches('/').to_owned(),
        Ok(None) => "Missing".to_owned(),
        Err(_) => "No server".to_owned(),
    }
}

impl Editor {
    pub fn open(dir: &Path) -> Result<Self, String> {
        let saved = current(dir)?
            .map(|url| url.as_str().trim_end_matches('/').to_owned())
            .unwrap_or_default();
        Ok(Self {
            path: dir.join("deploy.toml"),
            cursor: saved.chars().count(),
            value: saved.clone(),
            message: String::new(),
            saved,
            confirm_discard: false,
        })
    }

    pub fn has_changes(&self) -> bool {
        self.value != self.saved
    }

    pub fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let byte = self
            .value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(at, _)| at);
        self.value.insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.confirm_discard = false;
        self.message.clear();
    }

    /// Return true to leave. F5 saves; an empty field removes the URL.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc if !self.has_changes() || self.confirm_discard => return true,
            KeyCode::Esc => {
                self.confirm_discard = true;
                self.message = "Unsaved changes. Esc again to discard; F5 to save.".to_owned();
            }
            KeyCode::F(5) | KeyCode::Enter if self.has_changes() => {
                self.message = match self.save() {
                    Ok(()) => "Saved · API instances get it when initialized or updated".to_owned(),
                    Err(error) => error,
                };
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.value.clear();
                self.cursor = 0;
                self.message.clear();
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let mut chars: Vec<char> = self.value.chars().collect();
                let at = if key.code == KeyCode::Backspace {
                    self.cursor.checked_sub(1)
                } else {
                    Some(self.cursor)
                };
                if let Some(at) = at.filter(|at| *at < chars.len()) {
                    chars.remove(at);
                    self.cursor = self.cursor.min(at);
                    self.value = chars.into_iter().collect();
                    self.confirm_discard = false;
                    self.message.clear();
                }
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.paste(&c.to_string());
            }
            _ => {}
        }
        false
    }

    fn save(&mut self) -> Result<(), String> {
        let value = self.value.trim();
        let url = if value.is_empty() {
            None
        } else {
            Some(
                url::Url::parse(value)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
                    .ok_or("Enter a URL like http://prom.example:9090")?,
            )
        };
        hs_deploy::config::save_prometheus_url(&self.path, url.as_ref())?;
        self.value = value.to_owned();
        self.cursor = self.cursor.min(self.value.chars().count());
        self.saved = self.value.clone();
        self.confirm_discard = false;
        Ok(())
    }
}
