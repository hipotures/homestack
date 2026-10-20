//! Gold readiness (`gold.check_gold_readiness`): the Proxmox part (mapping
//! E1) and, when Gold runs, the guest contract (E2).
//!
//! The guest contract runs read-only shell commands through `exec`, which needs
//! `VM.GuestAgent.Unrestricted` on Gold (decisions.md); without it the
//! contract is reported as skipped. A stopped Gold is never started.
//! Sudo may be installed; the workspace account must have no sudo privileges.

use hs_model::config::User;
use hs_model::status::{GoldCheck, GoldReadiness};
use hs_pve::PveError;

use crate::error::CoreError;
use crate::guest::{GUEST_EXEC_TIMEOUT, guest_out, shell_quote};
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, GOLD_TAG, VmConfig, WORKSPACE_TAG, disk_option, disk_volume, has_tag, vm_config, vm_status,
};
use crate::keys::{unquote, validate_authorized_keys};
use crate::pve::Pve;

/// Gold is needed by cloning operations, not by server installation or status.
pub(crate) fn configured_gold(config: &hs_model::config::Config) -> Result<u32, CoreError> {
    config.gold_vmid.ok_or_else(|| {
        CoreError::Refused(
            "Gold is not configured; select a Gold VM before creating or refreshing workspaces".to_owned(),
        )
    })
}

fn check(scope: &str, name: &str, status: &str, detail: impl Into<String>, requirement: &str) -> GoldCheck {
    GoldCheck {
        scope: scope.to_owned(),
        name: name.to_owned(),
        status: status.to_owned(),
        detail: detail.into(),
        requirement: requirement.to_owned(),
    }
}

fn verdict(scope: &str, name: &str, ok: bool, detail: impl Into<String>) -> GoldCheck {
    check(scope, name, if ok { "pass" } else { "fail" }, detail, "required")
}

/// `_agent_enabled`
#[must_use]
pub fn agent_enabled(value: Option<&str>) -> bool {
    let text = value.unwrap_or_default().trim();
    text == "1"
        || text
            .split(',')
            .any(|token| matches!(token.trim().to_lowercase().as_str(), "enabled=1" | "enabled=true"))
}

