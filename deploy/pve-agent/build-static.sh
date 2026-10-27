#!/usr/bin/env bash
# Build the standalone x86-64 PVE application without changing other targets.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
python3 scripts/cargo.py build --locked --release -p hs-pve-agent \
    --bin homestack-pve-agent --target x86_64-unknown-linux-musl \
    --config 'target.x86_64-unknown-linux-musl.linker="rust-lld"' \
    --config 'target.x86_64-unknown-linux-musl.rustflags=["-C","target-feature=+crt-static"]' \
    --config 'profile.release.strip=true'
