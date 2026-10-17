//! Append public keys through the running alien VM's own SSH alias.

use base64::Engine;
use hs_client::Client;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::action::{Decision, Desktop, Progress, Start, Step, StepState};
use crate::system::{Output, System};

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// The script only appends missing identities to the login account's file.
pub fn script(keys: &[String]) -> String {
    let mut text = r#"set -eu
umask 077
: "${HOME:?No home directory}"
[ ! -L "$HOME/.ssh" ] && [ ! -L "$HOME/.ssh/authorized_keys" ] || { echo 'Refusing a symlink in the SSH key path' >&2; exit 1; }
mkdir -p "$HOME/.ssh"
chmod 700 "$HOME/.ssh"
file="$HOME/.ssh/authorized_keys"
[ -f "$file" ] || : > "$file"
chmod 600 "$file"
"#.to_owned();
    for key in keys {
        text.push_str(&format!("key={}\n", quote(key)));
        text.push_str(r#"blob=$(printf '%s\n' "$key" | awk '{print $2}')
if ! awk -v blob="$blob" '{for (i=1; i<NF; i++) if ($i ~ /^(ssh-|ecdsa-|sk-)/ && $(i+1) == blob) found=1} END {exit !found}' "$file"; then
    if [ -s "$file" ] && [ -n "$(tail -c 1 "$file")" ]; then printf '\n' >> "$file"; fi
    printf '%s\n' "$key" >> "$file"
fi
"#);
    }
    text.push_str("printf 'Public keys copied to %s\\n' \"$file\"\n");
    text
}

async fn run(system: Arc<dyn System>, argv: Vec<String>, input: String) -> Result<Output, String> {
    let output = tokio::task::spawn_blocking(move || system.run_input(&argv, &input))
        .await
        .map_err(|error| format!("SSH task failed: {error}"))?
        .map_err(|error| format!("Could not run SSH: {error}"))?;
    if output.code != 0 {
        return Err(format!("SSH failed ({}): {}", output.code, output.stderr.trim()));
    }
    Ok(output)
}

async fn running(client: &Client, vmid: u32, alias: &str) -> Result<(), String> {
    let status = client
        .get("/v1/status")
        .await
        .map_err(|error| crate::app::client_error(error).0)?;
    let vm = status["workspaces"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["vmid"] == vmid));
    if !vm.is_some_and(|vm| vm["role"] == "ALIEN" && vm["status"] == "running" && vm["name"] == alias) {
        return Err(format!(
            "VM {vmid} must still be a running alien VM named {alias}; nothing was copied."
        ));
    }
    Ok(())
}

pub async fn work(
    client: &Client,
    desktop: &Desktop,
    start: Start,
    ssh_prefix: Vec<String>,
    system: Arc<dyn System>,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<(), String> {
    let alias = start.parameters["alien_ssh"]
        .as_str()
        .filter(|alias| crate::app::valid_name(alias))
        .ok_or("No valid alien SSH alias")?
        .to_owned();
    let vmid = start.vmid.ok_or("No alien VM selected")?;
    if start.key_files.is_empty() {
        return Err("Select at least one public key file.".to_owned());
    }
    let mut keys = Vec::new();
    let mut private = Vec::new();
    for file in &start.key_files {
        let text = std::fs::read_to_string(crate::expand_home(file, &desktop.home))
            .map_err(|error| format!("{file}: {error}"))?;
        let lines: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        let [line] = lines.as_slice() else {
            return Err(format!("{file}: select a file containing exactly one public key."));
        };
        let words: Vec<_> = line.split_whitespace().collect();
        if words.len() < 3
            || !["ssh-", "sk-", "ecdsa-"]
                .iter()
                .any(|prefix| words[0].starts_with(prefix))
            || base64::engine::general_purpose::STANDARD.decode(words[1]).is_err()
            || line
                .chars()
                .any(|character| character.is_control() && character != '\t')
        {
            return Err(format!("{file}: invalid public key or missing key comment."));
        }
        keys.push((*line).to_owned());
        private.push(
            crate::identity::private_key_for(&desktop.home, std::path::Path::new(file))
                .map_err(|error| error.0)?
                .1,
        );
    }
    running(client, vmid, &alias).await?;
    let settings = run(
        Arc::clone(&system),
        vec!["ssh".to_owned(), "-G".to_owned(), alias.clone()],
        String::new(),
    )
    .await?;
    let setting = |key: &str| {
        settings
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key} ")))
            .map(str::to_owned)
    };
    let user = setting("user")
        .filter(|value| !value.is_empty())
        .ok_or("SSH did not report the login account for this alias")?;
    let host = setting("hostname")
        .filter(|value| !value.is_empty())
        .ok_or("SSH did not report the host for this alias")?;
    let mut lines = vec![
        format!("Copy {} public keys to {alias} ({user}@{host})", keys.len()),
        "Append missing keys to this login account's ~/.ssh/authorized_keys over SSH.".to_owned(),
        "Existing entries and restrictions are preserved. The VM stays running.".to_owned(),
        String::new(),
    ];
    lines.extend(start.key_files.iter().zip(&keys).map(|(file, key)| {
        format!(
            "{} — {file}",
            key.split_whitespace().skip(2).collect::<Vec<_>>().join(" ")
        )
    }));
    progress(Progress::Plan {
        lines,
        question: format!("Copy these public keys to {user}@{alias}?"),
        blocked: None,
        secret: None,
    });
    match decisions.recv().await {
        Some(Decision::Apply(_)) => {}
        Some(_) => {
            progress(Progress::Done {
                ok: true,
                lines: vec!["Cancelled. No changes were made.".to_owned()],
            });
            return Ok(());
        }
        None => return Ok(()),
    }
    running(client, vmid, &alias).await?;
    let current_settings = run(
        Arc::clone(&system),
        vec!["ssh".to_owned(), "-G".to_owned(), alias.clone()],
        String::new(),
    )
    .await?;
    if current_settings.stdout != settings.stdout {
        return Err(
            "The SSH alias configuration changed after review; nothing was copied. Review the keys again.".to_owned(),
        );
    }
    progress(Progress::Steps(vec![Step {
        name: format!("Copy public keys over SSH to {alias}"),
        state: StepState::Running,
    }]));
    let mut argv = ssh_prefix;
    argv.extend(
        [
            "ssh",
            "-T",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            &alias,
            "sh -s",
        ]
        .map(str::to_owned),
    );
    run(system, argv, script(&keys)).await?;
    for key in private {
        crate::identity::register_identity(&desktop.home, &desktop.loaded.path, &key)
            .map_err(|error| format!("Keys were copied, but local identity registration failed: {}", error.0))?;
    }
    progress(Progress::Done {
        ok: true,
        lines: vec![format!(
            "Copied {} public keys to {user}@{alias}: ~/.ssh/authorized_keys.",
            keys.len()
        )],
    });
    Ok(())
}
