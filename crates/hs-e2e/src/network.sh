#!/bin/sh
set -eu
test -f /etc/homestack-e2e.json
command -v ifup >/dev/null
test "$(sysctl -n net.ipv4.ip_forward)" = 0
if ! grep -q '^# BEGIN HOMESTACK E2E GUEST NETWORK$' /etc/network/interfaces; then
    if test -e /sys/class/net/vmbr0 || grep -Eq '^[[:space:]]*iface[[:space:]]+vmbr0[[:space:]]' /etc/network/interfaces; then
        echo 'Refusing to replace an existing vmbr0 not owned by this lab' >&2
        exit 1
    fi
    cat >> /etc/network/interfaces <<'NETWORK'

# BEGIN HOMESTACK E2E GUEST NETWORK
auto vmbr0
iface vmbr0 inet static
    address 10.77.0.1/24
    bridge-ports none
    bridge-stp off
    bridge-fd 0
    post-up sysctl -q -w net.ipv6.conf.vmbr0.disable_ipv6=1
# END HOMESTACK E2E GUEST NETWORK
NETWORK
fi
ifup vmbr0
