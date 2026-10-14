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
