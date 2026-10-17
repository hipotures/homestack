//! Guest verification after `create`, `refresh` and `migrate` (mapping D2).
//!
//! Python runs shell commands in the guest through `exec` (the block
//! "Verify workspace" of `lifecycle.create_workspace` and
//! `lifecycle._verify_refreshed_guest`). Decision 5 keeps the server out of
//! the guest's shell: the same facts are read with the audit-level agent
//! commands and `file-read` (`VM.GuestAgent.Audit` + `FileRead`):
//!
//! | Python | here |
//! |---|---|
//! | `cloud-init status --wait` | `/run/cloud-init/result.json` appears |
//! | `hostname` | `get-host-name` |
//! | `ip -4 addr show dev eth0` | `network-get-interfaces` |
//! | `findmnt` | `get-fsinfo` |
//! | `blkid` label | the ext4 superblock of the mounted device |
//! | `id`, `awk /etc/passwd` | `/etc/passwd` |
//! | `test -s .../authorized_keys` | the file is readable and not empty |
//! | `cloud-id` | `/run/cloud-init/cloud-id` |
//!
//! The order of the checks and their messages are Python's;
//! `docs/migration/phase-3.md` lists where the text differs.

use std::time::Duration;

use hs_model::config::User;
use serde_json::Value;
use tokio::time::Instant;

use crate::error::CoreError;
use crate::guest::{agent_get, ext4_label, find_program, read_text, wait_for_qga};
use crate::inventory::py_repr;
use crate::pve::Pve;

/// What the guest must look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExpectation {
    /// The VM name, which is also the hostname.
    pub name: String,
    /// `ip/cidr` on `eth0`.
    pub address: String,
    pub user: User,
    /// `HS_HOME_<vmid>`
    pub home_label: String,
}

/// How long to wait for the agent and for cloud-init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyTimeouts {
    pub agent: Duration,
    pub cloud_init: Duration,
}

impl Default for VerifyTimeouts {
    /// Python's: 600 s for the agent after `create` and `refresh`, and
    /// `timeout 180 cloud-init status --wait`.
    fn default() -> Self {
        Self {
            agent: Duration::from_secs(600),
            cloud_init: Duration::from_secs(180),
        }
    }
}

/// A guest that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// `cloud-id`, when cloud-init reports one.
    pub cloud_id: Option<String>,
    /// The user's `authorized_keys`, for `create`'s comparison with the
    /// requested keys (phase 5).
    pub user_authorized_keys: String,
}

/// The first check that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyFailure {
    Hostname {
        actual: String,
    },
    /// `actual` lists the IPv4 addresses of `eth0` as `ip/prefix`.
    Address {
        expected: String,
        actual: Vec<String>,
    },
    /// `found` is `/dev/DEVICE TYPE MOUNTPOINT` when something is mounted.
    HomeMount {
        path: String,
        found: String,
    },
    HomeLabel {
        actual: String,
    },
    /// `actual` is the user's `/etc/passwd` line, or empty.
    UserIdentity {
        actual: String,
    },
    UserKeysMissing,
    RootKeysMissing,
    /// `found` lists `name:uid:gid` of the accounts with UIDs 1000–65533.
    RegularUsers {
        found: Vec<String>,
    },
}

impl VerifyFailure {
    /// The message of `create_workspace`.
    #[must_use]
    pub fn create_message(&self, expected: &GuestExpectation) -> String {
        let detail = match self {
            Self::Hostname { actual } => format!(
                "hostname is {}, expected {}",
                py_repr(Some(actual)),
                py_repr(Some(&expected.name))
            ),
            Self::Address { expected, actual } => {
                format!("eth0 does not have {expected}\n{}", actual.join("\n"))
            }
            Self::HomeMount { path, found } => format!("{path} is not mounted as ext4\n{found}"),
            Self::HomeLabel { actual } => format!(
                "persistent home label is {}, expected {}",
                py_repr(Some(actual)),
                py_repr(Some(&expected.home_label))
            ),
            Self::UserIdentity { actual } => format!("unexpected user identity: {actual}"),
            Self::UserKeysMissing => "user authorized_keys is missing".to_owned(),
            Self::RootKeysMissing => "root authorized_keys is missing".to_owned(),
            Self::RegularUsers { found } => format!(
                "unexpected regular user accounts: {}",
                if found.is_empty() {
                    "none".to_owned()
                } else {
                    found.join(", ")
                }
            ),
        };
        format!("Verification failed: {detail}")
    }

