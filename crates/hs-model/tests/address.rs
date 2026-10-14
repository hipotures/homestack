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
