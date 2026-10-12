//! Gold readiness (`gold.check_gold_readiness`), Proxmox part (mapping E1).
//!
//! The checks, their order, names and details are Python's. When Gold runs,
//! the guest agent is pinged (audit level); the guest contract behind it
//! (tools, accounts, sudo, key files) needs `exec` and is reported as
//! skipped until phase 3 (mapping E2).

use hs_model::status::{GoldCheck, GoldReadiness};

use crate::error::CoreError;
use crate::inventory::{
    ALIEN_WORKSPACE_TAG, GOLD_TAG, VmConfig, WORKSPACE_TAG, disk_option, disk_volume, has_tag, vm_config, vm_status,
};
use crate::pve::Pve;

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

fn is_disk_slot(slot: &str) -> bool {
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

/// `urllib.parse.unquote`
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |i: usize| bytes.get(i).and_then(|b| (*b as char).to_digit(16));
        match (bytes[index], hex(index + 1), hex(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or(b'?'));
                index += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `workspace_ssh.validate_authorized_keys`: the number of key lines.
pub fn count_authorized_keys(text: &str) -> Result<usize, String> {
    let mut count = 0;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if !["ssh-", "ecdsa-", "sk-"].iter().any(|prefix| line.starts_with(prefix)) {
            let shown: String = line.chars().take(80).collect();
            return Err(format!("Unexpected line in authorized_keys source: '{shown}'"));
        }
        count += 1;
    }
    if count == 0 {
        return Err("No SSH public keys found for workspace user".to_owned());
    }
    Ok(count)
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
    ("security", "No sudo", "required"),
    ("security", "Root authorized_keys", "required"),
    ("security", "Workspace public-key source", "required"),
];
const OPTIONAL_SKIPS: [(&str, &str, &str); 2] = [
    ("guest", "rsync for Setup Files", "optional"),
    ("guest", "git/ssh-keygen for homestack repo", "optional"),
];
const NEEDS_EXEC: &str = "needs guest exec (VM.GuestAgent.Unrestricted); checked from phase 3";

/// `check_gold_readiness`
pub async fn gold_readiness(
    pve: &impl Pve,
    root_disk: &str,
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
        return Ok(readiness(vmid, node, power_state, checks));
    }

    match pve.post(&format!("/nodes/{node}/qemu/{vmid}/agent/ping")).await {
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
        }
        Ok(_) => {
            checks.push(verdict("guest", "QEMU Guest Agent response", true, "responding"));
            checks.push(check("guest", "Required guest tools", "skip", NEEDS_EXEC, "required"));
            for (scope, name, requirement) in OPTIONAL_SKIPS {
                checks.push(check(scope, name, "skip", NEEDS_EXEC, requirement));
            }
            for (scope, name, requirement) in GUEST_SKIPS.iter().skip(1).take(4) {
                checks.push(check(scope, name, "skip", NEEDS_EXEC, requirement));
            }
            if keys_ok {
                checks.push(verdict("security", "Workspace public-key source", true, keys_detail));
            } else {
                checks.push(check(
                    "security",
                    "Workspace public-key source",
                    "skip",
                    NEEDS_EXEC,
                    "required",
                ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_and_boot_order() {
        assert_eq!(
            extract_mac("virtio=bc:24:11:00:01:00,bridge=vmbr0").as_deref(),
            Some("BC:24:11:00:01:00")
        );
        assert_eq!(
            extract_mac("model=x,macaddr=AA:BB:CC:DD:EE:FF").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(extract_mac("bridge=vmbr0"), None);
        assert!(boot_order_contains_disk("order=scsi0;ide2;net0", "scsi0"));
        assert!(!boot_order_contains_disk("order=scsi10", "scsi1"));
    }

    #[test]
    fn authorized_keys_like_python() {
        assert_eq!(
            count_authorized_keys(&unquote("ssh-ed25519%20AAAA%20a%0A%23c%0Aecdsa-x%20B")),
            Ok(2)
        );
        assert!(count_authorized_keys("").is_err());
        assert!(count_authorized_keys("rsa AAAA").is_err());
    }
}
