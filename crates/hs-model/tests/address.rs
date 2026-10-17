use std::net::Ipv4Addr;

use hs_model::address::vmid_of;

#[test]
fn the_vmid_is_the_last_two_octets() {
    assert_eq!(vmid_of(Ipv4Addr::new(192, 168, 101, 10)), 101_010);
    assert_eq!(vmid_of(Ipv4Addr::new(192, 168, 102, 1)), 102_001);
    assert_eq!(vmid_of(Ipv4Addr::new(192, 168, 101, 254)), 101_254);
    assert_eq!(vmid_of(Ipv4Addr::new(192, 0, 2, 97)), 2097);
    assert_eq!(vmid_of(Ipv4Addr::new(10, 0, 0, 5)), 5);
}

#[test]
fn short_vmids_must_also_be_usable_in_the_configured_subnet() {
    use hs_model::address::address_of;
    assert_eq!(address_of(100, &network(25)), Some(Ipv4Addr::new(192, 168, 100, 100)));
    assert_eq!(address_of(127, &network(25)), None);
    assert_eq!(address_of(128, &network(25)), None);
    assert_eq!(address_of(200, &network(25)), None);
}

fn network(cidr: u8) -> hs_model::config::Network {
    hs_model::config::Network {
        prefix: "192.168.100".to_owned(),
        cidr,
        gateway: "192.168.100.1".to_owned(),
        dns: vec!["192.168.100.1".to_owned()],
    }
}

#[test]
fn a_small_vmid_keeps_its_old_address_and_a_large_one_is_its_last_two_octets() {
    use hs_model::address::address_of;
    let wide = network(22);
    // VMIDs 2–254: <prefix>.<vmid>, as before (and as in Python).
    assert_eq!(address_of(240, &wide), Some(Ipv4Addr::new(192, 168, 100, 240)));
    // From 1000: the third octet times 1000 plus the fourth.
    assert_eq!(address_of(101_010, &wide), Some(Ipv4Addr::new(192, 168, 101, 10)));
    assert_eq!(address_of(102_001, &wide), Some(Ipv4Addr::new(192, 168, 102, 1)));
    assert_eq!(address_of(101_254, &wide), Some(Ipv4Addr::new(192, 168, 101, 254)));
    // Nothing outside the gateway's network, no .0 or .255, nothing between.
    assert_eq!(address_of(104_010, &wide), None);
    assert_eq!(address_of(101_000, &wide), None);
    assert_eq!(address_of(101_255, &wide), None);
    assert_eq!(address_of(500, &wide), None);
    assert_eq!(address_of(1, &wide), None);
    // A /24 has no 192.168.101.x.
    assert_eq!(address_of(101_010, &network(24)), None);
    assert_eq!(
        address_of(100_010, &network(24)),
        Some(Ipv4Addr::new(192, 168, 100, 10))
    );
}