pub(crate) fn is_disk_slot(slot: &str) -> bool {
    ["ide", "sata", "scsi", "virtio"].iter().any(|prefix| {
        slot.strip_prefix(prefix)
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// `_cloud_init_slots`
#[must_use]
pub fn cloud_init_slots(vm_cfg: &VmConfig) -> Vec<String> {
    vm_cfg
        .iter()
        .filter(|(slot, value)| {
            is_disk_slot(slot)
                && disk_volume(value).contains("cloudinit")
                && disk_option(value, "media").as_deref() == Some("cdrom")
        })
        .map(|(slot, _)| slot.clone())
        .collect()
}

/// `_extra_data_disks`
fn extra_data_disks(vm_cfg: &VmConfig, root_disk: &str) -> Vec<String> {
    let cloud = cloud_init_slots(vm_cfg);
    vm_cfg
        .iter()
        .filter(|(slot, value)| {
            let volume = disk_volume(value);
            is_disk_slot(slot) && *slot != root_disk && !cloud.contains(slot) && !volume.is_empty() && volume != "none"
        })
        .map(|(slot, _)| slot.clone())
        .collect()
}

/// `_extra_nics`: `netN` for N ≥ 1 with a value.
fn extra_nics(vm_cfg: &VmConfig) -> Vec<String> {
    vm_cfg
        .iter()
        .filter(|(key, value)| {
            key.strip_prefix("net").is_some_and(|index| {
                index.bytes().next().is_some_and(|b| (b'1'..=b'9').contains(&b))
                    && index.bytes().all(|b| b.is_ascii_digit())
            }) && !value.trim().is_empty()
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn is_mac(text: &str) -> bool {
    text.len() == 17
        && text.split(':').count() == 6
        && text
            .split(':')
            .all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `guest.extract_mac`: the MAC of a NIC model, else any MAC in `net0`.
#[must_use]
pub fn extract_mac(net0: &str) -> Option<String> {
    let model = net0.split(',').find_map(|item| {
        let (key, value) = item.trim().split_once('=')?;
        (matches!(key, "virtio" | "e1000" | "rtl8139") && is_mac(value)).then_some(value)
    });
    let any = || {
        net0.split(|c: char| !(c.is_ascii_hexdigit() || c == ':'))
            .find(|token| is_mac(token))
    };
    model.or_else(any).map(str::to_uppercase)
}

/// `boot_order_contains_disk`
#[must_use]
pub fn boot_order_contains_disk(boot: &str, disk: &str) -> bool {
    boot.split(',')
        .filter_map(|item| item.split_once('='))
        .find(|(key, _)| *key == "order")
        .is_some_and(|(_, order)| order.split(';').any(|token| token == disk))
}

/// `workspace_ssh.validate_authorized_keys`: the number of key lines.
pub fn count_authorized_keys(text: &str) -> Result<usize, String> {
    validate_authorized_keys(text)
        .map(|keys| keys.lines().count())
        .map_err(|error| error.to_string())
}

/// `_pve_ssh_keys_usable`
fn pve_ssh_keys(vm_cfg: &VmConfig) -> (bool, String) {
    match vm_cfg.get("sshkeys").filter(|keys| !keys.is_empty()) {
        None => (false, "no Proxmox sshkeys configured".to_owned()),
        Some(raw) => match count_authorized_keys(&unquote(raw)) {
            Ok(count) => (true, format!("{count} public key(s) in Proxmox sshkeys")),
            Err(error) => (false, error),
        },
    }
}

const GUEST_SKIPS: [(&str, &str, &str); 6] = [
    ("guest", "Required guest tools", "required"),
    ("guest", "Workspace account", "required"),
    ("security", "Regular user policy", "required"),
    ("security", "No sudo privileges", "required"),
    ("security", "Root authorized_keys", "required"),
    ("security", "Workspace public-key source", "required"),
];

fn sudo_policy_command(user: &str) -> String {
    let user = shell_quote(user);
    format!(
        "if ! command -v sudo >/dev/null 2>&1; then echo DENIED; else policy=$(LC_ALL=C sudo -n -l -U {user} 2>&1); case \"$policy\" in \"User \"{user}\" is not allowed to run sudo on \"*.) echo DENIED;; *) echo UNVERIFIED;; esac; fi"
    )
}
const OPTIONAL_SKIPS: [(&str, &str, &str); 2] = [
    ("guest", "rsync for Setup Files", "optional"),
    ("guest", "git/ssh-keygen for homestack repo", "optional"),
];
const REQUIRED_TOOLS: [&str; 12] = [
    "cloud-init",
    "sshd",
    "nmcli",
    "lsblk",
    "blkid",
    "wipefs",
    "mkfs.ext4",
    "mount",
    "mountpoint",
    "systemctl",
    "systemd-escape",
    "install",
];

/// Not in Python: what the vendor snippet needs in Gold (decision 1 = A,
/// phase 4b). Its `bootcmd` makes NetworkManager netplan's renderer and runs
/// `netplan generate` and `udevadm trigger`; cloud-init itself is Python and
/// reads YAML. Checked after Python's guest contract.
const VENDOR_TOOLS: [&str; 4] = ["netplan", "udevadm", "NetworkManager", "python3"];
const PYTHON_YAML: &str = "python3 -c 'import yaml' >/dev/null 2>&1 && echo OK || echo MISSING";
const VENDOR_CHECK: (&str, &str, &str) = ("guest", "Cloud-Init vendor snippet tools", "required");

fn vendor_skip(detail: &str) -> GoldCheck {
    check(VENDOR_CHECK.0, VENDOR_CHECK.1, "skip", detail, VENDOR_CHECK.2)
}

/// Python's loop printing the commands `command -v` does not find.
fn missing_loop(tools: &[&str]) -> String {
    format!(
        r#"for c in {}; do command -v "$c" >/dev/null 2>&1 || printf "%s\n" "$c"; done"#,
        tools.join(" ")
    )
}

/// `guest_out_on_node(..., check=False)`, except that a refused call (no
/// `VM.GuestAgent.Unrestricted`) ends the guest contract.
async fn guest_value(pve: &impl Pve, node: &str, vmid: u32, command: &str) -> Result<String, Refused> {
    match guest_out(pve, node, vmid, command, false, GUEST_EXEC_TIMEOUT).await {
        Ok(out) => Ok(out),
        Err(CoreError::Pve(PveError::Forbidden { .. } | PveError::Unauthorized)) => Err(Refused),
        Err(error) => Ok(error.to_string()),
    }
}

/// The token may not run commands in Gold.
struct Refused;

/// The guest and security checks of a running Gold whose agent answers, in
/// Python's order.
async fn guest_contract(
    pve: &impl Pve,
    user: &User,
    node: &str,
    vmid: u32,
    keys: (bool, String),
    checks: &mut Vec<GoldCheck>,
) -> Result<(), Refused> {
    let missing_tools = guest_value(pve, node, vmid, &missing_loop(&REQUIRED_TOOLS)).await?;
    let missing_tools: Vec<&str> = missing_tools.lines().collect();
    checks.push(verdict(
        "guest",
        "Required guest tools",
        missing_tools.is_empty(),
        if missing_tools.is_empty() {
            "all present".to_owned()
        } else {
            format!("missing: {}", missing_tools.join(", "))
        },
    ));
    let rsync = guest_value(
        pve,
        node,
        vmid,
        "if command -v rsync >/dev/null 2>&1; then echo PRESENT; else echo ABSENT; fi",
    )
    .await?;
    checks.push(check(
        "guest",
        "rsync for Setup Files",
        if rsync == "PRESENT" { "pass" } else { "fail" },
        if rsync == "PRESENT" {
            "installed"
        } else {
            "not installed; Setup Files will be unavailable"
        },
        "optional",
    ));
    let missing_repo = guest_value(pve, node, vmid, &missing_loop(&["git", "ssh-keygen"])).await?;
    let missing_repo: Vec<&str> = missing_repo.lines().collect();
    checks.push(check(
        "guest",
        "git/ssh-keygen for homestack repo",
        if missing_repo.is_empty() { "pass" } else { "fail" },
        if missing_repo.is_empty() {
            "all present".to_owned()
        } else {
            format!("missing: {}", missing_repo.join(", "))
        },
        "optional",
    ));

    let name = shell_quote(&user.name);
    let account = guest_value(pve, node, vmid, &format!("getent passwd {name} || true")).await?;
    let fields: Vec<&str> = account.split(':').collect();
    let account_ok = fields.len() >= 4
        && fields[0] == user.name
        && fields[2] == user.uid.to_string()
        && fields[3] == user.gid.to_string();
    checks.push(verdict(
        "guest",
        "Workspace account",
        account_ok,
        format!("{} ({}:{})", user.name, user.uid, user.gid),
    ));
    let regular = guest_value(
        pve,
        node,
        vmid,
        r#"awk -F: '$3 >= 1000 && $3 < 65534 {print $1 ":" $3 ":" $4}' /etc/passwd"#,
    )
    .await?;
    let regular: Vec<&str> = regular.lines().collect();
    let expected = format!("{}:{}:{}", user.name, user.uid, user.gid);
    checks.push(verdict(
        "security",
        "Regular user policy",
        regular == [expected.as_str()],
        if regular.is_empty() {
            "none".to_owned()
        } else {
            regular.join(", ")
        },
    ));
    let sudo = guest_value(pve, node, vmid, &sudo_policy_command(&user.name)).await?;
    checks.push(verdict(
        "security",
        "No sudo privileges",
        sudo == "DENIED",
        if sudo == "DENIED" {
            "no permitted sudo commands"
        } else {
            "sudo privileges present or policy could not be verified"
        },
    ));
    let root_keys = guest_value(
        pve,
        node,
        vmid,
        "test -s /root/.ssh/authorized_keys && echo OK || echo MISSING",
    )
    .await?;
    checks.push(verdict(
        "security",
        "Root authorized_keys",
        root_keys == "OK",
        if root_keys == "OK" { "present" } else { "missing" },
    ));
    let (mut keys_ok, mut keys_detail) = keys;
    if !keys_ok {
        let text = guest_value(
            pve,
            node,
            vmid,
            &format!("cat /home/{name}/.ssh/authorized_keys 2>/dev/null || true"),
        )
        .await?;
        match count_authorized_keys(&text) {
            Ok(count) => {
                keys_ok = true;
                keys_detail = format!("{count} public key(s) in /home/{}/.ssh/authorized_keys", user.name);
            }
            Err(error) => keys_detail = error,
        }
    }
    checks.push(verdict("security", "Workspace public-key source", keys_ok, keys_detail));
    let vendor = guest_value(pve, node, vmid, &missing_loop(&VENDOR_TOOLS)).await?;
    let mut missing: Vec<&str> = vendor.lines().collect();
    if guest_value(pve, node, vmid, PYTHON_YAML).await? != "OK" {
        missing.push("python3-yaml");
    }
    checks.push(verdict(
        VENDOR_CHECK.0,
        VENDOR_CHECK.1,
        missing.is_empty(),
        if missing.is_empty() {
            "all present".to_owned()
        } else {
            format!("missing: {}", missing.join(", "))
        },
    ));
    Ok(())
}

/// `check_gold_readiness`
pub async fn gold_readiness(
    pve: &impl Pve,
    root_disk: &str,
    user: &User,
    node: &str,
    vmid: u32,
) -> Result<GoldReadiness, CoreError> {
    let vm_cfg = vm_config(pve, node, vmid).await?;
    let tags = vm_cfg.get("tags").map(String::as_str);
    let mut checks = vec![
        verdict(
            "PVE",
            "Role tag",
            has_tag(tags, GOLD_TAG) && !has_tag(tags, WORKSPACE_TAG) && !has_tag(tags, ALIEN_WORKSPACE_TAG),
            format!("requires {GOLD_TAG} without {WORKSPACE_TAG} or {ALIEN_WORKSPACE_TAG}"),
        ),
        verdict(
            "PVE",
            "Disposable root",
            vm_cfg.get(root_disk).is_some_and(|v| !v.is_empty()),
            root_disk,
        ),
    ];
    let extra = extra_data_disks(&vm_cfg, root_disk);
    checks.push(verdict(
        "PVE",
        "Extra data disks",
        extra.is_empty(),
        if extra.is_empty() {
            "none".to_owned()
        } else {
            extra.join(", ")
        },
    ));
    let mac = vm_cfg
        .get("net0")
        .filter(|n| !n.is_empty())
        .and_then(|net0| extract_mac(net0));
    checks.push(verdict(
        "PVE",
        "Primary NIC",
        mac.is_some(),
        mac.as_ref()
            .map_or_else(|| "net0 missing or invalid".to_owned(), |mac| format!("net0 {mac}")),
    ));
    let nics = extra_nics(&vm_cfg);
    checks.push(verdict(
        "PVE",
        "Extra NICs",
        nics.is_empty(),
        if nics.is_empty() {
            "none".to_owned()
        } else {
            nics.join(", ")
        },
    ));
    let cloud = cloud_init_slots(&vm_cfg);
    checks.push(verdict(
        "PVE",
        "Cloud-Init drive",
        !cloud.is_empty(),
        if cloud.is_empty() {
            "missing".to_owned()
        } else {
            cloud.join(", ")
        },
    ));
    let agent = vm_cfg.get("agent").filter(|a| !a.is_empty());
    checks.push(verdict(
        "PVE",
        "QEMU Guest Agent option",
        agent_enabled(agent.map(String::as_str)),
        agent.cloned().unwrap_or_else(|| "disabled".to_owned()),
    ));
    let boot = vm_cfg.get("boot").cloned().unwrap_or_default();
    checks.push(verdict(
        "PVE",
        "Boot order",
        boot_order_contains_disk(&boot, root_disk),
        if boot.is_empty() {
            "not configured".to_owned()
        } else {
            boot
        },
    ));
    let power_state = vm_status(pve, node, vmid).await;
    checks.push(verdict(
        "PVE",
        "Power state",
        matches!(power_state.as_str(), "running" | "stopped"),
        power_state.clone(),
    ));
    let (keys_ok, keys_detail) = pve_ssh_keys(&vm_cfg);

    if power_state != "running" {
        checks.push(verdict(
            "PVE",
            "Workspace public-key source",
            keys_ok,
            format!("{keys_detail}; required while Gold is stopped"),
        ));
        let detail = format!("not inspected because Gold is {power_state}");
        checks.push(check("guest", "QEMU Guest Agent response", "skip", &detail, "required"));
        for (scope, name, requirement) in GUEST_SKIPS.iter().take(5).chain(OPTIONAL_SKIPS.iter()) {
            checks.push(check(scope, name, "skip", &detail, requirement));
        }
        checks.push(vendor_skip(&detail));
        return Ok(readiness(vmid, node, power_state, checks));
    }

    match pve.post(&format!("/nodes/{node}/qemu/{vmid}/agent/ping"), &[]).await {
        Err(error) => {
            let detail = match &error {
                hs_pve::PveError::Api { message, .. }
                | hs_pve::PveError::Forbidden { message }
                | hs_pve::PveError::NodeUnreachable { message } => message.trim().to_owned(),
                other => other.to_string(),
            };
            checks.push(verdict("guest", "QEMU Guest Agent response", false, detail));
            for (scope, name, requirement) in GUEST_SKIPS.iter().chain(OPTIONAL_SKIPS.iter()) {
                checks.push(check(scope, name, "skip", "QEMU Guest Agent unavailable", requirement));
            }
            checks.push(vendor_skip("QEMU Guest Agent unavailable"));
        }
        Ok(_) => {
            checks.push(verdict("guest", "QEMU Guest Agent response", true, "responding"));
            let mut contract = Vec::new();
            let keys = (keys_ok, keys_detail.clone());
            if guest_contract(pve, user, node, vmid, keys, &mut contract).await.is_ok() {
                checks.extend(contract);
            } else {
                let detail = format!("needs VM.GuestAgent.Unrestricted on /vms/{vmid} to run commands in Gold");
                checks.push(check("guest", "Required guest tools", "skip", &detail, "required"));
                for (scope, name, requirement) in OPTIONAL_SKIPS {
                    checks.push(check(scope, name, "skip", &detail, requirement));
                }
                for (scope, name, requirement) in GUEST_SKIPS.iter().skip(1).take(4) {
                    checks.push(check(scope, name, "skip", &detail, requirement));
                }
                if keys_ok {
                    checks.push(verdict("security", "Workspace public-key source", true, keys_detail));
                } else {
                    checks.push(check(
                        "security",
                        "Workspace public-key source",
                        "skip",
                        &detail,
                        "required",
                    ));
                }
                checks.push(vendor_skip(&detail));
            }
        }
    }
    Ok(readiness(vmid, node, power_state, checks))
}

fn readiness(vmid: u32, node: &str, power_state: String, checks: Vec<GoldCheck>) -> GoldReadiness {
    GoldReadiness {
        vmid,
        node: node.to_owned(),
        power_state,
        ok: !checks.iter().any(|c| c.status == "fail" && c.requirement == "required"),
        guest_checked: checks.iter().any(|c| c.scope == "guest" && c.status != "skip"),
        checks,
    }
}
