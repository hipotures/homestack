//! `homestack-deploy`: one HomeStack server instance per Proxmox node,
//! only through the Proxmox API (`docs/migration/deploy.md`).
//!
//! ```text
//! homestack-deploy [--config FILE] cert [--force] [--push]
//! homestack-deploy [--config FILE] apply [--only NAME]...
//! homestack-deploy [--config FILE] status [--only NAME]...
//! homestack-deploy [--config FILE] remove [--only NAME]... --yes
//! ```
//!
//! The default configuration is `~/.config/homestack/deploy.toml`.
//! Exit codes: 0 success, 1 failure, 2 usage.

use std::path::PathBuf;
use std::process::ExitCode;

use hs_deploy::config::DeployConfig;
use hs_deploy::deploy;

const USAGE: &str = "usage: homestack-deploy [--config FILE] cert [--force] [--push]
       homestack-deploy [--config FILE] apply [--only NAME] (repeatable)
       homestack-deploy [--config FILE] status [--only NAME] (repeatable)
       homestack-deploy [--config FILE] remove [--only NAME] (repeatable) --yes";

struct Args {
    config: PathBuf,
    command: String,
    only: Vec<String>,
    force: bool,
    push: bool,
    yes: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let mut parsed = Args {
        config: home.join(".config/homestack/deploy.toml"),
        command: String::new(),
        only: Vec::new(),
        force: false,
        push: false,
        yes: false,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--config" => parsed.config = PathBuf::from(rest.next().ok_or("--config needs a file")?),
            "--only" => parsed
                .only
                .push(rest.next().ok_or("--only needs an instance name")?.clone()),
            "--force" => parsed.force = true,
            "--push" => parsed.push = true,
            "--yes" => parsed.yes = true,
            "cert" | "apply" | "status" | "remove" if parsed.command.is_empty() => parsed.command.clone_from(arg),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    if parsed.command.is_empty() {
        return Err("expected a command".to_owned());
    }
    Ok(parsed)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let args = match parse(&args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("homestack-deploy: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match DeployConfig::load(&args.config) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("homestack-deploy: {message}");
            return ExitCode::from(1);
        }
    };
    let say = |line: &str| println!("{line}");
    let result = match args.command.as_str() {
        "cert" => deploy::cert(&config, args.force, args.push, &say).await,
        "apply" => deploy::apply(&config, &args.only, &say).await,
        "status" => deploy::status(&config, &args.only, &say).await,
        "remove" if !args.yes => Err("remove destroys the containers; add --yes".to_owned()),
        "remove" => deploy::remove(&config, &args.only, &say).await,
        _ => Err("unknown command".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("homestack-deploy: {message}");
            ExitCode::from(1)
        }
    }
}
