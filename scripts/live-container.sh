#!/usr/bin/env bash
# Live gates of the server image against the real cluster:
#
# - phase 0: GET /v1/pve/version with certificate verification and failover
#   past a dead first endpoint,
# - phase 1: an uploaded configuration survives replacing the container (same
#   volume), and GET /v1/capabilities answers from the token's privileges.
#
# Proxmox is only read. Needs:
#
#   HOMESTACK_LIVE_TOKEN_FILE      PVE token file (USER@REALM!TOKENID=SECRET)
#   HOMESTACK_LIVE_PVE_ENDPOINTS   comma-separated https origins (certificate names)
#
# Builds Containerfile as homestack-server:check unless
# HOMESTACK_IMAGE names an existing image. Always removes its container.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
: "${HOMESTACK_LIVE_TOKEN_FILE:?HOMESTACK_LIVE_TOKEN_FILE is required}"
: "${HOMESTACK_LIVE_PVE_ENDPOINTS:?HOMESTACK_LIVE_PVE_ENDPOINTS is required}"

# Run docker; if this shell does not have the docker group yet but the user
# is a member, go through newgrp instead of asking for a new login.
docker_() {
    if docker info >/dev/null 2>&1; then
        docker "$@"
    elif id -nG "$(id -un)" | tr ' ' '\n' | grep -qx docker || getent group docker | grep -qw "$(id -un)"; then
        printf '%q ' docker "$@" | { cat; echo; } | newgrp docker
    else
        echo "live-container: docker is not usable by $(id -un)" >&2
        return 2
    fi
}

image=${HOMESTACK_IMAGE:-}
if [[ -z $image ]]; then
    image=homestack-server:check
    built=0
    docker_ build -q -f "$root/Containerfile" -t "$image" "$root" >/dev/null && built=1
    # Without BuildKit the compile stage (about 2 GB) stays behind as untagged
    # images, also after a failed build.
    docker_ image prune -f --filter label=org.homestack.stage=build >/dev/null
    ((built)) || { echo "live-container: image build failed" >&2; exit 1; }
fi

