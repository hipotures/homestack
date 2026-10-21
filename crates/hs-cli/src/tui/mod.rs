//! The HomeStack TUI (phase 13b, `docs/migration/phase-13b.md`).

pub mod acting;
pub mod action;
pub mod alien_keys;
pub mod chrome;
mod clipboard;
mod details;
pub mod draw;
mod draw_install;
mod draw_pangolin;
mod draw_setup;
pub mod install;
pub mod installing;
pub mod integrations;
pub mod layout;
pub mod pangolin;
pub mod pangolin_api;
pub mod pangolin_key;
pub mod pangolin_permanent;
mod pangolin_store;
pub mod pangolin_tunnel;
pub mod reinstall;
pub mod run;
pub mod setup;
pub mod setup_work;
pub mod shell;
pub mod state;
pub mod theme;

pub const USAGE: &str = "usage: homestack tui [--ssh USER@HOST] [--server-domain DOMAIN] [--server-image IMAGE]";

/// Explicit install values for an interactive TUI invocation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub ssh: Option<String>,
    pub server_domain: Option<String>,
    pub server_image: Option<String>,
}

impl Options {
    pub fn parse(argv: &[String]) -> Result<Self, crate::AppError> {
        if argv.first().is_some_and(|word| word != "tui") {
            return Err(crate::AppError::new(USAGE));
        }
        let mut options = Self::default();
        let mut args = argv.iter().skip(1);
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--ssh" => &mut options.ssh,
                "--server-domain" => &mut options.server_domain,
                "--server-image" => &mut options.server_image,
                _ => return Err(crate::AppError::new(format!("Unknown TUI argument {flag}; {USAGE}"))),
            };
            let value = args
                .next()
                .filter(|value| !value.trim().is_empty() && !value.starts_with('-'))
                .ok_or_else(|| crate::AppError::new(format!("{flag} needs a value; {USAGE}")))?;
            if slot.is_some() {
                return Err(crate::AppError::new(format!(
                    "{flag} was given more than once; {USAGE}"
                )));
            }
            *slot = Some(if flag == "--ssh" {
                crate::root_ssh::target(value)?
            } else {
                value.trim().to_owned()
            });
        }
        Ok(options)
    }
}

/// Plain `homestack` opens the TUI on a terminal; `homestack tui` requests it explicitly.
#[must_use]
pub fn wants_tui(argv: &[String], stdin_tty: bool, stdout_tty: bool) -> bool {
    (argv.is_empty() && stdin_tty && stdout_tty) || argv.first().is_some_and(|word| word == "tui")
}
