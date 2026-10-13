//! The command line of Python's `cli.build_parser`: the same commands and
//! options, help without a configuration, and exit code 2 on a usage error.

use std::path::PathBuf;

/// What was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// No command: the help screen.
    None,
    Discover,
    Install,
    Create {
        vmid: u32,
        name: String,
        home_size: Option<String>,
        storage: Option<String>,
        node: Option<String>,
    },
    Refresh {
        target: String,
    },
    Resize {
        target: String,
        root_size: Option<String>,
        home_size: Option<String>,
    },
    Addkey {
        target: String,
        key: Option<String>,
        restart: bool,
    },
    Destroy {
        target: String,
    },
    Migrate {
        target: String,
        target_node: String,
        target_storage: String,
    },
    Setup,
    Repo {
        target: String,
        repository: Option<String>,
    },
    Status {
        target: Option<String>,
    },
    Transport,
}

impl Command {
    /// The command's name in messages and `--json` (`"command"`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::None => "help",
            Self::Discover => "discover",
            Self::Install => "install",
            Self::Create { .. } => "create",
            Self::Refresh { .. } => "refresh",
            Self::Resize { .. } => "resize",
            Self::Addkey { .. } => "addkey",
            Self::Destroy { .. } => "destroy",
            Self::Migrate { .. } => "migrate",
            Self::Setup => "setup",
            Self::Repo { .. } => "repo",
            Self::Status { .. } => "status",
            Self::Transport => "transport",
        }
    }
}

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub help: bool,
    pub config: Option<PathBuf>,
    pub json: bool,
    pub yes: bool,
    pub command: Command,
}

/// A usage error (argparse's exit code 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    pub usage: String,
    pub message: String,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\nhomestack: error: {}", self.usage, self.message)
    }
}

impl std::error::Error for UsageError {}

const USAGE: &str = "usage: homestack [-h] [--config CONFIG] [--json] [-y] \
{discover,install,create,refresh,resize,addkey,destroy,migrate,setup,repo,status,transport} ...";

fn usage_of(command: &str) -> String {
    let rest = match command {
        "create" => "VMID NAME [--home-size HOME_SIZE] [--storage STORAGE] [--node NODE] [--json] [-y]",
        "refresh" | "destroy" => "TARGET [--json] [-y]",
        "resize" => "TARGET (--root-size ROOT_SIZE | --home-size HOME_SIZE) [--json] [-y]",
        "addkey" => "TARGET [--key KEY] [--restart] [--json] [-y]",
        "migrate" => "TARGET TARGET_NODE --target-storage TARGET_STORAGE [--json] [-y]",
        "repo" => "TARGET [REPOSITORY] [--json]",
        "status" => "[TARGET] [--json]",
        _ => "[--json]",
    };
    format!("usage: homestack {command} {rest}")
}

fn error(usage: String, message: impl Into<String>) -> UsageError {
    UsageError {
        usage,
        message: message.into(),
    }
}

/// The options and positionals after a command.
struct Words {
    usage: String,
    positionals: Vec<String>,
    values: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Words {
    fn value(&self, name: &str) -> Option<String> {
        self.values
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    /// Exactly `required` positionals, then up to `optional` more.
    fn positionals(&self, names: &[&str], optional: usize) -> Result<Vec<Option<String>>, UsageError> {
        let required = names.len() - optional;
        if self.positionals.len() < required {
            let missing: Vec<&str> = names[self.positionals.len()..required].to_vec();
            return Err(error(
                self.usage.clone(),
                format!("the following arguments are required: {}", missing.join(", ")),
            ));
        }
        if self.positionals.len() > names.len() {
            return Err(error(
                self.usage.clone(),
                format!("unrecognized arguments: {}", self.positionals[names.len()..].join(" ")),
            ));
        }
        Ok((0..names.len()).map(|i| self.positionals.get(i).cloned()).collect())
    }
}

/// Split `argv` (after the command) by the options the command takes.
fn words(command: &str, argv: &[String], valued: &[&str], flags: &[&str]) -> Result<Words, UsageError> {
    let usage = usage_of(command);
    let mut parsed = Words {
        usage: usage.clone(),
        positionals: Vec::new(),
        values: Vec::new(),
        flags: Vec::new(),
    };
    let mut rest = argv.iter();
    let mut only_positionals = false;
    while let Some(word) = rest.next() {
        if only_positionals || !word.starts_with('-') || word == "-" {
            parsed.positionals.push(word.clone());
            continue;
        }
        if word == "--" {
            only_positionals = true;
            continue;
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (word.as_str(), None),
        };
        let name = match name {
            "-y" => "--yes",
            "-h" => "--help",
            other => other,
        };
        if valued.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .filter(|value| !value.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| error(usage.clone(), format!("argument {name}: expected one argument")))?,
            };
            parsed.values.push((name.to_owned(), value));
        } else if flags.contains(&name) && inline.is_none() {
            parsed.flags.push(name.to_owned());
        } else {
            return Err(error(usage, format!("unrecognized arguments: {word}")));
        }
    }
    Ok(parsed)
}

const COMMON: [&str; 3] = ["--json", "--yes", "--help"];

