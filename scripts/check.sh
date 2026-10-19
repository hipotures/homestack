#!/usr/bin/env bash
# HomeStack checks (docs/migration/PLAN.md, section 2).
#
#   scripts/check.sh            L1-L3, no cluster
#   scripts/check.sh --live     also L4 against the real cluster
#   scripts/check.sh --phase N  only the tests and scenarios of phase N
#
# --live needs HOMESTACK_LIVE_TOKEN_FILE and HOMESTACK_LIVE_PVE_ENDPOINTS
# (see crates/hs-server/tests/live.rs), docker for the image check, and
# HOMESTACK_LIVE_TEST_TOKEN_FILE for changes in the test pool (crates/hs-server/tests/live_jobs.rs,
# live_cloudinit.rs, live_lifecycle.rs, live_refresh.rs, live_resize.rs,
# live_migrate.rs, live_addkey.rs, live_create_node.rs), the only live checks
# that change the cluster; the boot also needs the vendor snippets of this
# tree on every node (docs/migration/deploy.md). With
# HOMESTACK_LIVE_OUTSIDE_VMID (a VM in 240-249 tagged homestack-ws and in no
# pool) it also checks that changes outside the pool are refused
# (live_pool_guard.rs).
# The deployment gate (crates/hs-deploy/tests/live_deploy.rs) needs the
# files of its live-deploy.toml (deploy token, client token, certificate)
# and HOMESTACK_LIVE_IMAGE naming a published numeric API release.
# The client gate (crates/hs-cli/tests/live_cli.rs) deploys the same test
# instances and runs homestack create, status, refresh and destroy on VM 240.
# The TUI gate (crates/hs-cli/tests/live_tui.rs) does the same with the
# TUI's actions (plan, Apply, the job's steps), and a Cancel first.
set -euo pipefail

if [[ ${HOMESTACK_BUILD_WRAPPER:-} != 1 ]]; then
    exec python3 "$(dirname "$0")/cargo.py" --check "$@"
fi

usage() {
    sed -n '2,/^set -euo/{/^#/p}' "$0" | sed 's/^# \{0,1\}//'
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

if ((live)); then
    : "${HOMESTACK_LIVE_IMAGE:?--live requires HOMESTACK_LIVE_IMAGE naming a published API release}"
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
unset VIRTUAL_ENV
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

step "build cleanup boundaries"
python3 -m unittest discover -s scripts/tests

step "cargo fmt"
cargo fmt --all --check

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "Rust tests: unit, simulator (L2) and parity scenarios (L3)"
cargo test --workspace --locked

step "cluster install script (deploy/pve)"
uvx --from shellcheck-py==0.10.0.1 shellcheck -x deploy/pve/homestack-pve.sh deploy/pve/embed-snippets.sh \
    deploy/pve/tests/run.sh deploy/pve/tests/fake-pve scripts/live-probe-import-from.sh
deploy/pve/tests/run.sh

step "test manifest (L1)"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cargo test --workspace --locked -q -- --list --format terse > "$work/rust-tests" 2>/dev/null
cargo run -q -p hs-parity -- manifest-check "$root/parity/manifest.toml" "$root/parity/fixtures/python-test-ids.txt" "$work/rust-tests"

if ((live)); then
    : "${HOMESTACK_LIVE_TOKEN_FILE:?--live needs HOMESTACK_LIVE_TOKEN_FILE}"
    : "${HOMESTACK_LIVE_PVE_ENDPOINTS:?--live needs HOMESTACK_LIVE_PVE_ENDPOINTS}"
    : "${HOMESTACK_LIVE_TEST_TOKEN_FILE:?--live needs HOMESTACK_LIVE_TEST_TOKEN_FILE}"
    step "live cluster (L4): server in-process"
    cargo test -p hs-server --locked --test live -- --ignored --test-threads=1
    step "live cluster (L4): server image"
    "$root/scripts/live-container.sh"
    step "live cluster (L4): jobs in the test pool"
    cargo test -p hs-server --locked --test live_jobs -- --ignored --nocapture
    step "live cluster (L4): native Cloud-Init boot in the test pool"
    cargo test -p hs-server --locked --test live_cloudinit -- --ignored --nocapture
    if [[ -n ${HOMESTACK_LIVE_OUTSIDE_VMID:-} ]]; then
        step "live cluster (L4): changes outside the pool are refused"
        cargo test -p hs-server --locked --test live_pool_guard -- --ignored --nocapture
    fi
    step "live cluster (L4): create and destroy in the test pool, twice"
    cargo test -p hs-server --locked --test live_lifecycle -- --ignored --nocapture
    step "live cluster (L4): refresh, a killed server and its recovery in the test pool"
    cargo test -p hs-server --locked --test live_refresh -- --ignored --nocapture
    step "live cluster (L4): resize of a home and a root in the test pool"
    cargo test -p hs-server --locked --test live_resize -- --ignored --nocapture
    step "live cluster (L4): migrate to another node and back in the test pool"
    cargo test -p hs-server --locked --test live_migrate -- --ignored --nocapture
    step "live cluster (L4): addkey on a running, a restarted and a stopped VM in the test pool"
    cargo test -p hs-server --locked --test live_addkey -- --ignored --nocapture
    step "live cluster (L4): create on another node than Gold's in the test pool"
    cargo test -p hs-server --locked --test live_create_node -- --ignored --nocapture
    image=$HOMESTACK_LIVE_IMAGE
    step "live cluster (L4): three instances deployed from $image, one stopped"
    HOMESTACK_LIVE_IMAGE=$image cargo test -p hs-deploy --locked --test live_deploy -- --ignored --nocapture
    step "live cluster (L4): the homestack client on VM 240 through the test instances"
    HOMESTACK_LIVE_IMAGE=$image cargo test -p hs-cli --locked --test live_cli -- --ignored --nocapture
    step "live cluster (L4): the TUI's actions on VM 240 through the test instances"
    HOMESTACK_LIVE_IMAGE=$image cargo test -p hs-cli --locked --test live_tui -- --ignored --nocapture
fi

step "all checks passed$( ((live)) && echo ' (including live)')"
