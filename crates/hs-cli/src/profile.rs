//! Select a desktop profile before reading configuration or starting a client.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use hs_config::profiles::{self, DEFAULT, E2E};

use crate::AppError;

/// Remove the global selector, including when it appears after a subcommand.
pub fn selection(argv: &[String]) -> Result<(String, Vec<String>), AppError> {
    let mut name = None;
    let mut rest = Vec::new();
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            rest.push(word.clone());
            rest.extend(words.cloned());
            break;
        }
        let value = if matches!(word.as_str(), "--profile" | "--profil") {
            Some(
                words
                    .next()
                    .ok_or_else(|| AppError::new(format!("{word} requires a profile name")))?
                    .clone(),
            )
        } else {
            word.strip_prefix("--profile=")
                .or_else(|| word.strip_prefix("--profil="))
                .map(str::to_owned)
        };
        match value {
            Some(value) => {
                if name.replace(value).is_some() {
                    return Err(AppError::new("Select a profile only once"));
                }
            }
            None => rest.push(word.clone()),
        }
    }
    let name = name.unwrap_or_else(|| DEFAULT.to_owned());
    profiles::validate(&name).map_err(|error| AppError::new(error.to_string()))?;
    if name != DEFAULT
        && rest
            .iter()
            .any(|word| word == "--config" || word.starts_with("--config="))
    {
        return Err(AppError::new(
            "--config cannot override a named profile; edit that profile's client.toml",
        ));
    }
    Ok((name, rest))
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn ssh_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn executable(name: &str) -> Result<std::path::PathBuf, AppError> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| AppError::new(format!("Required host tool {name} was not found")))
}

/// HOME alone cannot isolate OpenSSH: its default home comes from passwd.
/// Force a profile-owned config, known_hosts and key selection as well.
pub fn prepare(root: &Path, program: &Path, ssh: &Path, argv: &[String], name: &str) -> std::io::Result<Command> {
    profiles::private_directory(root)?;
    for folder in [
        "home",
        "home/.ssh",
        "config",
        "config/homestack",
        "state",
        "cache",
        "bin",
    ] {
        profiles::private_directory(&root.join(folder))?;
    }
    let home = root.join("home");
    let config = home.join(".ssh/config");
    if config.is_symlink() {
        return Err(std::io::Error::other("Profile SSH config must not be a symlink"));
    }
    if !config.exists() {
        profiles::write_private(&config, format!(
            "Include {}\nHost *\n  IdentityAgent none\n  IdentityFile none\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  ControlMaster no\n",
            ssh_quote(&home.join(".ssh/config.d/homestack/*.conf")),
            ssh_quote(&home.join(".ssh/known_hosts")),
        ).as_bytes())?;
    }
    for (file, text) in [
        (
            "ssh",
            format!("#!/bin/sh\nexec {} -F {} \"$@\"\n", quote(ssh), quote(&config)),
        ),
        (
            "herdr",
            "#!/bin/sh\nprintf '%s\\n' 'Herdr is disabled in named profiles; use profile-specific SSH.' >&2\nexit 1\n"
                .to_owned(),
        ),
    ] {
        let path = root.join("bin").join(file);
        // Recreate wrappers rather than retaining stale executable paths.
        if path.is_symlink() {
            return Err(std::io::Error::other("Profile tool must not be a symlink"));
        }
        use std::io::Write;
        let mut temporary = tempfile::NamedTempFile::new_in(root.join("bin"))?;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        temporary.write_all(text.as_bytes())?;
        temporary.persist(path).map_err(|error| error.error)?;
    }
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let mut command = Command::new(program);
    command
        .args(argv)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("PATH", std::env::join_paths(paths).map_err(std::io::Error::other)?)
        .env("HOMESTACK_ACTIVE_PROFILE", name)
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("SSH_AGENT_PID")
        .env_remove("HOMESTACK_ASKPASS_SOCKET");
    Ok(command)
}

/// Re-exec with private environment roots before constructing the runtime.
/// `default` deliberately uses the desktop's existing canonical paths.
pub fn enter(name: &str, argv: &[String]) -> Result<(), AppError> {
    let base = profiles::environment_base().map_err(|error| AppError::new(error.to_string()))?;
    let root = profiles::directory(&base, name).map_err(|error| AppError::new(error.to_string()))?;
    let program = std::env::current_exe().map_err(|error| AppError::new(error.to_string()))?;
    let mut command = if name == E2E {
        // Check the reference before invoking anything; a missing lab can never open default.
        profiles::E2eRun::read(&root).map_err(|error| AppError::new(error.to_string()))?;
        let helper = program.with_file_name("homestack-e2e");
        if !helper.is_file() {
            return Err(AppError::new(
                "Profile e2e needs homestack-e2e beside homestack; build both with cargo build --locked -p hs-cli -p hs-e2e",
            ));
        }
        let mut command = Command::new(helper);
        command.args(["client", "--profile", E2E, "--"]).args(argv);
        command
    } else {
        profiles::private_directory(&base.join("profiles")).map_err(|error| AppError::new(error.to_string()))?;
        prepare(&root, &program, &executable("ssh")?, argv, name).map_err(|error| AppError::new(error.to_string()))?
    };
    eprintln!("Profile: {name}; storage: {}", root.display());
    Err(AppError::new(format!(
        "Cannot start profile {name}: {}",
        command.exec()
    )))
}

pub fn list() -> std::io::Result<String> {
    let base = profiles::environment_base()?;
    let mut output = format!(
        "PROFILE\tSTORAGE\ndefault\t{} (desktop configuration; SSH in ~/.ssh)\n",
        base.display()
    );
    let mut names = Vec::new();
    match std::fs::read_dir(base.join("profiles")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if profiles::validate(&name).is_ok() && name != DEFAULT && entry.file_type()?.is_dir() {
                    names.push(name);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    names.sort();
    for name in names {
        let root = profiles::directory(&base, &name)?;
        if name == E2E {
            match profiles::E2eRun::read(&root) {
                Ok(run) => output.push_str(&format!(
                    "{name}\t{} (disposable run {})\n",
                    run.artifacts.display(),
                    run.run_id
                )),
                Err(_) => output.push_str(&format!("{name}\t{} (unavailable)\n", root.display())),
            }
        } else {
            output.push_str(&format!("{name}\t{}\n", root.display()));
        }
    }
    Ok(output)
}
