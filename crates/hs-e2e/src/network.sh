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
    address 10.77.0.@INDEX@/24
    bridge-ports @BRIDGE_PORT@
    bridge-stp off
    bridge-fd 0
@PORT_UP@
    post-up sysctl -q -w net.ipv6.conf.vmbr0.disable_ipv6=1
# END HOMESTACK E2E GUEST NETWORK
NETWORK
fi
ifup vmbr0
if test '@BRIDGE_PORT@' != none; then
    for attempt in $(seq 1 100); do
        test "$(cat /sys/class/net/@BRIDGE_PORT@/operstate)" = up && break
        sleep 0.1
    done
    if test "$(cat /sys/class/net/@BRIDGE_PORT@/operstate)" != up; then
        echo 'The private backbone interface did not become ready' >&2
        exit 1
    fi
fi
