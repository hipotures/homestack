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
test "$(sysctl -n net.ipv4.ip_forward)" = 0
hostname -s
