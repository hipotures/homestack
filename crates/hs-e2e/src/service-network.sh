#!/bin/sh
set -eu
test -f /etc/homestack-e2e.json
test -d /sys/class/net/vmbr0/bridge
test "$(sysctl -n net.ipv4.ip_forward)" = 1
export DEBIAN_FRONTEND=noninteractive
apt-get -o APT::Status-Fd=3 -o Dpkg::Use-Pty=0 -y --no-install-recommends install skopeo
if ! grep -q 'api1.e2e.homestack.app' /etc/hosts; then
    printf '\n10.77.0.101 api1.e2e.homestack.app\n' >> /etc/hosts
fi
if ! grep -q 'pve1.e2e.homestack.app' /etc/hosts; then
    printf '\n10.77.0.1 pve1.e2e.homestack.app\n' >> /etc/hosts
fi
# Only the Proxmox OCI worker uses this proxy. APT retains its separate,
# read-only repository proxy; guests use the separately filtered Internet uplink.
install -d -m 0755 /etc/systemd/system/pvedaemon.service.d
cat > /etc/systemd/system/pvedaemon.service.d/homestack-e2e-registry.conf <<'PROXY'
[Service]
Environment="HTTPS_PROXY=http://127.0.0.1:3129"
Environment="https_proxy=http://127.0.0.1:3129"
Environment="NO_PROXY=localhost,127.0.0.1,10.77.0.0/24,10.0.2.15"
Environment="no_proxy=localhost,127.0.0.1,10.77.0.0/24,10.0.2.15"
PROXY
systemctl daemon-reload
systemctl restart pvedaemon
systemctl is-active --quiet homestack-e2e-dns.service
systemctl is-active --quiet homestack-e2e-internet.service
systemctl is-active --quiet pvedaemon
test "$(sysctl -n net.ipv4.ip_forward)" = 1
hostname -s
