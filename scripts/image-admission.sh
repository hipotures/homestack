#!/usr/bin/env bash
# Immutable runtime-image admission. Existing dev artifacts are reusable only
# when their platform, single-manifest topology and identity labels match.
set -euo pipefail
image=$1 revision=$2 version=$3 event=$4
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if docker manifest inspect "$image" >"$work/manifest.json" 2>"$work/error"; then
    if [[ $event != workflow_dispatch || $version != "dev-$revision" ]]; then
        echo "Refusing existing release image: $image" >&2
        exit 1
    fi
    python3 - "$work/manifest.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
if manifest.get("schemaVersion") != 2 or "manifests" in manifest or not manifest.get("config") or "layers" not in manifest:
    sys.exit("Existing dev image is not a single runtime manifest")
PY
    docker pull --platform linux/amd64 "$image" >/dev/null
    docker image inspect "$image" >"$work/config.json"
    python3 - "$work/config.json" "$revision" "$version" "${image%:*}" <<'PY'
import json, sys
image, = json.load(open(sys.argv[1]))
labels = image["Config"].get("Labels") or {}
expected = {
    "org.opencontainers.image.source": "https://github.com/hipotures/homestack",
    "org.opencontainers.image.revision": sys.argv[2],
    "org.opencontainers.image.version": sys.argv[3],
    "org.opencontainers.image.title": sys.argv[4].rsplit("/", 1)[-1],
}
if image.get("Os") != "linux" or image.get("Architecture") != "amd64" or any(labels.get(k) != v for k, v in expected.items()):
    sys.exit("Existing dev image does not match the expected component revision/platform")
PY
    echo "Already published and verified: $image"
    echo 'publish=false' >> "$GITHUB_OUTPUT"
else
    # Authentication, network and registry failures must not authorize a push.
    grep -Eqi 'manifest unknown|no such manifest' "$work/error" || {
        cat "$work/error" >&2
        exit 1
    }
    echo 'publish=true' >> "$GITHUB_OUTPUT"
fi
