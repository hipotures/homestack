#!/usr/bin/env bash
# Install on a PVE node; DESTDIR stages files without touching systemd.
set -euo pipefail
if (($# != 1)); then
    echo "Usage: $0 PATH_TO_STATIC_HOMESTACK_PVE_AGENT" >&2
    exit 2
fi
source_dir=$(cd "$(dirname "$0")" && pwd)
binary=$(realpath "$1")
destination=${DESTDIR:-}
config="$destination/etc/homestack/pve-agent.toml"
unit=homestack-pve-agent.service

if [[ -z $destination ]]; then
    if ((EUID != 0)); then
        echo "Run this installer as root on the PVE node." >&2
        exit 1
    fi
    if [[ ! -d /etc/pve || ! -d /run/systemd/system ]]; then
        echo "This installer requires a PVE node running systemd." >&2
        exit 1
    fi
elif [[ $destination != /* ]]; then
    echo "DESTDIR must be an absolute staging directory." >&2
    exit 1
fi

# Validate the selected binary and configuration before changing the service.
if [[ -f $config ]]; then
    "$binary" --check-config --config "$config"
else
    "$binary" --check-config --config "$source_dir/pve-agent.toml"
fi

install -d -m 0755 "$destination/usr/local/sbin" "$destination/etc/systemd/system"
install -d -m 0750 "$destination/etc/homestack"
if [[ ! -e $config ]]; then
    install -m 0640 "$source_dir/pve-agent.toml" "$config"
fi

# Atomic binary replacement is safe while the old executable is running.
temporary=$(mktemp "$destination/usr/local/sbin/.homestack-pve-agent.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT
install -m 0755 "$binary" "$temporary"
mv -f -- "$temporary" "$destination/usr/local/sbin/homestack-pve-agent"
install -m 0644 "$source_dir/$unit" "$destination/etc/systemd/system/$unit"
if [[ -z $destination ]]; then
    systemctl daemon-reload
    systemctl enable "$unit"
    systemctl restart "$unit"
    systemctl --no-pager --full status "$unit"
else
    echo "Staged homestack-pve-agent under $destination; systemd was not changed."
fi
