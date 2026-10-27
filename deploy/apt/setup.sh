#!/usr/bin/env bash
# Add the signed HomeStack APT source once on an amd64 PVE node.
set -euo pipefail
if (($#)); then
    echo "Usage: $0" >&2
    exit 2
fi
if ((EUID != 0)) || [[ ! -d /etc/pve ]]; then
    echo "Run this script as root on a PVE node." >&2
    exit 1
fi
if [[ $(dpkg --print-architecture) != amd64 ]]; then
    echo "HomeStack PVE packages currently support amd64 only." >&2
    exit 1
fi
for command in curl gpg apt-get; do
    command -v "$command" >/dev/null || { echo "Missing prerequisite: $command" >&2; exit 1; }
done
base=https://hipotures.github.io/homestack/apt
fingerprint=BDC607BB2F6BA396684B20A70A4E5AA38D54C9EE
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
curl --proto '=https' --proto-redir '=https' --fail --silent --show-error --location "$base/homestack-archive-keyring.gpg" -o "$temporary/key.gpg"
actual=$(gpg --homedir "$temporary" --batch --with-colons --show-keys "$temporary/key.gpg" 2>/dev/null | awk -F: '$1 == "fpr" {print $10; exit}')
if [[ $actual != "$fingerprint" ]]; then
    echo "HomeStack archive key fingerprint mismatch." >&2
    exit 1
fi
install -d -m 0755 /etc/apt/keyrings
install -m 0644 "$temporary/key.gpg" /etc/apt/keyrings/homestack.gpg
cat > "$temporary/homestack.sources" <<EOF
Types: deb
URIs: $base
Suites: stable
Components: main
Architectures: amd64
Signed-By: /etc/apt/keyrings/homestack.gpg
EOF
install -m 0644 "$temporary/homestack.sources" /etc/apt/sources.list.d/homestack.sources
apt-get update
echo "HomeStack APT source configured. Install with: apt-get install homestack-pve-agent"
