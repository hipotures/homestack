#!/usr/bin/env bash
# Every check of the Rust migration (docs/migration/PLAN.md, section 2).
#
#   scripts/check.sh            L1-L3, no cluster
#   scripts/check.sh --live     also L4 against the real cluster (read-only in phase 0)
#   scripts/check.sh --phase N  only the tests and scenarios of phase N
#
# --live needs HOMESTACK_LIVE_TOKEN_FILE and HOMESTACK_LIVE_PVE_ENDPOINTS
# (see rust/crates/hs-server/tests/live.rs) and docker for the image check.
set -euo pipefail

usage() {
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

live=0
phase=
while (($#)); do
    case $1 in
        --live) live=1 ;;
        --phase) phase=${2:?--phase needs a phase, e.g. 2 or 14a}; shift ;;
        -h | --help) usage ;;
        *) echo "check.sh: unknown argument $1" >&2; usage ;;
    esac
    shift
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
unset VIRTUAL_ENV
export PYTHONPATH="$root/rust/parity/python"
cargo_() { local command=$1; shift; cargo "$command" --manifest-path "$root/rust/Cargo.toml" "$@"; }
step() { printf '\n== %s\n' "$*"; }

if [[ -n $phase ]]; then
    export HOMESTACK_PHASE=$phase
    mapfile -t ported < <(cargo_ run -q -p hs-parity -- manifest-phase "$root/rust/parity/manifest.toml" "$phase")
    if ((${#ported[@]})); then
        step "Rust tests porting phase $phase (${#ported[@]})"
        cargo_ test --workspace --locked -- --exact "${ported[@]}"
    else
        step "no Rust tests port phase $phase yet"
    fi
    step "parity scenarios of phase $phase"
    cargo_ test -p hs-parity --locked --test scenarios
    exit 0
fi

step "cargo fmt"
cargo_ fmt --all --check

step "clippy"
cargo_ clippy --workspace --all-targets --locked -- -D warnings

step "Rust tests: unit, simulator (L2) and parity scenarios (L3)"
cargo_ test --workspace --locked

step "Python parity tools"
uv run --quiet python -m unittest discover -s rust/parity/python/tests -t rust/parity/python

step "Python test suite (the reference behaviour)"
uv run --quiet python -m unittest discover -s tests

step "test manifest (L1)"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
uv run --quiet python -m homestack_parity.list_tests tests > "$work/python-ids"
cargo_ test --workspace --locked -q -- --list --format terse > "$work/rust-tests" 2>/dev/null
cargo_ run -q -p hs-parity -- manifest-check "$root/rust/parity/manifest.toml" "$work/python-ids" "$work/rust-tests"

if ((live)); then
    : "${HOMESTACK_LIVE_TOKEN_FILE:?--live needs HOMESTACK_LIVE_TOKEN_FILE}"
    : "${HOMESTACK_LIVE_PVE_ENDPOINTS:?--live needs HOMESTACK_LIVE_PVE_ENDPOINTS}"
    step "live cluster (L4): server in-process"
    cargo_ test -p hs-server --locked --test live -- --ignored --test-threads=1
    step "live cluster (L4): server image"
    "$root/scripts/live-container.sh"
fi

step "all checks passed$( ((live)) && echo ' (including live)')"
