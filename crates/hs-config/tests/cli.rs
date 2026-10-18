//! The `hs-config` command.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn example() -> Result<String, std::io::Error> {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures/config.example.toml"))
}

fn hs_config(args: &[&str], home: &Path) -> Result<Output, std::io::Error> {
    Command::new(env!("CARGO_BIN_EXE_hs-config"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .output()
}

#[test]
fn split_writes_private_files_next_to_the_default_config() -> TestResult {
    let home = tempfile::tempdir()?;
    let dir = home.path().join(".config/homestack");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), example()?)?;

    let output = hs_config(&["split"], home.path())?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("server.toml") && stdout.contains("client.toml"),
        "{stdout}"
    );
    assert!(stdout.contains("not carried over: control, transport"), "{stdout}");
    for name in ["server.toml", "client.toml"] {
        let mode = std::fs::metadata(dir.join(name))?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }

    let check = hs_config(&["check", dir.join("server.toml").to_str().ok_or("path")?], home.path())?;
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    assert!(String::from_utf8(check.stdout)?.contains("config_version"));
    Ok(())
}

#[test]
fn split_never_overwrites_and_reports_invalid_input() -> TestResult {
    let home = tempfile::tempdir()?;
    let input = home.path().join("config.toml");
    let server = home.path().join("out-server.toml");
    let client = home.path().join("out-client.toml");
    std::fs::write(&input, example()?)?;
    std::fs::write(&server, "keep me")?;
    let args = [
        "split",
        "--config",
        input.to_str().ok_or("path")?,
        "--server-out",
        server.to_str().ok_or("path")?,
        "--client-out",
        client.to_str().ok_or("path")?,
    ];
    let output = hs_config(&args, home.path())?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&server)?, "keep me");
    assert!(!client.exists(), "nothing is written when one output exists");

    std::fs::remove_file(&server)?;
    std::fs::write(&input, example()?.replace("type = \"herdr\"", "type = \"ssh\""))?;
    let output = hs_config(&args, home.path())?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("transport.type"), "{stderr}");
    assert!(!server.exists() && !client.exists());

    let usage = hs_config(&["frobnicate"], home.path())?;
    assert_eq!(usage.status.code(), Some(2));
    Ok(())
}
