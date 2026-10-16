//! An isolated guest bridge inside the disposable Proxmox VM.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::{Result, vm::Vm};

pub const CONFIGURE: &str = include_str!("network.sh");

const VERIFY: &str = r#"
set -eu
test -d /sys/class/net/vmbr0/bridge
test "$(sysctl -n net.ipv4.ip_forward)" = 0
test "$(sysctl -n net.ipv6.conf.vmbr0.disable_ipv6)" = 1
# Container veths and VM taps are allowed; hardware ports must stay outside.
for port in /sys/class/net/vmbr0/brif/*; do
    interface=${port##*/}
    test ! -e "/sys/class/net/$interface/device"
done
ip -4 -o address show dev vmbr0 | grep -q 'inet 10.77.0.1/24 '
node=$(hostname -s)
pvesh get "/nodes/$node/network" --output-format json
"#;

fn bridge(inventory: &Value) -> Result<Value> {
    let bridge = inventory
        .as_array()
        .and_then(|items| items.iter().find(|item| item["iface"] == "vmbr0"))
        .ok_or("the isolated vmbr0 bridge is absent from the PVE network inventory")?;
    if bridge["type"] != "bridge"
        || bridge["active"] != 1
        || bridge["autostart"] != 1
        || bridge["cidr"] != "10.77.0.1/24"
        || bridge["bridge_ports"]
            .as_str()
            .is_none_or(|ports| !ports.is_empty() && ports != "none")
    {
        return Err("vmbr0 is not an active, persistent, isolated bridge at 10.77.0.1/24".into());
    }
    Ok(bridge.clone())
}

pub async fn configure(vm: &Vm, artifacts: &Path) -> Result<Value> {
    vm.checked_ssh(CONFIGURE, Duration::from_secs(60), &artifacts.join("guest-network.log"))
        .await?;
    verify(vm, artifacts).await
}

pub async fn verify(vm: &Vm, artifacts: &Path) -> Result<Value> {
    let output = vm
        .checked_ssh(VERIFY, Duration::from_secs(30), &artifacts.join("guest-network.log"))
        .await?;
    bridge(&serde_json::from_slice(&output)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_bridge_must_be_active_persistent_private_and_have_no_external_ports() -> Result<()> {
        let good = json!({"iface": "vmbr0", "type": "bridge", "active": 1,
            "autostart": 1, "cidr": "10.77.0.1/24", "bridge_ports": ""});
        assert_eq!(bridge(&json!([good.clone()]))?, good);
        assert!(bridge(&json!([])).is_err());
        for (key, value) in [
            ("type", json!("eth")),
            ("active", json!(0)),
            ("autostart", json!(0)),
            ("cidr", json!("10.0.2.15/24")),
            ("bridge_ports", json!("ens3")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(bridge(&json!([bad])).is_err(), "accepted invalid {key}");
        }
        Ok(())
    }
}
