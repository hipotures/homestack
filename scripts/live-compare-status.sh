#!/usr/bin/env bash
# Phase 2 live gate: Python `status --json` against the server's /v1/status,
# and `status VMID --json` against /v1/workspaces/VMID for every HomeStack VM
# Python lists, on the real cluster at the same time. Read-only. The only
# allowed differences are the rules status-global-live and status-workspace
# in parity/normalize.toml.
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
[[ -f $config ]] || { echo "live-compare-status: no Python config at $config" >&2; exit 1; }

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
python_status() {
    (cd "$root/python" && unset VIRTUAL_ENV && uv run --quiet homestack --config "$config" status "$@" --json)
}
compare() {
    if "$bin/hs-parity" compare "$root/parity/normalize.toml" "$1" "$2" "$3"; then
        echo "live-compare-status: $4 agrees"
    else
        echo "live-compare-status: $4 differs (rule $1)" >&2
        failed=1
    fi
}

failed=0
# Each pair runs concurrently so both read the cluster at the same time;
# Python runs one command at a time (one Herdr pane).
python_status > "$work/python-status.json" & python_pid=$!
rust /v1/status "$work/rust-status.json"
wait "$python_pid"
compare status-global-live "$work/python-status.json" "$work/rust-status.json" "global status"

mapfile -t vmids < <(python3 -I -c '
import json, sys
data = json.load(open(sys.argv[1]))
vms = ([data["gold"]] if data.get("gold") else []) + data["workspaces"]
print("\n".join(str(vm["vmid"]) for vm in vms))' "$work/python-status.json")
for vmid in "${vmids[@]}"; do
    python_status "$vmid" > "$work/python-$vmid.json" & python_pid=$!
    rust "/v1/workspaces/$vmid" "$work/rust-$vmid.json"
    wait "$python_pid"
    compare status-workspace "$work/python-$vmid.json" "$work/rust-$vmid.json" "VM $vmid"
done

if ((failed)); then
    exit 1
fi
echo "live-compare-status: OK (global status and ${#vmids[@]} VMs)"
