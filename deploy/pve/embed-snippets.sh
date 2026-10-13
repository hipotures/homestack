#!/usr/bin/env bash
# Write the vendor snippets of deploy/cloud-init/ into homestack-pve.sh,
# between its "EMBEDDED SNIPPETS" markers, so the owner copies one file.
# Run after the snippets change; crates/hs-core/tests/cloudinit.rs fails
# while the embedded copies differ.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script=$here/homestack-pve.sh
snippets=$here/../cloud-init
names=(homestack-vendor-create.yaml homestack-vendor-keep.yaml)

for name in "${names[@]}"; do
    if grep -qx 'HOMESTACK_SNIPPET' "$snippets/$name"; then
        echo "embed-snippets: $name contains the heredoc terminator" >&2
        exit 1
    fi
    [[ $(tail -c1 "$snippets/$name") == '' ]] || { echo "embed-snippets: $name lacks a final newline" >&2; exit 1; }
done

block=$(mktemp)
trap 'rm -f "$block"' EXIT
# shellcheck disable=SC2016 # the block is code for homestack-pve.sh
{
    echo '# BEGIN EMBEDDED SNIPPETS (deploy/pve/embed-snippets.sh writes this part)'
    echo 'snippet_text() {'
    echo '    case $1 in'
    for name in "${names[@]}"; do
        printf '        %s)\n' "$name"
        printf "            cat <<'HOMESTACK_SNIPPET'\n"
        cat "$snippets/$name"
        echo 'HOMESTACK_SNIPPET'
        echo '            ;;'
    done
    echo '        *) die "no embedded snippet $1" ;;'
    echo '    esac'
    echo '}'
    echo '# END EMBEDDED SNIPPETS'
} > "$block"

awk -v block="$block" '
    /^# BEGIN EMBEDDED SNIPPETS/ { while ((getline line < block) > 0) print line; skip = 1; next }
    /^# END EMBEDDED SNIPPETS/ { skip = 0; next }
    !skip' "$script" > "$script.new"
chmod --reference="$script" "$script.new"
mv "$script.new" "$script"
