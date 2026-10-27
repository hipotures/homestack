#!/usr/bin/env bash
# Remove the node agent; retain its operator-managed configuration.
set -euo pipefail
if (($#)); then
    echo "Usage: $0" >&2
    exit 2
fi
if ((EUID != 0)); then
    echo "Run this uninstaller as root on the PVE node." >&2
    exit 1
fi
systemctl disable --now homestack-pve-agent.service
rm -f /etc/systemd/system/homestack-pve-agent.service
rm -f /usr/local/sbin/homestack-pve-agent
systemctl daemon-reload
echo "Removed homestack-pve-agent; retained /etc/homestack/pve-agent.toml."
