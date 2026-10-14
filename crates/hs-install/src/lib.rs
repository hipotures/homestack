//! Installing and removing HomeStack in Proxmox VE from the desktop
//! (phase 13c, `docs/migration/phase-13c.md`): what `deploy/pve/homestack-pve.sh`
//! did on a node, driven through a root shell reached once over SSH or a
//! Herdr pane.
//!
//! The crate does no I/O of its own: every command to the node goes through
//! [`root::Root`], and the only thing written on the desktop is the secret
//! file of a new token.

pub mod apply;
pub mod plan;
pub mod probe;
pub mod root;
pub mod settings;
pub mod shell;

mod checks;
mod snippets;
mod state;

pub use apply::{Applied, apply};
pub use plan::{Action, Plan, Step, make_plan};
pub use probe::{Access, Node, Topology, probe};
pub use root::{LocalShell, Output, Root};
pub use settings::{Command, Settings};

/// Why an install or uninstall did not go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// One failure, shown as written.
    Failed(String),
    /// What belongs to the cluster's owner is not ready: nothing was
    /// changed.
    Checks { errors: Vec<String>, warnings: Vec<String> },
    /// Options the user has not given; each comes with what can be chosen.
    Missing(Vec<Question>),
}

/// One choice of a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What the option takes (a storage id, a VMID).
    pub value: String,
    /// What the user needs to tell it apart (type, nodes, name, state).
    pub detail: String,
}

/// An option the user has not given, with what can be chosen for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub option: &'static str,
    /// What is chosen, as a heading (capitalised, no full stop).
    pub title: String,
    /// A sentence more when the heading is not enough; may be empty.
    pub note: String,
    /// Several values, comma-separated, are allowed.
    pub multiple: bool,
    pub choices: Vec<Choice>,
}

/// The questions as the user reads them: the option and one choice a line.
#[must_use]
pub fn describe_missing(questions: &[Question]) -> String {
    const WIDTH: usize = 60;
    let mut out = String::from("Choose where to install; nothing was changed.\n");
    for question in questions {
        let hint = if question.multiple {
            " (several, comma-separated)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n{} is required: {}{hint}\n",
            question.option, question.title
        ));
        if !question.note.is_empty() {
            for line in wrap(&question.note, 88) {
                out.push_str(&format!("    {line}\n"));
            }
        }
        if question.choices.is_empty() {
            out.push_str("    (nothing to choose from: see the message above)\n");
        }
        let width = question
            .choices
            .iter()
            .map(|c| c.value.chars().count())
            .max()
            .unwrap_or(0);
        for choice in &question.choices {
            let mut detail: String = choice.detail.chars().take(WIDTH).collect();
            if choice.detail.chars().count() > WIDTH {
                detail.push_str("...");
            }
            out.push_str(&format!("    {:<width$}  {detail}\n", choice.value));
        }
    }
    out.trim_end().to_owned()
}

impl InstallError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) => f.write_str(message),
            Self::Missing(questions) => f.write_str(&describe_missing(questions)),
            Self::Checks { errors, warnings } => {
                for warning in warnings {
                    writeln!(f, "warning: {warning}")?;
                }
                for error in errors {
                    writeln!(f, "error: {error}")?;
                }
                f.write_str("nothing changed: the cluster's owner fixes the errors above first")
            }
        }
    }
}

impl std::error::Error for InstallError {}

/// `text` broken at spaces into lines of at most `width` characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}
