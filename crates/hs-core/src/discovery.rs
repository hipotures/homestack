//! The Proxmox part of `discovery.discover_environment` (mapping A5) and
//! `discovery.discover_workspace_networks` (A6).

use std::net::Ipv4Addr;

use hs_model::status::{Discovery, NetworkCandidate};
use serde_json::{Map, Value};

use crate::error::CoreError;
use crate::inventory::{
    GOLD_TAG, StatusResource, WORKSPACE_TAG, cluster_node_statuses, cluster_vm_maps, has_tag, node_dns, node_network,
    py_str, storage_definitions, text_or_none, vm_config,
};
use crate::pve::Pve;

fn pick(map: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| map.get(*key).map(|value| ((*key).to_owned(), value.clone())))
        .collect()
}

/// Version, nodes, Gold candidates and storage definitions, read-only.
pub async fn discovery(pve: &impl Pve) -> Result<Discovery, CoreError> {
    let version = pve.get("/version", &[]).await?;
    if !version.is_object() {
        return Err(CoreError::Unusable(
            "Proxmox version query did not return a JSON object".to_owned(),
        ));
    }
    let nodes = cluster_node_statuses(pve).await?;
    let mut resources = cluster_vm_maps(pve).await?;
    resources.sort_by_key(|(vmid, _)| *vmid);
    let definitions = storage_definitions(pve).await?;
    Ok(Discovery {
        proxmox_version: version,
        nodes,
        gold_candidates: resources
            .iter()
            .filter(|(_, item)| has_tag(text_or_none(item.get("tags")).as_deref(), GOLD_TAG))
            .map(|(_, item)| pick(item, &["vmid", "name", "node", "status", "tags"]))
            .collect(),
        storages: definitions
            .iter()
            .map(|item| pick(item, &["storage", "type", "content", "nodes", "shared"]))
            .collect(),
    })
}