    /// The message of `_verify_refreshed_guest`.
    #[must_use]
    pub fn refresh_message(&self) -> String {
        let detail = match self {
            Self::Hostname { .. } => "workspace hostname is incorrect".to_owned(),
            Self::Address { expected, .. } => format!("eth0 does not have {expected}"),
            Self::HomeMount { path, .. } => format!("{path} is not ext4"),
            Self::HomeLabel { actual } => format!("home label is {}", py_repr(Some(actual))),
            Self::UserIdentity { actual } => format!("unexpected user identity: {actual}"),
            Self::UserKeysMissing => "persistent user authorized_keys is missing".to_owned(),
            Self::RootKeysMissing => "root authorized_keys is missing".to_owned(),
            Self::RegularUsers { .. } => "unexpected regular user accounts".to_owned(),
        };
        format!("Refresh verification failed: {detail}")
    }
}

/// Pause between two looks at cloud-init's result.
const CLOUD_INIT_POLL: Duration = Duration::from_secs(2);

/// `command -v cloud-init && timeout 180 cloud-init status --wait || true`:
/// wait until cloud-init has written its result; never fails on its own.
async fn wait_for_cloud_init(pve: &impl Pve, node: &str, vmid: u32, timeout: Duration) -> Result<(), CoreError> {
    if find_program(pve, node, vmid, "cloud-init").await?.is_none() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    loop {
        if read_text(pve, node, vmid, "/run/cloud-init/result.json")
            .await?
            .is_some()
            || Instant::now() >= deadline
        {
            return Ok(());
        }
        tokio::time::sleep(CLOUD_INIT_POLL).await;
    }
}

/// The IPv4 addresses of an interface as `ip/prefix`.
fn ipv4_addresses(interfaces: &Value, name: &str) -> Vec<String> {
    interfaces
        .as_array()
        .into_iter()
        .flatten()
        .filter(|interface| interface.get("name").and_then(Value::as_str) == Some(name))
        .flat_map(|interface| {
            interface
                .get("ip-addresses")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|address| address.get("ip-address-type").and_then(Value::as_str) == Some("ipv4"))
        .filter_map(|address| {
            let ip = address.get("ip-address").and_then(Value::as_str)?;
            let prefix = address.get("prefix").and_then(Value::as_u64)?;
            Some(format!("{ip}/{prefix}"))
        })
        .collect()
}

/// `(device, type)` of the filesystem mounted exactly at `path`.
fn mounted_at(fsinfo: &Value, path: &str) -> Option<(String, String)> {
    fsinfo.as_array()?.iter().find_map(|fs| {
        (fs.get("mountpoint").and_then(Value::as_str) == Some(path)).then(|| {
            let text = |key: &str| fs.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            (text("name"), text("type"))
        })
    })
}

/// `awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}'`
fn regular_users(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let uid: u64 = fields.get(2)?.parse().ok()?;
            (1000..65534)
                .contains(&uid)
                .then(|| format!("{}:{}:{}", fields[0], fields[2], fields.get(3).unwrap_or(&"")))
        })
        .collect()
}

/// Whether a file exists and is not empty (`test -s`).
async fn non_empty(pve: &impl Pve, node: &str, vmid: u32, path: &str) -> Result<Option<String>, CoreError> {
    Ok(read_text(pve, node, vmid, path).await?.filter(|text| !text.is_empty()))
}

