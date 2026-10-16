//! Explicit TUI install values are validated before opening the terminal.

use hs_cli::tui::{Options, wants_tui};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

#[test]
fn explicit_tui_values_select_ssh_and_the_server_domain() -> Result<(), Box<dyn std::error::Error>> {
    let argv = args(&[
        "tui",
        "--ssh",
        "pve-e2e",
        "--server-domain",
        "e2e.homestack.app",
        "--server-image",
        "ghcr.io/test/server:tag",
    ]);
    assert!(wants_tui(&argv, true, true));
    assert!(wants_tui(&argv, false, false));
    let options = Options::parse(&argv)?;
    assert_eq!(options.ssh.as_deref(), Some("root@pve-e2e"));
    assert_eq!(options.server_image.as_deref(), Some("ghcr.io/test/server:tag"));
    assert_eq!(options.server_domain.as_deref(), Some("e2e.homestack.app"));
    assert_eq!(Options::parse(&[])?, Options::default());
    Ok(())
}

#[test]
fn invalid_or_missing_tui_values_are_rejected() {
    for words in [
        vec!["install", "--ssh", "pve-e2e"],
        vec!["tui", "--ssh"],
        vec!["tui", "--ssh", ""],
        vec!["tui", "--ssh", "root@pve;reboot"],
        vec!["tui", "--server-domain", "--ssh"],
        vec!["tui", "--server-domain", ""],
        vec!["tui", "--ssh", "pve-e2e", "--ssh", "production.invalid"],
        vec!["tui", "--unknown", "value"],
    ] {
        assert!(Options::parse(&args(&words)).is_err(), "accepted {words:?}");
    }
}

#[test]
fn tui_help_and_nonterminal_errors_do_not_open_a_client() -> Result<(), Box<dyn std::error::Error>> {
    let binary = env!("CARGO_BIN_EXE_homestack");
    let help = std::process::Command::new(binary).args(["tui", "--help"]).output()?;
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout)?.contains("--server-domain DOMAIN"));
    let output = std::process::Command::new(binary)
        .args(["tui", "--ssh", "root@pve-e2e"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("requires an interactive terminal"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn explicit_access_and_copy_mode_work_in_a_real_terminal() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tui_terminal.py"))
        .arg(env!("CARGO_BIN_EXE_homestack"))
        .output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}