fn common(extra: &[&'static str]) -> Vec<&'static str> {
    let mut flags = COMMON.to_vec();
    flags.extend_from_slice(extra);
    flags
}

/// Parse the command line (without the program name).
pub fn parse(argv: &[String]) -> Result<Args, UsageError> {
    let mut args = Args {
        help: false,
        config: None,
        json: false,
        yes: false,
        command: Command::None,
    };
    let mut index = 0;
    while let Some(word) = argv.get(index) {
        match word.as_str() {
            "-h" | "--help" => args.help = true,
            "--json" => args.json = true,
            "-y" | "--yes" => args.yes = true,
            "--config" => {
                index += 1;
                let value = argv
                    .get(index)
                    .ok_or_else(|| error(USAGE.to_owned(), "argument --config: expected one argument"))?;
                args.config = Some(PathBuf::from(value));
            }
            other if other.starts_with("--config=") => {
                args.config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other if other.starts_with('-') => {
                return Err(error(USAGE.to_owned(), format!("unrecognized arguments: {other}")));
            }
            _ => break,
        }
        index += 1;
    }
    let Some(name) = argv.get(index) else {
        return Ok(args);
    };
    let rest = &argv[index + 1..];
    let parsed = match name.as_str() {
        "discover" | "transport" | "install" => {
            let flags: &[&str] = if name == "install" {
                &["--help"]
            } else {
                &["--json", "--help"]
            };
            let words = words(name, rest, &[], flags)?;
            words.positionals(&[], 0)?;
            (
                words,
                match name.as_str() {
                    "discover" => Command::Discover,
                    "transport" => Command::Transport,
                    _ => Command::Install,
                },
            )
        }
        "create" => {
            let words = words(name, rest, &["--home-size", "--storage", "--node"], &COMMON)?;
            let found = words.positionals(&["vmid", "name"], 0)?;
            let vmid_text = found[0].clone().unwrap_or_default();
            let vmid = vmid_text.parse::<u32>().map_err(|_| {
                error(
                    words.usage.clone(),
                    format!("argument vmid: invalid int value: '{vmid_text}'"),
                )
            })?;
            let command = Command::Create {
                vmid,
                name: found[1].clone().unwrap_or_default(),
                home_size: words.value("--home-size"),
                storage: words.value("--storage"),
                node: words.value("--node"),
            };
            (words, command)
        }
        "refresh" | "destroy" => {
            let words = words(name, rest, &[], &COMMON)?;
            let target = words.positionals(&["target"], 0)?[0].clone().unwrap_or_default();
            let command = if name == "refresh" {
                Command::Refresh { target }
            } else {
                Command::Destroy { target }
            };
            (words, command)
        }
        "resize" => {
            let words = words(name, rest, &["--root-size", "--home-size"], &COMMON)?;
            let target = words.positionals(&["target"], 0)?[0].clone().unwrap_or_default();
            let root_size = words.value("--root-size");
            let home_size = words.value("--home-size");
            match (&root_size, &home_size) {
                (None, None) => {
                    return Err(error(
                        words.usage,
                        "one of the arguments --root-size --home-size is required",
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(error(
                        words.usage,
                        "argument --home-size: not allowed with argument --root-size",
                    ));
                }
                _ => {}
            }
            (
                words,
                Command::Resize {
                    target,
                    root_size,
                    home_size,
                },
            )
        }
        "addkey" => {
            let words = words(name, rest, &["--key"], &common(&["--restart"]))?;
            let target = words.positionals(&["target"], 0)?[0].clone().unwrap_or_default();
            let command = Command::Addkey {
                target,
                key: words.value("--key"),
                restart: words.flag("--restart"),
            };
            (words, command)
        }
        "migrate" => {
            let words = words(name, rest, &["--target-storage"], &COMMON)?;
            let found = words.positionals(&["target", "target_node"], 0)?;
            let target_storage = words.value("--target-storage").ok_or_else(|| {
                error(
                    words.usage.clone(),
                    "the following arguments are required: --target-storage",
                )
            })?;
            let command = Command::Migrate {
                target: found[0].clone().unwrap_or_default(),
                target_node: found[1].clone().unwrap_or_default(),
                target_storage,
            };
            (words, command)
        }
        "setup" => {
            // Still Python's (phase 14): taken as a whole.
            let help = rest.iter().any(|word| word == "-h" || word == "--help");
            let words = Words {
                usage: usage_of(name),
                positionals: Vec::new(),
                values: Vec::new(),
                flags: if help { vec!["--help".to_owned()] } else { Vec::new() },
            };
            (words, Command::Setup)
        }
        "repo" => {
            let words = words(name, rest, &[], &["--json", "--help"])?;
            let found = words.positionals(&["target", "repository"], 1)?;
            let command = Command::Repo {
                target: found[0].clone().unwrap_or_default(),
                repository: found[1].clone(),
            };
            (words, command)
        }
        "status" => {
            let words = words(name, rest, &[], &["--json", "--help"])?;
            let target = words.positionals(&["target"], 1)?[0].clone();
            (words, Command::Status { target })
        }
        other => {
            return Err(error(
                USAGE.to_owned(),
                format!(
                    "argument command: invalid choice: '{other}' (choose from discover, install, create, refresh, resize, addkey, destroy, migrate, setup, repo, status, transport)"
                ),
            ));
        }
    };
    let (words, command) = parsed;
    args.json |= words.flag("--json");
    args.yes |= words.flag("--yes");
    args.help |= words.flag("--help");
    args.command = command;
    Ok(args)
}
