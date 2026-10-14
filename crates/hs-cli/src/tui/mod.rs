//! The HomeStack TUI (phase 13b, `docs/migration/phase-13b.md`).

pub mod acting;
pub mod action;
pub mod chrome;
pub mod draw;
pub mod layout;
pub mod run;
pub mod state;
pub mod theme;

/// Plain `homestack` (no argument at all) on a terminal opens the TUI;
/// anything else, or output that is not a terminal, is the CLI.
#[must_use]
pub fn wants_tui(argv: &[String], stdin_tty: bool, stdout_tty: bool) -> bool {
    argv.is_empty() && stdin_tty && stdout_tty
}
