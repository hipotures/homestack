#!/usr/bin/env bash
# HomeStack verification through the existing serialized Cargo wrapper.
#   scripts/check.sh          formatting and application compilation
#   scripts/check.sh --full   retained automated checks and static analysis
#   scripts/check.sh --live   existing live checks in an authorized isolated lab
# --live requires HOMESTACK_LIVE_IMAGE, HOMESTACK_LIVE_TOKEN_FILE,
# HOMESTACK_LIVE_PVE_ENDPOINTS and HOMESTACK_LIVE_TEST_TOKEN_FILE.
# See the live test sources and existing E2E documentation for lab setup.
set -euo pipefail

if [[ ${HOMESTACK_BUILD_WRAPPER:-} != 1 ]]; then
    exec python3 "$(dirname "$0")/cargo.py" --check "$@"
fi

live=0
full=0
while (($#)); do
    case $1 in
        --full) full=1 ;;
        --live) live=1 ;;
        -h | --help) sed -n '2,8s/^# *//p' "$0"; exit 0 ;;
        *) echo "check.sh: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
unset VIRTUAL_ENV
step() { printf '\n== %s\n' "$*"; }

if ((live)); then
    : "${HOMESTACK_LIVE_IMAGE:?--live requires HOMESTACK_LIVE_IMAGE naming a published API release}"
fi

step "formatting"
cargo fmt --all --check
step "application binaries"
cargo build --workspace --bins --locked

if ((full)); then
    step "retained Python checks"
    python3 -m unittest discover -s scripts/tests
    step "static analysis"
    cargo clippy --workspace --all-targets --locked -- -D warnings
    step "retained Rust checks (live scenarios remain opt-in)"
    cargo test --workspace --locked
    step "cluster install script"
    uvx --from shellcheck-py==0.10.0.1 shellcheck -x deploy/pve/homestack-pve.sh deploy/pve/embed-snippets.sh \
        deploy/pve/tests/run.sh deploy/pve/tests/fake-pve scripts/live-probe-import-from.sh
    deploy/pve/tests/run.sh
fi

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

step "requested checks passed"
