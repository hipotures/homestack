//! Configuration collected by the installer, before any cluster change.

use std::path::Path;

use hs_install::{Choice, Question};
use toml::{Table, Value};

use crate::AppError;

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