/// Verify a started workspace guest. `Ok(Err(..))` names the first check
/// that failed; `Err` is a failure to read (agent timeout, missing
/// privilege, Proxmox error).
pub async fn verify_guest(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    expected: &GuestExpectation,
    timeouts: &VerifyTimeouts,
) -> Result<Result<Verified, VerifyFailure>, CoreError> {
    wait_for_qga(pve, node, vmid, timeouts.agent).await?;
    wait_for_cloud_init(pve, node, vmid, timeouts.cloud_init).await?;

    let host = agent_get(pve, node, vmid, "get-host-name").await?;
    let hostname = host.get("host-name").and_then(Value::as_str).unwrap_or_default();
    if hostname != expected.name {
        return Ok(Err(VerifyFailure::Hostname {
            actual: hostname.to_owned(),
        }));
    }

    let addresses = ipv4_addresses(&agent_get(pve, node, vmid, "network-get-interfaces").await?, "eth0");
    if !addresses.contains(&expected.address) {
        return Ok(Err(VerifyFailure::Address {
            expected: expected.address.clone(),
            actual: addresses,
        }));
    }

    let home = format!("/home/{}", expected.user.name);
    let mount = mounted_at(&agent_get(pve, node, vmid, "get-fsinfo").await?, &home);
    let device = match mount {
        Some((device, fstype)) if fstype == "ext4" => device,
        other => {
            let found = other
                .map(|(device, fstype)| format!("/dev/{device} {fstype} {home}"))
                .unwrap_or_default();
            return Ok(Err(VerifyFailure::HomeMount { path: home, found }));
        }
    };
    let label = ext4_label(pve, node, vmid, &format!("/dev/{device}"))
        .await?
        .unwrap_or_default();
    if label != expected.home_label {
        return Ok(Err(VerifyFailure::HomeLabel { actual: label }));
    }

    let passwd = read_text(pve, node, vmid, "/etc/passwd").await?.unwrap_or_default();
    let account = passwd
        .lines()
        .find(|line| line.split(':').next() == Some(expected.user.name.as_str()));
    let uid_ok = account.is_some_and(|line| line.split(':').nth(2) == Some(expected.user.uid.to_string().as_str()));
    if !uid_ok {
        return Ok(Err(VerifyFailure::UserIdentity {
            actual: account.unwrap_or_default().to_owned(),
        }));
    }

    let Some(user_keys) = non_empty(pve, node, vmid, &format!("{home}/.ssh/authorized_keys")).await? else {
        return Ok(Err(VerifyFailure::UserKeysMissing));
    };
    if non_empty(pve, node, vmid, "/root/.ssh/authorized_keys")
        .await?
        .is_none()
    {
        return Ok(Err(VerifyFailure::RootKeysMissing));
    }

    let regular = regular_users(&passwd);
    let user = &expected.user;
    if regular != [format!("{}:{}:{}", user.name, user.uid, user.gid)] {
        return Ok(Err(VerifyFailure::RegularUsers { found: regular }));
    }

    let cloud_id = read_text(pve, node, vmid, "/run/cloud-init/cloud-id")
        .await?
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    Ok(Ok(Verified {
        cloud_id,
        user_authorized_keys: user_keys,
    }))
}

/// `lifecycle._verify_refreshed_guest`: the `cloud-id`, or an error with
/// the refresh message.
pub async fn verify_refreshed_guest(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    expected: &GuestExpectation,
    timeouts: &VerifyTimeouts,
) -> Result<Option<String>, CoreError> {
    match verify_guest(pve, node, vmid, expected, timeouts).await? {
        Ok(verified) => Ok(verified.cloud_id),
        Err(failure) => Err(CoreError::Failed(failure.refresh_message())),
    }
}

/// The end of `migrate_workspace` for a VM started on `node`: the agent
/// within `agent_timeout`, then the home at `/home/<user>` must be ext4 with
/// the label `home_label`. Python reads `findmnt` and `blkid`; the server
/// reads `get-fsinfo` and the superblock, and shows the mount as `findmnt`
/// would print it.
pub async fn verify_migrated_home(
    pve: &impl Pve,
    node: &str,
    vmid: u32,
    user: &str,
    home_label: &str,
    agent_timeout: Duration,
) -> Result<(), CoreError> {
    wait_for_qga(pve, node, vmid, agent_timeout).await?;
    let home = format!("/home/{user}");
    let mount = mounted_at(&agent_get(pve, node, vmid, "get-fsinfo").await?, &home);
    let (mounted, label) = match mount {
        Some((device, fstype)) => {
            let label = ext4_label(pve, node, vmid, &format!("/dev/{device}"))
                .await?
                .unwrap_or_default();
            (format!("/dev/{device} {fstype} {home}"), label)
        }
        None => (String::new(), String::new()),
    };
    if !mounted.contains("ext4") || label != home_label {
        return Err(CoreError::Failed(format!(
            "Migration completed but persistent home verification failed on {node}: mount={}, label={}",
            py_repr(Some(&mounted)),
            py_repr(Some(&label))
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_users_like_awk() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      developer:x:1000:1000::/home/developer:/bin/bash\n\
                      nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
                      odd:x:abc:1::/:/bin/sh\n\
                      \n\
                      guest:x:1001:100::/home/guest:/bin/bash\n";
        assert_eq!(regular_users(passwd), ["developer:1000:1000", "guest:1001:100"]);
    }
}
