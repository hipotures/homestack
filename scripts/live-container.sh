#!/usr/bin/env bash
# Phase 0 live gate: the server image answers GET /v1/pve/version from the
# real cluster, with certificate verification and failover past a dead first
# endpoint. Read-only. Needs:
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
cleanup() {
    docker_ rm -f "$name" >/dev/null 2>&1 || true
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
docker_ run -d --name "$name" --user 65534:65534 -p "127.0.0.1:$port:8080" "${hosts[@]}" \
    -v "$work/api-token:/run/secrets/api-token:ro" \
    -v "$work/pve-token:/run/secrets/pve-token:ro" \
    -e HOMESTACK_API_TOKEN_FILE=/run/secrets/api-token \
    -e HOMESTACK_PVE_TOKEN_FILE=/run/secrets/pve-token \
    -e HOMESTACK_PVE_ENDPOINTS="$endpoints" \
    "$image" >/dev/null

for _ in $(seq 50); do
    curl -fsS "http://127.0.0.1:$port/health/live" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$port/health/live"
echo

status=$(printf 'header = "Authorization: Bearer %s"\n' "$(cat "$work/api-token")" |
    curl -sS -K - -o "$work/version.json" -w '%{http_code}' "http://127.0.0.1:$port/v1/pve/version")
cat "$work/version.json"
echo
if [[ $status != 200 ]]; then
    echo "live-container: GET /v1/pve/version returned $status" >&2
    docker_ logs "$name" >&2 || true
    exit 1
fi
if ! docker_ logs "$name" 2>&1 | grep -q '"endpoint":"https://localhost:1"'; then
    echo "live-container: the dead first endpoint was not tried" >&2
    exit 1
fi
echo "live-container: OK (image $image, dead first endpoint skipped)"
