//! `homestack`: the HomeStack client (phase 13).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use hs_cli::app::{self, Context, Running};
use hs_cli::system::{RealSystem, Terminal};
use hs_cli::view::Style;

fn context() -> Context {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let stdout_tty = std::io::stdout().is_terminal();
    let width = crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| usize::from(columns))
        .filter(|columns| *columns > 0)
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|text| text.parse().ok()))
        .unwrap_or(100);
    Context {
        home,
        config_home,
        state_home: std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        stdin_tty: std::io::stdin().is_terminal(),
        style: Style {
            color: stdout_tty && std::env::var_os("NO_COLOR").is_none(),
            width,
        },
        system: Arc::new(RealSystem::default()),
        console: Box::new(Terminal),
        poll: Duration::from_millis(500),
        servers: Arc::new(hs_cli::servers::RealServers),
    }
}

fn main() -> ExitCode {
    // Run by ssh as its askpass (SSH_ASKPASS) for the TUI: the prompt is the
    // only argument.
    if let Some(socket) = std::env::var_os(hs_cli::askpass::SOCKET_ENV) {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        let hint = std::env::var("SSH_ASKPASS_PROMPT").ok();
        let code = hs_cli::askpass::client(std::path::Path::new(&socket), hint.as_deref(), &prompt);
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() == 2 && argv[0] == "tui" && matches!(argv[1].as_str(), "--help" | "-h") {
        println!("{}", hs_cli::tui::USAGE);
        return ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("homestack: {error}");
            return ExitCode::from(1);
        }
    };
    if hs_cli::tui::wants_tui(&argv, std::io::stdin().is_terminal(), std::io::stdout().is_terminal()) {
        let options = match hs_cli::tui::Options::parse(&argv) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("homestack: {error}");
                return ExitCode::FAILURE;
            }
        };
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            eprintln!("homestack: the TUI requires an interactive terminal");
            return ExitCode::FAILURE;
        }
        // Raw mode turns Ctrl+C into a key, so the TUI handles it itself.
        let code = runtime.block_on(async {
            let mut ctx = context();
            match hs_cli::tui::run::run(&mut ctx, options).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("homestack: {error}");
                    1
                }
            }
        });
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    let code = runtime.block_on(async {
        let mut ctx = context();
        let running = Running::default();
        let outcome = tokio::select! {
            code = app::run_following(&argv, &mut ctx, &running) => Some(code),
            _ = tokio::signal::ctrl_c() => None,
        };
        match outcome {
            Some(code) => code,
            None => app::interrupted(&argv, &mut context(), &running),
        }
    });
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
