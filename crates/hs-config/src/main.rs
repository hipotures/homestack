//! `hs-config`: configuration tools for the HomeStack server migration.
//!
//! ```text
//! hs-config split [--config FILE] [--server-out FILE] [--client-out FILE]
//! hs-config check FILE
//! ```
//!
//! `split` reads the Python `config.toml` (default: the path the Python CLI
//! uses) and writes `server.toml` and `client.toml` next to it, mode 0600.
//! It never overwrites a file. `check` validates a `server.toml` and prints
//! its version, the value `GET /v1/config/version` will report after upload.
//!
//! Exit codes: 0 success, 1 invalid configuration or file problem, 2 usage.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hs_config::paths::default_config_path;
use hs_config::split::split;
use hs_model::config::{Config, config_version};

const USAGE: &str =
    "usage: hs-config split [--config FILE] [--server-out FILE] [--client-out FILE]\n       hs-config check FILE";

enum Failure {
    Usage(String),
    Failed(String),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("split") => run_split(&args[1..]),
        Some("check") => run_check(&args[1..]),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => Err(Failure::Usage("expected a command".to_owned())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            eprintln!("hs-config: {message}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Failed(message)) => {
            eprintln!("hs-config: {message}");
            ExitCode::from(1)
        }
    }
}

fn run_split(args: &[String]) -> Result<(), Failure> {
    let mut config = None;
    let mut server_out = None;
    let mut client_out = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--config" => &mut config,
            "--server-out" => &mut server_out,
            "--client-out" => &mut client_out,
            other => return Err(Failure::Usage(format!("unknown argument {other}"))),
        };
        let value = rest
            .next()
            .ok_or_else(|| Failure::Usage(format!("{flag} needs a file")))?;
        *slot = Some(PathBuf::from(value));
    }
    let config = match config {
        Some(config) => config,
        None => default_config_path(|name| std::env::var(name).ok())
            .ok_or_else(|| Failure::Usage("set --config, HOME or XDG_CONFIG_HOME".to_owned()))?,
    };
    let dir = config.parent().unwrap_or(Path::new(".")).to_owned();
    let server_out = server_out.unwrap_or_else(|| dir.join("server.toml"));
    let client_out = client_out.unwrap_or_else(|| dir.join("client.toml"));

    let text = read(&config)?;
    let files =
        split(&text).map_err(|error| Failure::Failed(format!("{}: {}: {error}", config.display(), error.field)))?;
    for out in [&server_out, &client_out] {
        if out.exists() {
            return Err(Failure::Failed(format!("{} exists; not overwriting", out.display())));
        }
    }
    write_new(&server_out, &files.server_toml)?;
    if let Err(failure) = write_new(&client_out, &files.client_toml) {
        let _ = std::fs::remove_file(&server_out);
        return Err(failure);
    }
    println!(
        "wrote {} (config_version {})",
        server_out.display(),
        config_version(&files.server_toml)
    );
    println!("wrote {}", client_out.display());
    if !files.dropped.is_empty() {
        println!("not carried over: {}", files.dropped.join(", "));
    }
    Ok(())
}

fn run_check(args: &[String]) -> Result<(), Failure> {
    let [file] = args else {
        return Err(Failure::Usage("check needs one file".to_owned()));
    };
    let path = Path::new(file);
    let text = read(path)?;
    Config::parse(&text).map_err(|error| Failure::Failed(format!("{}: {}: {error}", path.display(), error.field)))?;
    println!("config_version {}", config_version(&text));
    Ok(())
}

fn read(path: &Path) -> Result<String, Failure> {
    std::fs::read_to_string(path).map_err(|error| Failure::Failed(format!("{}: {error}", path.display())))
}

/// Create `path` with mode 0600; fail if it exists.
fn write_new(path: &Path, text: &str) -> Result<(), Failure> {
    let failed = |error: std::io::Error| Failure::Failed(format!("{}: {error}", path.display()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)
}
