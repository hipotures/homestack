#!/bin/sh
# Run inside the Prometheus container as the configuration owner.
set -eu

config=/etc/prometheus/prometheus.yml
backup="$config.before-ai-telemetry"
candidate="$config.ai-telemetry-new"

if grep -q '^[[:space:]]*promote_resource_attributes:' "$config"; then
    echo 'Resource promotion is already configured; review the existing list instead of replacing it.' >&2
    exit 1
fi
if [ -e "$backup" ] || [ -e "$candidate" ]; then
    echo 'A previous telemetry backup or candidate exists; review it before trying again.' >&2
    exit 1
fi

cp -p "$config" "$backup"
trap 'rm -f "$candidate"' EXIT HUP INT TERM
awk '
function attributes() {
    print "  promote_resource_attributes:"
    print "    - host.name"
    print "    - project"
}
/^otlp:[[:space:]]*($|#)/ { print; attributes(); found=1; next }
{ print }
END { if (!found) { print "otlp:"; attributes() } }
' "$config" > "$candidate"

/bin/promtool check config "$candidate"
# Write the existing file so a bind-mounted configuration also works.
if ! cat "$candidate" > "$config"; then
    cat "$backup" > "$config"
    echo 'Could not write the configuration. Restored the original configuration.' >&2
    exit 1
fi
if ! kill -HUP 1; then
    cat "$backup" > "$config"
    echo 'Could not signal Prometheus. Restored the original configuration.' >&2
    exit 1
fi
echo "Configuration validated and reload requested. Backup: $backup"
