#!/usr/bin/env bash
# Phase 6 probe (PLAN.md, phase 6; api-mapping.md B3 step 2): the two ways to
# put a copy of Gold's root next to a workspace through the API, since the
# API has no `qm disk import`.
#
#   A  the VM runs; `import-from` into a free slot (scsi29), then
#      `PUT /unlink` without `force`: does the copy become `unusedN`?
#   B  the VM is stopped; `import-from` straight into the root slot: does the
#      old root become `unusedN`?
#
# It also checks what the refresh journal in `description` (decision 4) relies
# on: a write with the current `digest` succeeds, one with a stale `digest`
# is refused, and how large a description Proxmox keeps.
#
# Everything happens on one VM of the agent's own, cloned from the test Gold
# into the test pool and destroyed at the end, also after a failure. The
# script refuses any VMID outside 240-249, a token other than the test token
# and a Gold outside pool homestack-gold. Needs:
#
#   HOMESTACK_LIVE_TEST_TOKEN_FILE  the test token (homestack@pve!test=SECRET)
#   HOMESTACK_LIVE_PVE_ENDPOINTS    comma-separated https origins; the first is used
#
# Optional: HOMESTACK_PROBE_VMID (240), HOMESTACK_LIVE_TEST_GOLD (248),
# HOMESTACK_PROBE_NODE (pve2), HOMESTACK_PROBE_STORAGE (local-lvm).
set -euo pipefail

: "${HOMESTACK_LIVE_TEST_TOKEN_FILE:?HOMESTACK_LIVE_TEST_TOKEN_FILE is required}"
: "${HOMESTACK_LIVE_PVE_ENDPOINTS:?HOMESTACK_LIVE_PVE_ENDPOINTS is required}"
vmid=${HOMESTACK_PROBE_VMID:-240}
gold=${HOMESTACK_LIVE_TEST_GOLD:-248}
node=${HOMESTACK_PROBE_NODE:-pve2}
storage=${HOMESTACK_PROBE_STORAGE:-local-lvm}
pool=homestack-test
endpoint=${HOMESTACK_LIVE_PVE_ENDPOINTS%%,*}

die() { echo "probe-import-from: $*" >&2; exit 1; }
in_test_range() { [[ $1 =~ ^24[0-9]$ ]]; }
in_test_range "$vmid" || die "VMID $vmid is outside the test range 240-249; refusing"
in_test_range "$gold" || die "Gold $gold is outside the test range 240-249; refusing"
[[ $vmid != "$gold" ]] || die "the probe VM cannot be the Gold"
grep -q '^homestack@pve!test=' "$HOMESTACK_LIVE_TEST_TOKEN_FILE" || die "not the test token (homestack@pve!test); refusing"

work=$(mktemp -d)
umask 077
printf 'Authorization: PVEAPIToken=%s\n' "$(tr -d '\n' <"$HOMESTACK_LIVE_TEST_TOKEN_FILE")" >"$work/auth"

# api METHOD PATH [curl args...]: prints the body's `data`; fails on non-2xx. DELETE takes
# its parameters in the query (-G): Proxmox refuses a DELETE with a body.
api() {
    local method=$1 path=$2 code
    shift 2
    code=$(curl -sS -o "$work/body" -w '%{http_code}' -X "$method" -H @"$work/auth" "$@" "$endpoint/api2/json$path")
    if [[ $code != 2* ]]; then
        echo "HTTP $code $(jq -c '{message, errors}' "$work/body" 2>/dev/null || cat "$work/body")" >"$work/error"
        return 1
    fi
    jq -c '.data' "$work/body"
}
# wait_task UPID: waits for a task and fails with its exit status.
wait_task() {
    local upid=$1 enc status
    enc=$(jq -rn --arg u "$upid" '$u | @uri')
    for _ in $(seq 1 900); do
        status=$(api GET "/nodes/$node/tasks/$enc/status")
        if [[ $(jq -r '.status' <<<"$status") == stopped ]]; then
            [[ $(jq -r '.exitstatus' <<<"$status") == OK ]] && return 0
            jq -r '.exitstatus' <<<"$status" >"$work/error"
            return 1
        fi
        sleep 1
    done
    echo "task $upid did not finish" >"$work/error"
    return 1
}
config() { api GET "/nodes/$node/qemu/$vmid/config"; }
unused_of() { jq -r --arg v "$2" 'to_entries[] | select((.key | test("^unused[0-9]+$")) and (.value | split(",")[0]) == $v) | .key' <<<"$1"; }
volume_of() { jq -r --arg k "$2" '(.[$k] // "") | split(",")[0] // ""' <<<"$1"; }
report() { printf '%-4s %s\n' "$1" "$2"; }
fail() { report "$1" "FAILED: $2: $(cat "$work/error" 2>/dev/null)"; }
now() { date +%s; }

