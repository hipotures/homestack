//! A machine's number follows from its address (owner's decision of
//! 2026-10-14): the VMID is the last two octets, the third times 1000 plus
//! the fourth, so `192.168.101.10` is VMID 101010.

use std::net::Ipv4Addr;

/// The VMID of `address`.
#[must_use]
pub fn vmid_of(address: Ipv4Addr) -> u32 {
    let [_, _, third, fourth] = address.octets();
    u32::from(third) * 1000 + u32::from(fourth)
}

/// The address of workspace `vmid` in `network`: VMIDs 2–254 are
/// `<prefix>.<vmid>` (as before, and as in Python); from 1000 the third
/// octet is `vmid / 1000` and the fourth `vmid % 1000`, under the first two
/// of the gateway's network. `None` outside the gateway's network, for a
/// fourth octet of 0 or 255 and for VMIDs in between.
#[must_use]
pub fn address_of(vmid: u32, network: &crate::config::Network) -> Option<Ipv4Addr> {
    let gateway: Ipv4Addr = network.gateway.parse().ok()?;
    let address = if (2..=254).contains(&vmid) {
        format!("{}.{vmid}", network.prefix).parse().ok()?
    } else {
        let (third, fourth) = (vmid / 1000, vmid % 1000);
        if vmid < 1000 || third > 255 || !(1..=254).contains(&fourth) {
            return None;
        }
        let [first, second, _, _] = gateway.octets();
        Ipv4Addr::new(first, second, u8::try_from(third).ok()?, u8::try_from(fourth).ok()?)
    };
    let mask = u32::MAX
        .checked_shl(32_u32.checked_sub(u32::from(network.cidr))?)
        .unwrap_or(0);
    let start = u32::from(gateway) & mask;
    let ip = u32::from(address);
    (ip & mask == start && ip != start && ip != (start | !mask)).then_some(address)
}
