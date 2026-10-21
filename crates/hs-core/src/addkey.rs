//! `addkey` (mapping C4; phase 9): give a workspace one more SSH public key.
//!
//! Ports `addkey.read_public_key`, `merge_sshkeys`, `ensure_keys_item`,
//! `build_addkey_plan` and `add_key` under decision 1 = A and the owner's
//! decision of 2026-10-13 (`docs/migration/decisions.md`, phase 9). Python
//! rewrote the VM's own vendor and meta snippets; here every node has the
//! same two static vendor snippets, whose key step appends the keys of
//! user-data that root's and the user's `authorized_keys` lack. So:
//!
//! - `sshkeys` gains the key and `cicustom` names the keep snippet (Python's
//!   `allow_format=0`): a new `sshkeys` is a new Cloud-Init instance, which
//!   runs the key step at the next start;
//! - a running VM also gets the keys at once, by the same step run through
//!   `agent/exec`, and they are checked with `file-read`;
//! - the VM is restarted only when the request asks for it.
//!
//! The local steps (`register_identity`, `sync_local_ssh`, `known_hosts`)
//! belong to the client. `docs/migration/phase-9.md` lists what differs
//! from Python.

use std::time::Duration;

use hs_model::config::Config;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::cloudinit::{HomeFormat, cicustom_value, quote_sshkeys};
use crate::error::CoreError;
use crate::guest::{GUEST_EXEC_TIMEOUT, guest_exec, qga_ping, read_text, wait_for_qga};
use crate::inventory::vm_config;
use crate::keys::{parse_authorized_key_records, unquote};
use crate::lifecycle::{LifecycleTiming, post_task, require_pool, step_finished, step_started, text, vmid_of};
use crate::power::{SHUTDOWN_TIMEOUT_SECS, START_TIMEOUT_SECS, shutdown_vm, start_vm};
use crate::pve::Pve;
use crate::refresh::parse_ipconfig0;
use crate::status::resolve_existing_workspace;
use crate::tasks::Observer;

/// The first line of the key step (Python's `ENSURE_MARKER`).
pub const ENSURE_MARKER: &str = "# HomeStack ensure-keys";
/// What the key step prints when it is done (Python's `ENSURE_DONE`).
pub const ENSURE_DONE: &str = "HomeStack: ensured SSH keys from Cloud-Init";
/// The terminator of the keys' heredoc (Python's `KEYS_HEREDOC`).
pub const KEYS_HEREDOC: &str = "HS_KEYS";

/// The loop of the key step over the directories in `$dir`, with the keys
/// in `$keys`: the same lines as in the vendor snippets.
pub const ENSURE_LOOP: &str = r#"    [ -d "$dir" ] || continue
    owner=$(stat -c %U "$dir")
    group=$(stat -c %G "$dir")
    f="$dir/.ssh/authorized_keys"
    install -d -m 700 -o "$owner" -g "$group" "$dir/.ssh"
    [ -f "$f" ] || install -m 600 -o "$owner" -g "$group" /dev/null "$f"
    [ -z "$(tail -c1 "$f")" ] || echo >> "$f"
    printf '%s\n' "$keys" | while IFS= read -r key; do
        [ -n "$key" ] || continue
        blob=$(printf '%s' "$key" | awk '{print $2}')
        grep -qF "$blob" "$f" || printf '%s\n' "$key" >> "$f"
    done
"#;

/// The agent's time after a restart (Python: `wait_for_qga_on_node`, 300 s).
const AGENT_TIMEOUT: Duration = Duration::from_secs(300);
/// Cloud-Init's time after a restart (Python: `timeout 300 cloud-init status --wait`).
const CLOUD_INIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Pause between two looks at Cloud-Init's result.
const CLOUD_INIT_POLL: Duration = Duration::from_secs(2);

/// What `addkey` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddkeyRequest {
    pub vmid: u32,
    /// One public key with a comment; without it the keys already in the
    /// VM's `sshkeys` are applied (Python: `addkey VMID` without `--key`).
    pub key: Option<String>,
    /// Restart a running VM afterwards (Python's question at the end).
    pub restart: bool,
}

/// One public key of a request (`read_public_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub key_type: String,
    pub blob: String,
    pub comment: String,
    /// `type blob comment`
    pub line: String,
}

