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
# Builds the production node target of Containerfile as homestack-server:check unless
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
    docker_ build --target node -q -f "$root/Containerfile" -t "$image" "$root" >/dev/null && built=1
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
# Files stay private on the host; a root initializer installs only these two
# run-owned tokens for the image's unprivileged server account.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=HomeStack-live-test-CA \
    -keyout "$work/ca-key.pem" -out "$work/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=localhost \
    -keyout "$work/key.pem" -out "$work/request.pem" >/dev/null 2>&1
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > "$work/extensions"
openssl x509 -req -in "$work/request.pem" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" \
    -CAcreateserial -days 1 -extfile "$work/extensions" -out "$work/cert.pem" >/dev/null 2>&1
{
    printf 'HOMESTACK_TLS_CERT_B64=%s\n' "$(base64 -w0 "$work/cert.pem")"
    printf 'HOMESTACK_TLS_KEY_B64=%s\n' "$(base64 -w0 "$work/key.pem")"
} > "$work/tls.env"
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
# shellcheck disable=SC2016 # The initializer expands token inside the container.
docker_ run --rm --entrypoint sh -v "$work:/run/fixture:ro" \
    -v "$volume:/var/lib/homestack" "$image" -c '
        for token in api-token pve-token; do
            install -o homestack -g nogroup -m 0600 "/run/fixture/$token" "/var/lib/homestack/$token"
        done'
management_port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
front() { curl --noproxy '*' --cacert "$work/ca.pem" "$@"; }
start() {
    docker_ run -d --name "$name" -p "127.0.0.1:$port:443" \
        -p "127.0.0.1:$management_port:80" "${hosts[@]}" --env-file "$work/tls.env" \
        -v "$volume:/var/lib/homestack" \
        -e HOMESTACK_API_TOKEN_FILE=/var/lib/homestack/api-token \
        -e HOMESTACK_PVE_TOKEN_FILE=/var/lib/homestack/pve-token \
        -e HOMESTACK_PVE_ENDPOINTS="$endpoints" \
        "$image" >/dev/null
    for _ in $(seq 50); do
        front -fsS "https://127.0.0.1:$port/health/live" >/dev/null 2>&1 && return 0
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
        front -sS -K - -X "$method" -o "$output" -w '%{http_code}' "$@" "https://127.0.0.1:$port$path"
}

start
# Both processes must be under runit, and management HTTP must not proxy API.
docker_ exec "$name" sv status /etc/service/homestack /etc/service/caddy
curl --noproxy '*' -fsS "http://127.0.0.1:$management_port/health/live" >/dev/null
for path in /v1/pve/version /v1/secrets/pve-token /v1/config; do
    [[ $(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$management_port$path") == 404 ]] ||
        fail "management HTTP exposed $path"
done
front -fsS "https://127.0.0.1:$port/health/live"
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
if [[ $status != 200 ]] || ! cmp -s "$work/server.toml" "$work/server-after.toml"; then
    fail "GET /v1/config after the restart differs from the upload"
fi
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
