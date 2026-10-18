#!/bin/sh
set -eu
test -f /etc/homestack-e2e.json
test -d /sys/class/net/vmbr0/bridge
export DEBIAN_FRONTEND=noninteractive
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install dnsmasq-base nftables
install -d -m 0755 /etc/homestack-e2e /usr/local/lib/homestack-e2e
install -d -m 0755 -o nobody -g nogroup /var/lib/homestack-e2e
cat > /usr/local/lib/homestack-e2e/internet-link.py <<'LINK'
@INTERNET_LINK@
LINK
cat > /usr/local/lib/homestack-e2e/network-check.py <<'CHECK'
@NETWORK_CHECK@
CHECK
cat > /etc/homestack-e2e/dns.conf <<'DNS'
no-resolv
server=10.78.0.3
no-poll
no-hosts
local=/e2e.homestack.app/
bind-interfaces
interface=vmbr0
listen-address=10.77.0.1
user=nobody
group=nogroup
domain=e2e.homestack.app
dhcp-authoritative
dhcp-range=10.77.0.50,10.77.0.99,255.255.255.0,1h
dhcp-option=option:router,10.77.0.1
dhcp-option=option:dns-server,10.77.0.1
dhcp-option=option:domain-search,e2e.homestack.app
dhcp-leasefile=/var/lib/homestack-e2e/dhcp.leases
log-dhcp
host-record=pve.e2e.homestack.app,10.77.0.1
host-record=pve1.e2e.homestack.app,10.77.0.1
host-record=api1.e2e.homestack.app,10.77.0.101
@CLUSTER_DNS@
DNS
cat > /etc/systemd/system/homestack-e2e-dns.service <<'UNIT'
[Unit]
Description=Internal DNS and DHCP for the disposable HomeStack E2E lab
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=/usr/sbin/dnsmasq --keep-in-foreground --conf-file=/etc/homestack-e2e/dns.conf
Restart=on-failure
[Install]
WantedBy=multi-user.target
UNIT
cat > /etc/systemd/system/homestack-e2e-internet.service <<'UNIT'
[Unit]
Description=Private uplink to the externally filtered E2E Internet gateway
After=network-online.target
Wants=network-online.target
[Service]
ExecStartPre=/bin/sh /usr/local/lib/homestack-e2e/internet-rules.sh
ExecStart=/usr/bin/python3 -u /usr/local/lib/homestack-e2e/internet-link.py
Restart=on-failure
RestartSec=1
[Install]
WantedBy=multi-user.target
UNIT
cat > /etc/homestack-e2e/internet.nft <<'NFT'
table ip homestack_e2e {
    chain forward {
        type filter hook forward priority 0; policy accept;
        iifname "vmbr0" oifname "vmbr0" accept
        iifname "vmbr0" oifname "hsewan0" accept
        iifname "hsewan0" oifname "vmbr0" ct state established,related accept
        iifname "vmbr0" drop
        oifname "vmbr0" drop
    }
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr 10.77.0.0/24 oifname "hsewan0" masquerade
    }
}
NFT
# Restore only this lab's table on startup, including after a PVE reboot.
cat > /usr/local/lib/homestack-e2e/internet-rules.sh <<'RULES'
set -eu
test -f /etc/homestack-e2e.json
if nft list table ip homestack_e2e >/dev/null 2>&1; then
    { printf 'delete table ip homestack_e2e\n'; cat /etc/homestack-e2e/internet.nft; } | nft -f -
else
    nft -f /etc/homestack-e2e/internet.nft
fi
RULES
printf 'net.ipv4.ip_forward=1\n' > /etc/sysctl.d/80homestack-e2e-internet.conf
sysctl -q -p /etc/sysctl.d/80homestack-e2e-internet.conf
dnsmasq --test --conf-file=/etc/homestack-e2e/dns.conf
systemctl daemon-reload
systemctl enable --now homestack-e2e-dns.service homestack-e2e-internet.service
systemctl restart homestack-e2e-dns.service homestack-e2e-internet.service
systemctl is-active --quiet homestack-e2e-dns.service
systemctl is-active --quiet homestack-e2e-internet.service
python3 - 10.77.0.1 53 <<'DNSCHECK'
import socket
import struct
import sys
import time

# systemd considers the foreground daemon started before its sockets are bound.
for attempt in range(50):
    try:
        with socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=0.2):
            break
    except OSError:
        if attempt == 49:
            raise
        time.sleep(0.1)

# musl resolves A and AAAA together; both must succeed for IPv4-only names.
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as dns:
    dns.settimeout(2)
    dns.connect((sys.argv[1], int(sys.argv[2])))
    for name in ("pve.e2e.homestack.app", "pve1.e2e.homestack.app", "api1.e2e.homestack.app"):
        question = b"".join(bytes([len(label)]) + label.encode() for label in name.split(".")) + b"\0"
        for kind in (1, 28):
            query = struct.pack("!6H", kind, 0x100, 1, 0, 0, 0) + question + struct.pack("!HH", kind, 1)
            dns.send(query)
            reply = dns.recv(4096)
            identifier, flags, questions, answers, _, _ = struct.unpack("!6H", reply[:12])
            if identifier != kind or not flags & 0x8000 or flags & 0xF or questions != 1:
                raise RuntimeError(f"E2E DNS failed for {name}, type {kind}: {reply[:12].hex()}")
            if answers != (1 if kind == 1 else 0):
                raise RuntimeError(f"E2E DNS returned unexpected answers for {name}, type {kind}")
print("E2E DNS verified: IPv4 records and empty successful IPv6 answers")
DNSCHECK
test "$(sysctl -n net.ipv4.ip_forward)" = 1