work=$(mktemp -d)
name=homestack-check-$$
volume=homestack-check-$$
cleanup() {
    docker_ rm -f "$name" >/dev/null 2>&1 || true
    docker_ volume rm -f "$volume" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

umask 077
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$work/api-token"
cp "$HOMESTACK_LIVE_TOKEN_FILE" "$work/pve-token"
chmod 0755 "$work"
chmod 0644 "$work/api-token" "$work/pve-token"
port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

# Node names may come from this host's /etc/hosts rather than DNS; give the
# container the same answers. The certificate is still checked by name.
hosts=()
IFS=, read -ra urls <<<"$HOMESTACK_LIVE_PVE_ENDPOINTS"
for url in "${urls[@]}"; do
    host=$(python3 -I -c 'import sys, urllib.parse; print(urllib.parse.urlsplit(sys.argv[1].strip()).hostname)' "$url")
    ip=$(getent ahostsv4 "$host" | awk 'NR == 1 { print $1 }')
    [[ -n $ip ]] || { echo "live-container: cannot resolve $host" >&2; exit 1; }
    hosts+=(--add-host "$host:$ip")
done

# A dead endpoint first: nothing listens on port 1 inside the container.
endpoints="https://localhost:1,$HOMESTACK_LIVE_PVE_ENDPOINTS"
docker_ volume create "$volume" >/dev/null
start() {
    docker_ run -d --name "$name" --user 65534:65534 -p "127.0.0.1:$port:8080" "${hosts[@]}" \
        -v "$work/api-token:/run/secrets/api-token:ro" \
        -v "$work/pve-token:/run/secrets/pve-token:ro" \
        -v "$volume:/var/lib/homestack" \
        -e HOMESTACK_API_TOKEN_FILE=/run/secrets/api-token \
        -e HOMESTACK_PVE_TOKEN_FILE=/run/secrets/pve-token \
        -e HOMESTACK_PVE_ENDPOINTS="$endpoints" \
        "$image" >/dev/null
    for _ in $(seq 50); do
        curl -fsS "http://127.0.0.1:$port/health/live" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "live-container: the server did not come up" >&2
    docker_ logs "$name" >&2 || true
    exit 1
}
fail() {
    echo "live-container: $*" >&2
    docker_ logs "$name" >&2 || true
    exit 1
}
# api METHOD PATH OUTPUT [curl arguments...]: prints the HTTP status.
api() {
    local method=$1 path=$2 output=$3
    shift 3
    printf 'header = "Authorization: Bearer %s"\n' "$(cat "$work/api-token")" |
        curl -sS -K - -X "$method" -o "$output" -w '%{http_code}' "$@" "http://127.0.0.1:$port$path"
}

start
curl -fsS "http://127.0.0.1:$port/health/live"
echo

# Phase 0
status=$(api GET /v1/pve/version "$work/version.json")
cat "$work/version.json"
echo
[[ $status == 200 ]] || fail "GET /v1/pve/version returned $status"
docker_ logs "$name" 2>&1 | grep -q '"endpoint":"https://localhost:1"' ||
    fail "the dead first endpoint was not tried"
echo "live-container: version OK (dead first endpoint skipped)"

# Phase 1: a configuration for the first endpoint's node, e.g. pve1 with
# homestack-storage-1. Only ACL paths depend on it; Gold is the test Gold
# 248 (hst-gold), so no VM of the owner is named.
host=$(python3 -I -c 'import sys, urllib.parse; print(urllib.parse.urlsplit(sys.argv[1].strip()).hostname)' "${urls[0]}")
node=${host%%.*}
number=$(sed -E 's/^[^0-9]*0*//' <<<"$node")
[[ $number =~ ^[1-9][0-9]*$ ]] || fail "cannot derive a layout number from node $node"
cat > "$work/server.toml" <<TOML
version = 1

[node]
name = "$node"
gold_vmid = 248

[storage_layouts]
homestack-storage-$number = ["local"]

[root]
disk = "scsi0"

[home]
disk = "scsi1"
default_size = "20G"

[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]

[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"

[user]
name = "user"
uid = 1000
gid = 1000
TOML
status=$(api PUT /v1/config "$work/put.json" -H 'If-None-Match: *' -H 'Content-Type: application/toml' \
    --data-binary "@$work/server.toml")
[[ $status == 200 ]] || fail "PUT /v1/config returned $status: $(cat "$work/put.json")"
uploaded=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["config_version"])' "$work/put.json")

# Replace the container; the volume stays.
docker_ rm -f "$name" >/dev/null
start
status=$(api GET /v1/config/version "$work/version-after.json")
[[ $status == 200 ]] || fail "GET /v1/config/version returned $status"
after=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["config_version"])' "$work/version-after.json")
[[ $after == "$uploaded" ]] || fail "configuration version $after after the restart, uploaded $uploaded"
status=$(api GET /v1/config "$work/server-after.toml")
[[ $status == 200 ]] && cmp -s "$work/server.toml" "$work/server-after.toml" ||
    fail "GET /v1/config after the restart differs from the upload"
echo "live-container: configuration $uploaded survived replacing the container"

status=$(api GET /v1/capabilities "$work/capabilities.json")
[[ $status == 200 ]] || fail "GET /v1/capabilities returned $status: $(cat "$work/capabilities.json")"
python3 -I - "$work/capabilities.json" <<'PY' || fail "unexpected capabilities"
import json, sys
body = json.load(open(sys.argv[1]))
ops = {op["operation"]: op for op in body["operations"]}
for op in ops.values():
    assert op["available"] == (not op["missing"]), op
    print(f'  {op["operation"]}: {"available" if op["available"] else str(len(op["missing"])) + " missing"}')
assert ops["read"]["available"], "the token cannot even read"
PY
echo "live-container: OK (image $image)"