created=0
cleanup() {
    if ((created)); then
        if upid=$(api POST "/nodes/$node/qemu/$vmid/status/stop" | jq -r .); then
            wait_task "$upid" || true
        fi
        if upid=$(api DELETE "/nodes/$node/qemu/$vmid" -G --data-urlencode purge=1 --data-urlencode destroy-unreferenced-disks=1 | jq -r .); then
            wait_task "$upid" || echo "probe-import-from: destroying VM $vmid failed: $(cat "$work/error")" >&2
        fi
        if api GET /cluster/resources --data-urlencode type=vm -G | jq -e --argjson id "$vmid" 'any(.[]; .vmid == $id)' >/dev/null; then
            echo "probe-import-from: VM $vmid still exists" >&2
        else
            report "--" "VM $vmid destroyed"
        fi
    fi
    rm -rf "$work"
}
trap cleanup EXIT

# Preconditions: the Gold in its pool, the VMID free.
gold_pool=$(api GET /cluster/resources --data-urlencode type=vm -G | jq -r --argjson id "$gold" '.[] | select(.vmid == $id) | .pool // ""')
[[ $gold_pool == homestack-gold ]] || die "Gold $gold is not in pool homestack-gold (got '$gold_pool'); refusing"
api GET /cluster/resources --data-urlencode type=vm -G | jq -e --argjson id "$vmid" 'any(.[]; .vmid == $id)' >/dev/null && die "VMID $vmid exists; refusing"
gold_cfg=$(api GET "/nodes/$node/qemu/$gold/config")
gold_root=$(volume_of "$gold_cfg" scsi0)
[[ -n $gold_root ]] || die "Gold $gold has no scsi0"
report "--" "PVE $(api GET /version | jq -r .version), Gold $gold root $gold_root, probe VM $vmid on $node/$storage"

upid=$(api POST "/nodes/$node/qemu/$gold/clone" --data-urlencode newid="$vmid" --data-urlencode name=hst-probe-import \
    --data-urlencode full=1 --data-urlencode storage="$storage" --data-urlencode pool="$pool" | jq -r .) || die "clone: $(cat "$work/error")"
created=1
wait_task "$upid" || die "clone: $(cat "$work/error")"
cfg=$(config)
old_root=$(volume_of "$cfg" scsi0)
report "--" "cloned: root $old_root"

