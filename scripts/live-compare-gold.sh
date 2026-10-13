#!/usr/bin/env bash
# Phase 3 live gate: Python's `gold.check_gold_readiness` (as the installer
# runs it) against the server's /v1/gold/readiness for the configured Gold,
# at the same time. Read-only: a running Gold is inspected through the guest
# agent (the server needs VM.GuestAgent.Unrestricted on Gold, decisions.md),
# a stopped Gold is not started. They must agree (rule `gold-readiness`:
# the server also checks the vendor snippet's tools, phase 4b).
#
#   HOMESTACK_LIVE_TOKEN_FILE      server's PVE token file (USER@REALM!TOKENID=SECRET)
#   HOMESTACK_LIVE_PVE_ENDPOINTS   comma-separated https origins
#   HOMESTACK_LIVE_CONFIG          Python config.toml (default: the CLI's default path)
#
# Python reaches Proxmox the way it does every day (its configured
# transport); the server gets the server part of the same config
# (hs-config split).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
: "${HOMESTACK_LIVE_TOKEN_FILE:?HOMESTACK_LIVE_TOKEN_FILE is required}"
: "${HOMESTACK_LIVE_PVE_ENDPOINTS:?HOMESTACK_LIVE_PVE_ENDPOINTS is required}"
config=${HOMESTACK_LIVE_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/homestack/config.toml}
[[ -f $config ]] || { echo "live-compare-gold: no Python config at $config" >&2; exit 1; }

cd "$root"
cargo build -q --locked -p hs-server -p hs-config -p hs-parity
bin=$root/target/debug

work=$(mktemp -d)
server_pid=
cleanup() {
    [[ -n $server_pid ]] && kill "$server_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
umask 077
mkdir -p "$work/state"
"$bin/hs-config" split --config "$config" --server-out "$work/state/server.toml" --client-out "$work/client.toml" >/dev/null
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$work/api-token"
port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

HOMESTACK_LISTEN=127.0.0.1:$port \
HOMESTACK_API_TOKEN_FILE=$work/api-token \
HOMESTACK_PVE_TOKEN_FILE=$HOMESTACK_LIVE_TOKEN_FILE \
HOMESTACK_PVE_ENDPOINTS=$HOMESTACK_LIVE_PVE_ENDPOINTS \
HOMESTACK_CONFIG_FILE=$work/state/server.toml \
HOMESTACK_LOG=warn \
    "$bin/homestack-server" 2> "$work/server.log" &
server_pid=$!
for _ in $(seq 50); do
    curl -fsS "http://127.0.0.1:$port/health/live" >/dev/null 2>&1 && break
    sleep 0.2
done

rust() {
    printf 'header = "Authorization: Bearer %s"\n' "$(cat "$work/api-token")" |
        curl -fsS -K - "http://127.0.0.1:$port$1" > "$2"
}
python_gold() {
    (cd "$root/python" && unset VIRTUAL_ENV &&
        PYTHONPATH=$root/parity/python uv run --quiet python -m homestack_parity.live_driver \
            --config "$config" -- gold-readiness)
}
compare() {
    if "$bin/hs-parity" compare "$root/parity/normalize.toml" "$1" "$2" "$3"; then
        echo "live-compare-gold: $4 agrees"
    else
        echo "live-compare-gold: $4 differs (rule $1)" >&2
        failed=1
    fi
}

failed=0
python_gold > "$work/python-gold.json" & python_pid=$!
rust /v1/gold/readiness "$work/rust-gold.json"
wait "$python_pid"
compare gold-readiness "$work/python-gold.json" "$work/rust-gold.json" "Gold readiness"
if ((failed)); then
    exit 1
fi
python3 -I -c '
import json, sys
data = json.load(open(sys.argv[1]))
run = sum(1 for check in data["checks"] if check["status"] != "skip")
print("live-compare-gold: OK (Gold %s %s, ok=%s, %d of %d checks run)"
      % (data["vmid"], data["power_state"], data["ok"], run, len(data["checks"])))' "$work/rust-gold.json"
