#!/usr/bin/env bash
# Every check of the Rust migration (docs/migration/PLAN.md, section 2).
#
#   scripts/check.sh            L1-L3, no cluster
#   scripts/check.sh --live     also L4 against the real cluster
#   scripts/check.sh --phase N  only the tests and scenarios of phase N
#
# --live needs HOMESTACK_LIVE_TOKEN_FILE and HOMESTACK_LIVE_PVE_ENDPOINTS
# (see crates/hs-server/tests/live.rs), docker for the image check, the
# Python CLI's own config and transport for the comparisons
# (scripts/live-compare-*.sh), and HOMESTACK_LIVE_TEST_TOKEN_FILE for the
# jobs and the Cloud-Init boot in the test pool
# (crates/hs-server/tests/live_jobs.rs, live_cloudinit.rs), the only live
# checks that change the cluster; the boot also needs the vendor snippets
# on Gold's node (docs/migration/deploy.md).
set -euo pipefail

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
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
export PYTHONPATH="$root/parity/python"
# Python is the uv project in python/; it is removed in the last phase.
uv_() { uv --directory "$root/python" "$@"; }
step() { printf '\n== %s\n' "$*"; }

if [[ -n $phase ]]; then
    export HOMESTACK_PHASE=$phase
    mapfile -t ported < <(cargo run -q -p hs-parity -- manifest-phase "$root/parity/manifest.toml" "$phase")
    if ((${#ported[@]})); then
        step "Rust tests porting phase $phase (${#ported[@]})"
        cargo test --workspace --locked -- --exact "${ported[@]}"
    else
        step "no Rust tests port phase $phase yet"
    fi
    step "parity scenarios and configuration corpus of phase $phase"
    cargo test -p hs-parity --locked --test scenarios --test config_corpus
    exit 0
fi

step "cargo fmt"
cargo fmt --all --check

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "Rust tests: unit, simulator (L2) and parity scenarios (L3)"
cargo test --workspace --locked

step "cluster install script (deploy/pve)"
uvx --from shellcheck-py==0.10.0.1 shellcheck -x deploy/pve/homestack-pve.sh deploy/pve/embed-snippets.sh \
    deploy/pve/tests/run.sh deploy/pve/tests/fake-pve
deploy/pve/tests/run.sh

step "Python parity tools"
uv_ run --quiet python -m unittest discover -s "$root/parity/python/tests" -t "$root/parity/python"

step "Python test suite (the reference behaviour)"
uv_ run --quiet python -m unittest discover -s tests

step "test manifest (L1)"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
uv_ run --quiet python -m homestack_parity.list_tests tests > "$work/python-ids"
cargo test --workspace --locked -q -- --list --format terse > "$work/rust-tests" 2>/dev/null
cargo run -q -p hs-parity -- manifest-check "$root/parity/manifest.toml" "$work/python-ids" "$work/rust-tests"

if ((live)); then
    : "${HOMESTACK_LIVE_TOKEN_FILE:?--live needs HOMESTACK_LIVE_TOKEN_FILE}"
    : "${HOMESTACK_LIVE_PVE_ENDPOINTS:?--live needs HOMESTACK_LIVE_PVE_ENDPOINTS}"
    : "${HOMESTACK_LIVE_TEST_TOKEN_FILE:?--live needs HOMESTACK_LIVE_TEST_TOKEN_FILE}"
    step "live cluster (L4): server in-process"
    cargo test -p hs-server --locked --test live -- --ignored --test-threads=1
    step "live cluster (L4): server image"
    "$root/scripts/live-container.sh"
    step "live cluster (L4): Python status against the server"
    "$root/scripts/live-compare-status.sh"
    step "live cluster (L4): Python Gold readiness against the server"
    "$root/scripts/live-compare-gold.sh"
    step "live cluster (L4): jobs in the test pool"
    cargo test -p hs-server --locked --test live_jobs -- --ignored --nocapture
    step "live cluster (L4): native Cloud-Init boot in the test pool"
    cargo test -p hs-server --locked --test live_cloudinit -- --ignored --nocapture
    step "live cluster (L4): create and destroy in the test pool, twice"
    cargo test -p hs-server --locked --test live_lifecycle -- --ignored --nocapture
fi

step "all checks passed$( ((live)) && echo ' (including live)')"