# A: running VM, import into scsi29, unlink without force.
variant_a() {
    local upid cfg t0 new key
    if ! { upid=$(api POST "/nodes/$node/qemu/$vmid/status/start" | jq -r .) && wait_task "$upid"; }; then
        fail A start
        return
    fi
    cfg=$(config)
    t0=$(now)
    upid=$(api POST "/nodes/$node/qemu/$vmid/config" --data-urlencode "scsi29=$storage:0,import-from=$gold_root" \
        --data-urlencode "digest=$(jq -r .digest <<<"$cfg")" | jq -r .) || { fail A "import-from into scsi29"; return; }
    wait_task "$upid" || { fail A "import-from task"; return; }
    cfg=$(config)
    new=$(volume_of "$cfg" scsi29)
    pending=$(api GET "/nodes/$node/qemu/$vmid/pending" | jq -c '[.[] | select(.key == "scsi29") | {value, pending}]')
    report A "import-from into scsi29 while running: $((  $(now) - t0 ))s, scsi29=${new:-<none>}, pending=$pending"
    [[ -n $new ]] || return
    api PUT "/nodes/$node/qemu/$vmid/unlink" --data-urlencode idlist=scsi29 >/dev/null \
        || { fail A "unlink scsi29 without force"; return; }
    cfg=$(config)
    key=$(unused_of "$cfg" "$new")
    report A "unlink scsi29 without force: scsi29=$(volume_of "$cfg" scsi29), copy is ${key:-<not unused>}, root still $(volume_of "$cfg" scsi0)"
    [[ -n $key ]] || return
    api PUT "/nodes/$node/qemu/$vmid/unlink" --data-urlencode idlist="$key" --data-urlencode force=1 >/dev/null || { fail A "delete $key"; return; }
    cfg=$(config)
    report A "unlink $key force=1: unused left: $(jq -c '[to_entries[] | select(.key | test("^unused")) | .value]' <<<"$cfg")"
}

# B: stopped VM, import straight into scsi0.
variant_b() {
    local upid cfg t0 new key
    if ! { upid=$(api POST "/nodes/$node/qemu/$vmid/status/stop" | jq -r .) && wait_task "$upid"; }; then
        fail B stop
        return
    fi
    cfg=$(config)
    local spec
    spec=$(jq -r '.scsi0' <<<"$cfg" | cut -d, -f2- | tr ',' '\n' | grep -v '^size=' | paste -sd, -)
    t0=$(now)
    upid=$(api POST "/nodes/$node/qemu/$vmid/config" --data-urlencode "scsi0=$storage:0,import-from=$gold_root${spec:+,$spec}" \
        --data-urlencode "digest=$(jq -r .digest <<<"$cfg")" | jq -r .) || { fail B "import-from into scsi0"; return; }
    wait_task "$upid" || { fail B "import-from task"; return; }
    cfg=$(config)
    new=$(volume_of "$cfg" scsi0)
    key=$(unused_of "$cfg" "$old_root")
    report B "import-from into scsi0 while stopped: $(( $(now) - t0 ))s, scsi0=$(jq -r .scsi0 <<<"$cfg"), old root is ${key:-<not unused>}"
    [[ -n $key ]] || return
    api PUT "/nodes/$node/qemu/$vmid/unlink" --data-urlencode idlist="$key" --data-urlencode force=1 >/dev/null || { fail B "delete $key"; return; }
    cfg=$(config)
    report B "unlink $key force=1: unused left: $(jq -c '[to_entries[] | select(.key | test("^unused")) | .value]' <<<"$cfg"), root $new"
}

# J: the journal in `description` (decision 4).
variant_j() {
    local cfg stale size kept
    cfg=$(config)
    stale=$(jq -r .digest <<<"$cfg")
    api PUT "/nodes/$node/qemu/$vmid/config" --data-urlencode "description=homestack probe 1" --data-urlencode "digest=$stale" >/dev/null \
        || { fail J "write with the current digest"; return; }
    if api PUT "/nodes/$node/qemu/$vmid/config" --data-urlencode "description=homestack probe 2" --data-urlencode "digest=$stale" >/dev/null; then
        report J "write with a stale digest: ACCEPTED"
    else
        report J "write with a stale digest: refused: $(cat "$work/error")"
    fi
    for size in 2048 8192 8193 65536; do
        if api PUT "/nodes/$node/qemu/$vmid/config" --data-urlencode "description=$(head -c "$size" /dev/zero | tr '\0' a)" >/dev/null; then
            kept=$(config | jq -r '.description // "" | length')
            report J "description of $size bytes: accepted, $kept kept"
        else
            report J "description of $size bytes: refused: $(cat "$work/error")"
        fi
    done
}

variant_a
variant_b
variant_j