/// `read_public_key` for the text of a `.pub` file: exactly one key, with a
/// comment. The client reads the file.
pub fn read_public_key(text: &str) -> Result<PublicKey, CoreError> {
    let records = parse_authorized_key_records(text)?;
    let [record] = records.as_slice() else {
        return Err(CoreError::Invalid(format!(
            "The public key must be exactly one key, found {}",
            records.len()
        )));
    };
    if record.comment.is_empty() {
        return Err(CoreError::Invalid(
            "The public key has no comment; give the key a name, for example ssh-keygen -C trezor-vm".to_owned(),
        ));
    }
    Ok(PublicKey {
        key_type: record.key_type.clone(),
        blob: record.blob.clone(),
        comment: record.comment.clone(),
        line: format!("{} {}", record.identity, record.comment),
    })
}

/// The key lines of a VM's `sshkeys` (percent-encoded by Proxmox).
fn sshkeys_lines(current: &str) -> Vec<String> {
    unquote(current)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `merge_sshkeys`: the keys for `sshkeys` with `key` added, and whether it
/// was already there (by its blob).
#[must_use]
pub fn merge_sshkeys(current: &str, key: &PublicKey) -> (Vec<String>, bool) {
    let lines = sshkeys_lines(current);
    let present = lines
        .iter()
        .any(|line| line.split_whitespace().nth(1) == Some(key.blob.as_str()));
    if present {
        return (lines, true);
    }
    let mut lines = lines;
    lines.push(key.line.clone());
    (lines, false)
}

/// `ensure_keys_item` as one script for `agent/exec`: append the `keys`
/// that root's and `user`'s `authorized_keys` lack, creating the file and
/// its directory with private modes; nothing is removed.
#[must_use]
pub fn ensure_keys_script(keys: &[String], user: &str) -> String {
    let mut script = format!("{ENSURE_MARKER}\nset -eu\nkeys=$(cat <<'{KEYS_HEREDOC}'\n");
    for key in keys {
        script.push_str(key);
        script.push('\n');
    }
    script.push_str(KEYS_HEREDOC);
    script.push_str("\n)\n");
    script.push_str(&format!("for dir in /root /home/{user}; do\n"));
    script.push_str(ENSURE_LOOP);
    script.push_str("done\n");
    script.push_str(&format!("echo '{ENSURE_DONE}'\n"));
    script
}

/// `build_addkey_plan` for a workspace of `pool`.
pub async fn plan_addkey(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    request: &AddkeyRequest,
) -> Result<Value, CoreError> {
    let vmid = request.vmid;
    let key = request.key.as_deref().map(read_public_key).transpose()?;
    require_pool(pve, vmid, pool).await?;
    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    let (ip, _, _) = parse_ipconfig0(info.vm_config.get("ipconfig0").map(String::as_str).unwrap_or_default())?;
    let cicustom = info.vm_config.get("cicustom").map(String::as_str).unwrap_or_default();
    let keep_snippet = cicustom == cicustom_value(cfg, HomeFormat::Keep);
    let current = info.vm_config.get("sshkeys").map(String::as_str).unwrap_or_default();
    let (keys, present) = match &key {
        Some(key) => merge_sshkeys(current, key),
        None => {
            let keys = sshkeys_lines(current);
            if keys.is_empty() {
                return Err(CoreError::Refused(format!(
                    "VM {vmid} has no keys in its Proxmox sshkeys; give a key"
                )));
            }
            (keys, true)
        }
    };
    let label = key.as_ref().map_or_else(
        || {
            keys.iter()
                .map(|line| line.split_whitespace().last().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        },
        |key| key.comment.clone(),
    );
    let user = &cfg.user.name;
    let running = info.status == "running";
    let mut steps = vec![
        match &key {
            Some(_) if present => "Add the key to the Proxmox sshkeys of the VM (already there)".to_owned(),
            Some(_) => "Add the key to the Proxmox sshkeys of the VM".to_owned(),
            None => format!("Keep the Proxmox sshkeys as they are: {label}"),
        },
        if keep_snippet {
            "Keep the keep vendor snippet, which cannot rewrite authorized_keys or home".to_owned()
        } else {
            "Replace custom Cloud-Init snippets with the HomeStack keep vendor snippet; Proxmox generates user, network and meta data".to_owned()
        },
    ];
    if running {
        steps.push(format!(
            "Append missing keys to root and {user} authorized_keys now through the guest agent; nothing is removed"
        ));
        steps.push(format!("Check that every key is in root and {user} authorized_keys"));
        steps.push(if request.restart {
            "Restart the VM, then check the keys again".to_owned()
        } else {
            "The VM is NOT restarted".to_owned()
        });
    } else {
        steps.push(format!(
            "The VM is stopped and stays stopped; Cloud-Init appends missing keys to root and {user} \
             authorized_keys at its next start"
        ));
    }
    if !present {
        steps.push(
            "The new sshkeys start a new Cloud-Init instance at the next start: the VM gets new SSH host keys"
                .to_owned(),
        );
    }
    Ok(json!({
        "command": "addkey",
        "vmid": vmid,
        "name": info.name,
        "node": info.node,
        "pool": pool,
        "status": info.status,
        "ip": ip,
        "user": user,
        "key": key.as_ref().map(|key| json!({
            "comment": key.comment,
            "type": key.key_type,
            "line": key.line,
            "blob": key.blob,
        })),
        "label": label,
        "key_already_in_sshkeys": present,
        "keep_snippet": keep_snippet,
        "original_cicustom": cicustom,
        "original_sshkeys": current,
        "keys": keys,
        "restart": request.restart,
        "steps": steps,
    }))
}

/// The keys of `keys` that `path` lacks, by blob; `None` for every key when
/// the file is missing.
async fn missing_keys(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    path: &str,
    keys: &[String],
) -> Result<Vec<String>, CoreError> {
    let text = read_text(pve, node, vmid, path).await?.unwrap_or_default();
    Ok(keys
        .iter()
        .filter_map(|line| {
            let blob = line.split_whitespace().nth(1)?;
            (!text.contains(blob)).then(|| line.split_whitespace().last().unwrap_or("?").to_owned())
        })
        .collect())
}

/// Python's check after the restart: every key in root's and the user's
/// `authorized_keys`, read with `file-read`.
async fn verify_keys(pve: &impl Pve, node: &str, vmid: u32, user: &str, keys: &[String]) -> Result<(), CoreError> {
    let mut lost = Vec::new();
    for path in [
        "/root/.ssh/authorized_keys".to_owned(),
        format!("/home/{user}/.ssh/authorized_keys"),
    ] {
        for comment in missing_keys(pve, node, vmid, &path, keys).await? {
            lost.push(format!("{comment} in {path}"));
        }
    }
    if lost.is_empty() {
        Ok(())
    } else {
        Err(CoreError::Failed(format!(
            "Not all keys are in root and {user} authorized_keys of VM {vmid}: {}",
            lost.join("; ")
        )))
    }
}

/// `timeout 300 cloud-init status --wait` and `status: done`: Cloud-Init's
/// `/run/cloud-init/result.json` appears and lists no errors (decision 5).
async fn wait_cloud_init_done(pve: &impl Pve, node: &str, vmid: u32) -> Result<(), CoreError> {
    let deadline = Instant::now() + CLOUD_INIT_TIMEOUT;
    let result = loop {
        if let Some(text) = read_text(pve, node, vmid, "/run/cloud-init/result.json").await? {
            break text;
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Failed(format!(
                "Cloud-Init on VM {vmid} did not report 'done' after the restart: no result within {}s",
                CLOUD_INIT_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(CLOUD_INIT_POLL).await;
    };
    let errors = serde_json::from_str::<Value>(&result)
        .ok()
        .and_then(|value| value.pointer("/v1/errors").and_then(Value::as_array).cloned());
    match errors {
        Some(errors) if errors.is_empty() => Ok(()),
        _ => Err(CoreError::Failed(format!(
            "Cloud-Init on VM {vmid} did not report 'done' after the restart: {}",
            result.trim().chars().take(200).collect::<String>()
        ))),
    }
}

/// `add_key` for a plan of [`plan_addkey`]: the workspace is checked again
/// (tags, pool, node, power state) before anything changes.
pub async fn add_key(
    pve: &impl Pve,
    cfg: &Config,
    pool: &str,
    plan: &Value,
    timing: &LifecycleTiming,
    observer: &impl Observer,
) -> Result<Value, CoreError> {
    let vmid = vmid_of(plan)?;
    let node = text(plan, "node")?;
    let planned_status = text(plan, "status")?;
    let user = text(plan, "user")?;
    let keys: Vec<String> = plan
        .get("keys")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    if keys.is_empty() {
        return Err(CoreError::Unusable("The plan has no keys".to_owned()));
    }
    let present = plan
        .get("key_already_in_sshkeys")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let keep_snippet = plan.get("keep_snippet").and_then(Value::as_bool).unwrap_or(false);
    let restart = plan.get("restart").and_then(Value::as_bool).unwrap_or(false);

    let info = resolve_existing_workspace(pve, cfg, vmid).await?;
    require_pool(pve, vmid, pool).await?;
    if info.node != node {
        return Err(CoreError::Refused(format!(
            "VM {vmid} is on {}, not on {node} as planned; generate a new plan",
            info.node
        )));
    }
    let status = &info.status;
    if status != planned_status {
        return Err(CoreError::Refused(format!(
            "VM {vmid} is {status}, not {planned_status} as planned; generate a new plan"
        )));
    }
    if info.vm_config.get("cicustom").map(String::as_str).unwrap_or_default()
        != text(plan, "original_cicustom")?
        || info.vm_config.get("sshkeys").map(String::as_str).unwrap_or_default()
            != text(plan, "original_sshkeys")?
    {
        return Err(CoreError::Refused(format!(
            "VM {vmid} Cloud-Init snippets or SSH keys changed after planning; generate a new plan"
        )));
    }
    let running = status == "running";

    if !present || !keep_snippet {
        let step = "Cloud-Init keys";
        step_started(observer, step);
        let configured = async {
            let sshkeys = quote_sshkeys(&(keys.join("\n") + "\n"));
            let cicustom = cicustom_value(cfg, HomeFormat::Keep);
            let mut form: Vec<(&str, &str)> = Vec::new();
            if !present {
                form.push(("sshkeys", &sshkeys));
            }
            if !keep_snippet {
                form.push(("cicustom", &cicustom));
            }
            if let Some(digest) = info.vm_config.get("digest").filter(|digest| !digest.is_empty()) {
                form.push(("digest", digest));
            }
            post_task(
                pve,
                &format!("/nodes/{node}/qemu/{vmid}/config"),
                &form,
                &format!("Setting the SSH keys of VM {vmid}"),
                &timing.wait(600),
                observer,
            )
            .await?;
            let written = vm_config(pve, node, vmid).await?;
            let written_keys = sshkeys_lines(written.get("sshkeys").map(String::as_str).unwrap_or_default());
            if written_keys != keys || written.get("cicustom") != Some(&cicustom) {
                return Err(CoreError::Failed(format!(
                    "VM {vmid} does not hold the planned sshkeys and keep vendor snippet after the change"
                )));
            }
            Ok(())
        }
        .await;
        step_finished(observer, step, configured.is_ok());
        configured?;
    }

    let mut verified = false;
    let mut agent_note = None;
    if running {
        let step = "Add keys through the guest agent";
        step_started(observer, step);
        let added = async {
            if let Err(error) = qga_ping(pve, node, vmid).await {
                return Ok(Some(format!(
                    "The QEMU Guest Agent of VM {vmid} did not answer ({error}); the keys are added at its next start"
                )));
            }
            guest_exec(
                pve,
                node,
                vmid,
                &ensure_keys_script(&keys, user),
                true,
                GUEST_EXEC_TIMEOUT,
            )
            .await?;
            verify_keys(pve, node, vmid, user, &keys).await?;
            Ok::<_, CoreError>(None)
        }
        .await;
        step_finished(observer, step, added.is_ok());
        agent_note = added?;
        verified = agent_note.is_none();
    }

    let restarted = running && restart;
    if restarted {
        let step = "Restart workspace";
        step_started(observer, step);
        let cycled = async {
            shutdown_vm(pve, node, vmid, SHUTDOWN_TIMEOUT_SECS, &timing.wait(600), observer).await?;
            start_vm(pve, node, vmid, &timing.wait(START_TIMEOUT_SECS), observer).await
        }
        .await;
        step_finished(observer, step, cycled.is_ok());
        cycled?;

        let step = "Verify keys after the restart";
        step_started(observer, step);
        let checked = async {
            wait_for_qga(pve, node, vmid, AGENT_TIMEOUT).await?;
            wait_cloud_init_done(pve, node, vmid).await?;
            verify_keys(pve, node, vmid, user, &keys).await
        }
        .await;
        step_finished(observer, step, checked.is_ok());
        checked?;
        verified = true;
    }

    let mut result: Map<String, Value> = Map::new();
    result.insert("ok".to_owned(), json!(true));
    result.insert("command".to_owned(), json!("addkey"));
    result.insert("vmid".to_owned(), json!(vmid));
    result.insert("name".to_owned(), plan.get("name").cloned().unwrap_or(Value::Null));
    result.insert("ip".to_owned(), plan.get("ip").cloned().unwrap_or(Value::Null));
    result.insert("key".to_owned(), plan.get("label").cloned().unwrap_or(Value::Null));
    result.insert("restarted".to_owned(), json!(restarted));
    result.insert("pending_restart".to_owned(), json!(running && !verified));
    result.insert("verified".to_owned(), json!(verified));
    result.insert("new_instance_at_next_start".to_owned(), json!(!present && !restarted));
    if let Some(note) = agent_note {
        result.insert("note".to_owned(), json!(note));
    }
    result.insert("event".to_owned(), json!("workspace.key_added"));
    Ok(Value::Object(result))
}
