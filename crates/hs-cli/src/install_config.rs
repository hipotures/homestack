//! Configuration collected by the installer, before any cluster change.

use std::path::Path;

use hs_install::{Choice, Question};
use toml::{Table, Value};

use crate::AppError;

pub const KEY_OPTION: &str = "--workspace-keys";

const FIELDS: &[(&str, &str, &str, &str, &str)] = &[
    ("config-root-disk", "root", "disk", "Root disk slot", "scsi0"),
    ("config-home-disk", "home", "disk", "Home disk slot", "scsi1"),
    ("config-home-size", "home", "default_size", "Default home size", "20G"),
    ("config-network-prefix", "network", "prefix", "Workspace IP prefix", ""),
    ("config-network-cidr", "network", "cidr", "Network prefix length", "24"),
    ("config-network-gateway", "network", "gateway", "Network gateway", ""),
    ("config-network-dns", "network", "dns", "DNS servers", ""),
    ("config-user-name", "user", "name", "Workspace user", ""),
    ("config-user-uid", "user", "uid", "Workspace user UID", "1000"),
    ("config-user-gid", "user", "gid", "Workspace user GID", "1000"),
    (
        "config-snippet-dir",
        "cloud_init",
        "snippet_dir",
        "Snippet directory",
        "/var/lib/vz/snippets",
    ),
];

#[derive(Clone, Default)]
pub struct Input {
    document: Table,
}

impl Input {
    pub fn key_question(&self, home: &Path) -> Question {
        let configured: Vec<String> = self
            .document
            .get("workspace_ssh")
            .and_then(Value::as_table)
            .and_then(|table| table.get("identity_files"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let mut choices: Vec<Choice> = configured
            .iter()
            .map(|value| Choice {
                value: value.clone(),
                detail: "Configured identity".to_owned(),
            })
            .collect();
        let defaults = (0..choices.len()).collect();
        for (public, name) in crate::identity::public_keys(home) {
            let Ok((_, identity)) = crate::identity::private_key_for(home, Path::new(&public)) else {
                continue;
            };
            if let Some(existing) = choices
                .iter_mut()
                .find(|choice| crate::expand_home(&choice.value, home) == crate::expand_home(&identity, home))
            {
                existing.detail = name;
            } else {
                choices.push(Choice {
                    value: identity,
                    detail: name,
                });
            }
        }
        Question {
            option: KEY_OPTION, title: "Workspace SSH keys".to_owned(),
            note: "Select local identities used to log in to workspaces. Private keys stay on this desktop. This selection does not upload keys to Gold.".to_owned(),
            multiple: true, choices, defaults,
        }
    }

    pub fn identities(&mut self, identities: Vec<String>) {
        let table = self
            .document
            .entry("workspace_ssh")
            .or_insert_with(|| Value::Table(Table::new()));
        if let Some(table) = table.as_table_mut() {
            table.insert(
                "identity_files".into(),
                Value::Array(identities.into_iter().map(Value::String).collect()),
            );
        }
    }

    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(AppError::new(format!("{}: {}", path.display(), error.kind()))),
        };
        let document = text
            .parse()
            .map_err(|_| AppError::new(format!("{}: invalid TOML", path.display())))?;
        let mut input = Self { document };
        for (option, section, key, _, default) in FIELDS {
            if input
                .document
                .get(*section)
                .and_then(Value::as_table)
                .and_then(|table| table.get(*key))
                .is_none()
            {
                input.answer(option, default);
            }
        }
        Ok(input)
    }

    pub fn questions(&self) -> Vec<Question> {
        FIELDS
            .iter()
            .map(|(option, section, key, title, _)| {
                let value = self
                    .document
                    .get(*section)
                    .and_then(Value::as_table)
                    .and_then(|table| table.get(*key));
                let text = match value {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
                    Some(value) => value.to_string(),
                    None => String::new(),
                };
                Question {
                    option,
                    title: (*title).to_owned(),
                    note: if *section == "user" {
                        "The account inside Gold and workspace VMs, not the Proxmox or desktop account."
                    } else if *key == "dns" {
                        "IP addresses separated by commas."
                    } else {
                        "Type the value; Enter reviews all settings. Nothing changes before confirmation."
                    }
                    .to_owned(),
                    multiple: false,
                    choices: vec![Choice {
                        value: text,
                        detail: String::new(),
                    }],
                    defaults: vec![0],
                }
            })
            .collect()
    }

    pub fn answer(&mut self, option: &str, text: &str) {
        let Some((_, section, key, _, _)) = FIELDS.iter().find(|(name, ..)| *name == option) else {
            return;
        };
        let text = text.trim();
        let value = if matches!(*key, "cidr" | "uid" | "gid") {
            text.parse::<i64>()
                .map(Value::Integer)
                .unwrap_or_else(|_| Value::String(text.to_owned()))
        } else if *key == "dns" {
            Value::Array(
                text.split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| Value::String(value.to_owned()))
                    .collect(),
            )
        } else {
            Value::String(text.to_owned())
        };
        let table = self
            .document
            .entry((*section).to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        if let Some(table) = table.as_table_mut() {
            table.insert((*key).to_owned(), value);
        }
    }

    pub fn text(&self) -> Result<String, String> {
        toml::to_string(&self.document).map_err(|error| error.to_string())
    }
}

pub fn is_text(question: &Question) -> bool {
    question.option.starts_with("config-")
}

pub fn defaults(question: &Question) -> crate::install_flow::Answer {
    if is_text(question) {
        crate::install_flow::Answer::Text(
            question
                .choices
                .first()
                .map_or_else(String::new, |choice| choice.value.clone()),
        )
    } else {
        crate::install_flow::Answer::Picked(question.defaults.clone())
    }
}