/// `_config_option`
fn config_option(value: Option<&str>, key: &str) -> Option<String> {
    value?
        .split(',')
        .filter_map(|item| item.trim().split_once('='))
        .find(|(item_key, _)| *item_key == key)
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// An IPv4 network from `address/prefix` or `address/netmask`
/// (`ipaddress.IPv4Interface(...).network`).
fn ipv4_network(text: &str) -> Option<(Ipv4Addr, u32)> {
    let (address, suffix) = text.split_once('/').unwrap_or((text, "32"));
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix = match suffix.parse::<u32>() {
        Ok(prefix) if prefix <= 32 => prefix,
        Ok(_) => return None,
        Err(_) => {
            let mask = u32::from(suffix.parse::<Ipv4Addr>().ok()?);
            if mask.leading_ones() + mask.trailing_zeros() != 32 {
                return None;
            }
            mask.leading_ones()
        }
    };
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((Ipv4Addr::from(u32::from(address) & mask), prefix))
}

/// `_mapping_compatible`: the scheme `prefix.<vmid>` needs a `.0` network
/// of at most /24.
fn compatible((network, prefix): (Ipv4Addr, u32)) -> Option<String> {
    (network.octets()[3] == 0 && prefix <= 24).then(|| format!("{network}/{prefix}"))
}

/// `_network_from_ipconfig`
fn network_from_ipconfig(value: Option<&str>) -> Option<(String, Option<String>)> {
    let ip = config_option(value, "ip")?;
    if ip == "dhcp" || ip == "manual" {
        return None;
    }
    let network = compatible(ipv4_network(&ip)?)?;
    Some((network, config_option(value, "gw")))
}

/// `_network_from_pve_interface`
fn network_from_interface(item: &Map<String, Value>) -> Option<String> {
    let text = |key: &str| {
        text_or_none(item.get(key))
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let address = text("address");
    let interface = match (&address, text("cidr"), text("netmask")) {
        (Some(address), _, _) if address.contains('/') => address.clone(),
        (Some(_), Some(cidr), _) if cidr.contains('/') => cidr,
        (Some(address), Some(cidr), _) => format!("{address}/{cidr}"),
        (Some(address), None, Some(netmask)) => format!("{address}/{netmask}"),
        _ => return None,
    };
    compatible(ipv4_network(&interface)?)
}

fn unique_ipv4(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        if value.parse::<Ipv4Addr>().is_ok() && !result.contains(&value) {
            result.push(value);
        }
    }
    result
}

/// `_parse_dns_values`
fn parse_dns_values(value: Option<&str>) -> Vec<String> {
    unique_ipv4(
        value
            .unwrap_or_default()
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
    )
}

/// `_pve_dns_servers`
fn pve_dns_servers(data: &Map<String, Value>) -> Vec<String> {
    unique_ipv4(
        ["dns1", "dns2", "dns3"]
            .iter()
            .filter_map(|key| text_or_none(data.get(*key)))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    )
}

/// `discover_workspace_networks`: network profiles compatible with the Gold
/// VM's bridge, from existing workspaces, Gold itself and the node's
/// bridges, in that order of preference.
pub async fn workspace_networks(
    pve: &impl Pve,
    resources: &[StatusResource],
    gold_vmid: u32,
    gold_node: &str,
) -> Result<Vec<NetworkCandidate>, CoreError> {
    let gold_cfg = vm_config(pve, gold_node, gold_vmid).await?;
    let gold_bridge = config_option(gold_cfg.get("net0").map(String::as_str), "bridge");
    let dns_fallback = match node_dns(pve, gold_node).await {
        Ok(data) => pve_dns_servers(&data),
        Err(_) => Vec::new(),
    };
    let mut candidates: Vec<NetworkCandidate> = Vec::new();
    let mut add = |candidate: NetworkCandidate| match candidates
        .iter_mut()
        .find(|existing| existing.cidr == candidate.cidr && existing.bridge == candidate.bridge)
    {
        Some(existing) => {
            if existing.gateway.is_none() {
                existing.gateway = candidate.gateway;
            }
            if existing.dns_servers.is_empty() {
                existing.dns_servers = candidate.dns_servers;
            }
        }
        None => candidates.push(candidate),
    };

    for resource in resources.iter().filter(|r| has_tag(r.tags.as_deref(), WORKSPACE_TAG)) {
        let node = resource.node.trim();
        if node.is_empty() {
            continue;
        }
        let Ok(vm_cfg) = vm_config(pve, node, resource.vmid).await else {
            continue;
        };
        let bridge = config_option(vm_cfg.get("net0").map(String::as_str), "bridge");
        if let (Some(gold), Some(bridge)) = (&gold_bridge, &bridge)
            && gold != bridge
        {
            continue;
        }
        let Some((cidr, gateway)) = network_from_ipconfig(vm_cfg.get("ipconfig0").map(String::as_str)) else {
            continue;
        };
        let dns = parse_dns_values(vm_cfg.get("nameserver").map(String::as_str));
        add(NetworkCandidate {
            cidr,
            bridge: bridge.or_else(|| gold_bridge.clone()),
            gateway,
            dns_servers: if dns.is_empty() { dns_fallback.clone() } else { dns },
            source: format!("HomeStack workspace VM {}", resource.vmid),
        });
    }

    if let Some((cidr, gateway)) = network_from_ipconfig(gold_cfg.get("ipconfig0").map(String::as_str)) {
        let dns = parse_dns_values(gold_cfg.get("nameserver").map(String::as_str));
        add(NetworkCandidate {
            cidr,
            bridge: gold_bridge.clone(),
            gateway,
            dns_servers: if dns.is_empty() { dns_fallback.clone() } else { dns },
            source: format!("Gold VM {gold_vmid}"),
        });
    }

    let interfaces = node_network(pve, gold_node).await.unwrap_or_default();
    for item in &interfaces {
        let bridge = item.get("iface").map(py_str).unwrap_or_default().trim().to_owned();
        let kind = item.get("type").map(py_str).unwrap_or_default().trim().to_lowercase();
        if kind != "bridge" && kind != "ovsbridge" {
            continue;
        }
        if gold_bridge.as_ref().is_some_and(|gold| *gold != bridge) {
            continue;
        }
        let Some(cidr) = network_from_interface(item) else {
            continue;
        };
        let gateway = text_or_none(item.get("gateway"))
            .map(|g| g.trim().to_owned())
            .filter(|g| !g.is_empty());
        add(NetworkCandidate {
            cidr,
            bridge: (!bridge.is_empty()).then(|| bridge.clone()),
            gateway,
            dns_servers: dns_fallback.clone(),
            source: format!("PVE bridge {bridge} on {gold_node}"),
        });
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_like_ipaddress() {
        assert_eq!(
            ipv4_network("192.168.100.200/24"),
            Some((Ipv4Addr::new(192, 168, 100, 0), 24))
        );
        assert_eq!(
            ipv4_network("192.168.1.11/255.255.255.0"),
            Some((Ipv4Addr::new(192, 168, 1, 0), 24))
        );
        assert_eq!(ipv4_network("10.0.0.1"), Some((Ipv4Addr::new(10, 0, 0, 1), 32)));
        assert_eq!(ipv4_network("10.0.0.1/255.0.255.0"), None);
        assert_eq!(compatible((Ipv4Addr::new(10, 0, 0, 0), 25)), None);
        assert_eq!(
            compatible((Ipv4Addr::new(10, 0, 0, 0), 16)).as_deref(),
            Some("10.0.0.0/16")
        );
        assert_eq!(
            parse_dns_values(Some("1.1.1.1, 8.8.8.8;x 1.1.1.1")),
            ["1.1.1.1", "8.8.8.8"]
        );
    }
}
