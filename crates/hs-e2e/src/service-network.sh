#!/bin/sh
set -eu
test -f /etc/homestack-e2e.json
test -d /sys/class/net/vmbr0/bridge
test "$(sysctl -n net.ipv4.ip_forward)" = 0
export DEBIAN_FRONTEND=noninteractive
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install dnsmasq-base skopeo
install -d -m 0755 /etc/homestack-e2e
cat > /etc/homestack-e2e/dns.conf <<'DNS'
no-resolv
no-poll
no-hosts
local=/e2e.homestack.app/
bind-interfaces
interface=vmbr0
listen-address=10.77.0.1
host-record=pve.e2e.homestack.app,10.77.0.1
host-record=pve1.e2e.homestack.app,10.77.0.1
host-record=api1.e2e.homestack.app,10.77.0.101
DNS
cat > /etc/systemd/system/homestack-e2e-dns.service <<'UNIT'
[Unit]
Description=Internal DNS for the disposable HomeStack E2E lab
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=/usr/sbin/dnsmasq --keep-in-foreground --conf-file=/etc/homestack-e2e/dns.conf
Restart=on-failure
[Install]
WantedBy=multi-user.target
UNIT
if ! grep -q 'api1.e2e.homestack.app' /etc/hosts; then
    printf '\n10.77.0.101 api1.e2e.homestack.app\n' >> /etc/hosts
fi
if ! grep -q 'pve1.e2e.homestack.app' /etc/hosts; then
    printf '\n10.77.0.1 pve1.e2e.homestack.app\n' >> /etc/hosts
fi
# Only the Proxmox OCI worker uses this proxy. APT retains its separate,
# read-only repository proxy, and containers have no general forwarding.
install -d -m 0755 /etc/systemd/system/pvedaemon.service.d
cat > /etc/systemd/system/pvedaemon.service.d/homestack-e2e-registry.conf <<'PROXY'
[Service]
Environment="HTTPS_PROXY=http://127.0.0.1:3129"
Environment="https_proxy=http://127.0.0.1:3129"
Environment="NO_PROXY=localhost,127.0.0.1,10.77.0.0/24,10.0.2.15"
Environment="no_proxy=localhost,127.0.0.1,10.77.0.0/24,10.0.2.15"
PROXY
systemctl daemon-reload
systemctl enable --now homestack-e2e-dns.service
systemctl restart homestack-e2e-dns.service pvedaemon
systemctl is-active --quiet homestack-e2e-dns.service pvedaemon
python3 - 10.77.0.1 53 <<'DNSCHECK'
import socket
import struct
import sys

# musl resolves A and AAAA together. REFUSED for AAAA makes an IPv4-only
# name unusable even when its A record is correct.
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
test "$(sysctl -n net.ipv4.ip_forward)" = 0
hostname -s
