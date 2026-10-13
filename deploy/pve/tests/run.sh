#!/usr/bin/env bash
# Tests of deploy/pve/homestack-pve.sh on fake PVE tools (fake-pve), with
# no cluster: install on one node and in a three-node cluster, idempotence,
# adoption of the hand-made state of phases 0-4, uninstall, pools with VMs,
# snippets in use, an unreachable node and the checks that stop an install.
#
#   deploy/pve/tests/run.sh          every test
#   deploy/pve/tests/run.sh NAME...  only these
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script=$here/../homestack-pve.sh
cloud_init=$here/../../cloud-init
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tab=$'\t'

failures=0
current=

fail() {
    printf '  FAIL %s: %s\n' "$current" "$*" >&2
    failures=$((failures + 1))
}

# A fresh fake cluster in $HS_FAKE. NODES: "name ip" pairs; the first is local.
setup() {
    export HS_FAKE=$work/$current
    rm -rf "$HS_FAKE"
    mkdir -p "$HS_FAKE/bin" "$HS_FAKE/vz"
    local tool
    for tool in pvesh pveum pvesm ssh openssl; do
        ln -s "$here/fake-pve" "$HS_FAKE/bin/$tool"
    done
    : > "$HS_FAKE/calls.log"
    : > "$HS_FAKE/acl.tsv"
    : > "$HS_FAKE/tokens.tsv"
    : > "$HS_FAKE/pools.tsv"
    printf 'root@pam\n' > "$HS_FAKE/users"
    printf 'PVEAuditor\tDatastore.Audit,Pool.Audit,Sys.Audit,VM.Audit\t1\nPVESDNUser\tSDN.Audit,SDN.Use\t1\n' \
        > "$HS_FAKE/roles.tsv"
    printf '{}\n' > "$HS_FAKE/options.json"
    cat > "$HS_FAKE/storage.json" <<'EOF'
[{"storage":"local","type":"dir","path":"/var/lib/vz","content":"iso,vztmpl,backup,snippets"},
 {"storage":"local-lvm","type":"lvmthin","content":"images,rootdir"},
 {"storage":"worker-roots","type":"lvmthin","content":"images","nodes":"pve1"}]
EOF
    : > "$HS_FAKE/nodes.tsv"
    local local_flag=1 name
    for name in "$@"; do
        printf '%s\t192.0.2.%s\t1\t%s\n' "$name" "${name//[!0-9]/}" "$local_flag" >> "$HS_FAKE/nodes.tsv"
        printf '%s.example.net\n' "$name" >> "$HS_FAKE/trusted"
        local_flag=0
    done
    if (($# > 1)); then
        touch "$HS_FAKE/cluster"
    fi
    printf '101\t%s\tgold\thomestack-gold\tvirtio=BC:24:11:00:00:01,bridge=vmbr0\tuser=local:snippets/gold-user.yaml\n' \
        "$1" > "$HS_FAKE/vms.tsv"
    printf '100\t%s\tdesktop\t\tvirtio=BC:24:11:00:00:02,bridge=vmbr0\t\n' "$1" >> "$HS_FAKE/vms.tsv"
    # An unrelated snippet of the owner's, which nothing may touch.
    mkdir -p "$HS_FAKE/vz/snippets"
    echo 'owner' > "$HS_FAKE/vz/snippets/gold-user.yaml"
}

# The hand-made state of phases 0-4 (docs/migration/phase-4b.md, section 1).
setup_today() {
    setup pve2 pve1 pve3
    cat >> "$HS_FAKE/roles.tsv" <<'EOF'
HomeStackServerDisks	VM.Config.Disk	0
HomeStackServerGoldGuest	VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted	0
HomeStackTest	Datastore.AllocateSpace,Datastore.Audit,VM.Allocate,VM.Audit,VM.Clone,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt	0
HomeStackTestGold	VM.Audit,VM.Clone	0
HomeStackTestStorage	Datastore.AllocateSpace,Datastore.Audit	0
EOF
    printf 'homestack@pve\n' >> "$HS_FAKE/users"
    printf 'homestack@pve\tserver\t1\nhomestack@pve\ttest\t1\n' > "$HS_FAKE/tokens.tsv"
    printf 'homestack-test\t\tHomeStack migration tests, VMIDs 240-249\n' > "$HS_FAKE/pools.tsv"
    sort -u > "$HS_FAKE/acl.tsv" <<'EOF'
/	user	homestack@pve	PVEAuditor
/	token	homestack@pve!server	PVEAuditor
/	token	homestack@pve!test	PVEAuditor
/	group	automation	PVESysAdmin
/pool/homestack-test	user	homestack@pve	PVEAuditor
/pool/homestack-test	user	homestack@pve	HomeStackTest
/pool/homestack-test	token	homestack@pve!test	HomeStackTest
/sdn/zones/localnetwork/vmbr0	user	homestack@pve	PVEAuditor
/sdn/zones/localnetwork/vmbr0	user	homestack@pve	PVESDNUser
/sdn/zones/localnetwork/vmbr0	token	homestack@pve!test	PVESDNUser
/storage/local	user	homestack@pve	PVEAuditor
/storage/local	user	homestack@pve	HomeStackTestStorage
/storage/local	token	homestack@pve!test	HomeStackTestStorage
/storage/local-lvm	user	homestack@pve	HomeStackTestStorage
/storage/local-lvm	user	homestack@pve	PVEAuditor
/storage/local-lvm	token	homestack@pve!test	HomeStackTestStorage
/vms	user	homestack@pve	HomeStackServerDisks
/vms	user	homestack@pve	PVEAuditor
/vms	token	homestack@pve!server	PVEAuditor
/vms	token	homestack@pve!server	HomeStackServerDisks
/vms/101	user	homestack@pve	HomeStackServerGoldGuest
/vms/101	user	homestack@pve	HomeStackTestGold
/vms/101	user	homestack@pve	PVEAuditor
/vms/101	user	homestack@pve	HomeStackServerDisks
/vms/101	token	homestack@pve!server	HomeStackServerGoldGuest
/vms/101	token	homestack@pve!server	HomeStackServerDisks
/vms/101	token	homestack@pve!server	PVEAuditor
/vms/101	token	homestack@pve!test	HomeStackTestGold
EOF
    cp "$cloud_init/homestack-vendor-create.yaml" "$cloud_init/homestack-vendor-keep.yaml" "$HS_FAKE/vz/snippets/"
}

hs() {
    PATH=$HS_FAKE/bin:$PATH "$script" "$@" --token-dir "$HS_FAKE" > "$HS_FAKE/out" 2>&1
}

hs_ok() {
    if ! hs "$@"; then
        fail "homestack-pve.sh $* failed:"
        sed 's/^/    /' "$HS_FAKE/out" >&2
        return 1
    fi
}

hs_fails() {
    if hs "$@"; then
        fail "homestack-pve.sh $* should have failed:"
        sed 's/^/    /' "$HS_FAKE/out" >&2
        return 1
    fi
}

changes() { grep -c '^pveum ' "$HS_FAKE/calls.log" || true; }

expect_out() {
    grep -qF -- "$1" "$HS_FAKE/out" || { fail "output lacks '$1':"; sed 's/^/    /' "$HS_FAKE/out" >&2; }
}

# The ACL entries of homestack@pve and its tokens, as "path type ugid role".
homestack_acl() {
    awk -F'\t' '$3 == "homestack@pve" || index($3, "homestack@pve!") == 1' "$HS_FAKE/acl.tsv" | sort
}

expect_acl() {
    local expected
    expected=$(sort)
    if [[ $(homestack_acl) != "$expected" ]]; then
        fail "ACL differs:"
        diff <(printf '%s\n' "$expected") <(homestack_acl) | sed 's/^/    /' >&2 || true
    fi
}

expect_roles() {
    local got
    got=$(awk -F'\t' '$3 == 0 { print $1 "\t" $2 }' "$HS_FAKE/roles.tsv" | sort)
    local expected
    expected=$(sort)
    [[ $got == "$expected" ]] || { fail "roles differ:"; diff <(printf '%s\n' "$expected") <(printf '%s\n' "$got") | sed 's/^/    /' >&2 || true; }
}

expect_pools() {
    local got expected
    got=$(cut -f1,2 "$HS_FAKE/pools.tsv" | sort)
    expected=$(sort)
    [[ $got == "$expected" ]] || { fail "pools differ: got '$got', expected '$expected'"; }
}

snippet_dir() {
    if [[ $1 == local ]]; then echo "$HS_FAKE/vz/snippets"; else echo "$HS_FAKE/nodes/$1/vz/snippets"; fi
}

expect_snippets() {
    local where name
    for where in "$@"; do
        for name in homestack-vendor-create.yaml homestack-vendor-keep.yaml; do
            cmp -s "$(snippet_dir "$where")/$name" "$cloud_init/$name" || fail "$name missing or different on $where"
        done
    done
}

expect_no_snippets() {
    local where
    for where in "$@"; do
        if compgen -G "$(snippet_dir "$where")/homestack-vendor-*" > /dev/null; then
            fail "snippets left on $where"
        fi
    done
}

ROLES_TARGET="HomeStack	Pool.Audit,VM.Allocate,VM.Audit,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt
HomeStackGold	Pool.Audit,VM.Audit,VM.Clone,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted
HomeStackStorage	Datastore.AllocateSpace,Datastore.Audit"

# --- tests ---------------------------------------------------------------

test_install_single_node() {
    setup pve1
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'single node'
    expect_acl <<'EOF'
/	token	homestack@pve!server	PVEAuditor
/	user	homestack@pve	PVEAuditor
/pool/homestack	token	homestack@pve!server	HomeStack
/pool/homestack	user	homestack@pve	HomeStack
/pool/homestack-gold	token	homestack@pve!server	HomeStackGold
/pool/homestack-gold	user	homestack@pve	HomeStackGold
/sdn/zones/localnetwork/vmbr0	token	homestack@pve!server	PVESDNUser
/sdn/zones/localnetwork/vmbr0	user	homestack@pve	PVESDNUser
/storage/local	token	homestack@pve!server	HomeStackStorage
/storage/local	user	homestack@pve	HomeStackStorage
/storage/local-lvm	token	homestack@pve!server	HomeStackStorage
/storage/local-lvm	user	homestack@pve	HomeStackStorage
EOF
    expect_roles <<< "$ROLES_TARGET"
    expect_pools <<< $'homestack\t\nhomestack-gold\t101'
    grep -qx 'homestack@pve' "$HS_FAKE/users" || fail "user not created"
    grep -q "^homestack@pve${tab}server${tab}1$" "$HS_FAKE/tokens.tsv" || fail "server token not created with privsep"
    [[ $(cat "$HS_FAKE/homestack-pve-token-server") == 'homestack@pve!server=fake-secret-server' ]] || fail "token file content"
    [[ $(stat -c %a "$HS_FAKE/homestack-pve-token-server") == 600 ]] || fail "token file mode is not 600"
    grep -q 'fake-secret' "$HS_FAKE/out" && fail "the secret was printed"
    expect_snippets local
    [[ $(cat "$HS_FAKE/vz/snippets/gold-user.yaml") == owner ]] || fail "owner's snippet changed"
    [[ ! -e $HS_FAKE/ssh.log ]] || fail "ssh used on a single node"

    : > "$HS_FAKE/calls.log"
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    [[ $(changes) == 0 ]] || fail "second install changed: $(cat "$HS_FAKE/calls.log")"
    expect_out 'Nothing to change'
    hs_ok status --storages local-lvm --snippet-storage local || return
}

test_install_cluster_and_uninstall() {
    setup pve1 pve2 pve3
    hs_ok install --storages local-lvm,worker-roots --snippet-storage local --yes || return
    expect_out 'cluster of 3 nodes'
    expect_snippets local pve2 pve3
    grep -q '/storage/worker-roots' "$HS_FAKE/acl.tsv" || fail "worker-roots has no ACL"

    : > "$HS_FAKE/calls.log"
    hs_ok install --storages local-lvm,worker-roots --snippet-storage local --yes || return
    [[ $(changes) == 0 ]] || fail "second install changed: $(cat "$HS_FAKE/calls.log")"
    expect_out 'Nothing to change'

    hs_ok uninstall --snippet-storage local --yes || return
    expect_acl < /dev/null
    expect_roles < /dev/null
    expect_pools < /dev/null
    grep -q homestack "$HS_FAKE/users" && fail "user left"
    [[ -s $HS_FAKE/tokens.tsv ]] && fail "tokens left"
    expect_no_snippets local pve2 pve3
    [[ $(cat "$HS_FAKE/vz/snippets/gold-user.yaml") == owner ]] || fail "owner's snippet removed"
    grep -q '^101' "$HS_FAKE/vms.tsv" || fail "Gold removed"
    grep -q '/vms\|acl.*group' "$HS_FAKE/calls.log" && fail "touched a non-HomeStack ACL"
    expect_out 'homestack-pve-token-'
}

test_adopt_today_with_test() {
    setup_today
    hs_ok install --test --storages local-lvm,worker-roots --snippet-storage local --yes || return
    expect_acl <<'EOF'
/	token	homestack@pve!server	PVEAuditor
/	token	homestack@pve!test	PVEAuditor
/	user	homestack@pve	PVEAuditor
/pool/homestack-gold	token	homestack@pve!test	HomeStackGold
/pool/homestack-gold	user	homestack@pve	HomeStackGold
/pool/homestack-gold	user	homestack@pve	PVEAuditor
/pool/homestack-test	token	homestack@pve!test	HomeStack
/pool/homestack-test	user	homestack@pve	HomeStack
/pool/homestack-test	user	homestack@pve	PVEAuditor
/sdn/zones/localnetwork/vmbr0	token	homestack@pve!test	PVESDNUser
/sdn/zones/localnetwork/vmbr0	user	homestack@pve	PVEAuditor
/sdn/zones/localnetwork/vmbr0	user	homestack@pve	PVESDNUser
/storage/local	token	homestack@pve!test	HomeStackStorage
/storage/local	user	homestack@pve	HomeStackStorage
/storage/local	user	homestack@pve	PVEAuditor
/storage/local-lvm	token	homestack@pve!test	HomeStackStorage
/storage/local-lvm	user	homestack@pve	HomeStackStorage
/storage/local-lvm	user	homestack@pve	PVEAuditor
/storage/worker-roots	token	homestack@pve!test	HomeStackStorage
/storage/worker-roots	user	homestack@pve	HomeStackStorage
/storage/worker-roots	user	homestack@pve	PVEAuditor
/vms	token	homestack@pve!server	HomeStackServerDisks
/vms	token	homestack@pve!server	PVEAuditor
/vms	user	homestack@pve	HomeStackServerDisks
/vms	user	homestack@pve	PVEAuditor
/vms/101	token	homestack@pve!server	HomeStackServerDisks
/vms/101	token	homestack@pve!server	HomeStackServerGoldGuest
/vms/101	token	homestack@pve!server	PVEAuditor
/vms/101	user	homestack@pve	HomeStackServerDisks
/vms/101	user	homestack@pve	HomeStackServerGoldGuest
/vms/101	user	homestack@pve	PVEAuditor
EOF
    expect_roles <<< "$ROLES_TARGET
HomeStackServerDisks	VM.Config.Disk
HomeStackServerGoldGuest	VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted"
    expect_pools <<< $'homestack-gold\t101\nhomestack-test\t'
    grep -q 'token add' "$HS_FAKE/calls.log" && fail "an existing token was recreated"
    ! compgen -G "$HS_FAKE/homestack-pve-token-*" > /dev/null || fail "a token file was written"
    grep -q 'group' "$HS_FAKE/acl.tsv" || fail "the owner's group ACL was removed"
    # pve2 (local) had the snippets already; pve1 and pve3 get them.
    expect_snippets local pve1 pve3

    : > "$HS_FAKE/calls.log"
    hs_ok install --test --storages local-lvm,worker-roots --snippet-storage local --yes || return
    [[ $(changes) == 0 ]] || fail "second install --test changed: $(cat "$HS_FAKE/calls.log")"

    local server_before
    server_before=$(homestack_acl | grep 'homestack@pve!server')
    hs_ok uninstall --test --yes || return
    [[ $(homestack_acl | grep 'homestack@pve!server') == "$server_before" ]] || fail "uninstall --test changed the server token"
    homestack_acl | grep -q 'homestack@pve!test' && fail "test token entries left"
    grep -q "${tab}test${tab}" "$HS_FAKE/tokens.tsv" && fail "test token left"
    homestack_acl | grep -q 'homestack-test' && fail "user entries on the test pool left"
    expect_pools <<< $'homestack-gold\t101'
    expect_snippets local pve1 pve3
    grep -qx 'homestack@pve' "$HS_FAKE/users" || fail "user removed by uninstall --test"
}

test_pool_with_vms_and_snippets_in_use() {
    setup pve1 pve2
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    printf '240\tpve2\tws\thomestack-ws\tvirtio=BC:24:11:00:00:03,bridge=vmbr0\tvendor=local:snippets/homestack-vendor-keep.yaml\n' \
        >> "$HS_FAKE/vms.tsv"
    sed -i "s/^homestack\t\t/homestack\t240\t/" "$HS_FAKE/pools.tsv"
    hs_ok uninstall --snippet-storage local --yes || return
    expect_out 'homestack: 240'
    grep -q "^homestack${tab}240" "$HS_FAKE/pools.tsv" || fail "pool with a VM was deleted"
    grep -q '^homestack-gold' "$HS_FAKE/pools.tsv" && fail "Gold pool left"
    local where
    for where in local pve2; do
        cmp -s "$(snippet_dir "$where")/homestack-vendor-keep.yaml" "$cloud_init/homestack-vendor-keep.yaml" ||
            fail "the snippet in use was removed from $where"
        [[ ! -e "$(snippet_dir "$where")/homestack-vendor-create.yaml" ]] || fail "the unused snippet was kept on $where"
    done
    expect_out 'homestack-vendor-keep.yaml is used by VM 240'
    grep -q '^240' "$HS_FAKE/vms.tsv" || fail "VM removed"

    hs_ok uninstall --snippet-storage local --yes --force || return
    expect_no_snippets local pve2
    grep -q "^homestack${tab}240" "$HS_FAKE/pools.tsv" || fail "--force deleted a pool with a VM"
}

test_unreachable_node() {
    setup pve1 pve2 pve3
    echo pve3 > "$HS_FAKE/offline-ssh"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'pve3'
    expect_snippets local pve2
    grep -qx 'homestack@pve' "$HS_FAKE/users" || fail "the rest was not done"

    : > "$HS_FAKE/offline-ssh"
    : > "$HS_FAKE/calls.log"
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    expect_snippets local pve2 pve3
    [[ $(changes) == 0 ]] || fail "rerun changed Proxmox objects again"
    expect_out 'on pve3'
    grep -q 'yaml on pve2\|yaml on pve1' "$HS_FAKE/out" && fail "rerun wrote the snippets again where they were"
}

test_offline_node_in_cluster() {
    setup pve1 pve2
    sed -i "s/^pve2\t\(.*\)\t1\t0$/pve2\t\1\t0\t0/" "$HS_FAKE/nodes.tsv"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'pve2 is offline'
}

test_checks_stop_install() {
    setup pve1
    sed -i 's/iso,vztmpl,backup,snippets/iso,vztmpl,backup/' "$HS_FAKE/storage.json"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out "storage local does not allow content type snippets"
    [[ $(changes) == 0 ]] || fail "changed Proxmox before stopping"

    setup pve1
    hs_fails install --storages missing --snippet-storage local --yes || return
    expect_out "storage missing does not exist"

    setup pve1
    sed -i 's/homestack-gold//' "$HS_FAKE/vms.tsv"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'no VM is tagged homestack-gold'

    setup pve1
    printf 'other\t101\tsomeone else\n' > "$HS_FAKE/pools.tsv"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'Gold 101 is in pool other'
    [[ $(changes) == 0 ]] || fail "changed Proxmox before stopping"

    setup pve1
    echo '{"version":"8.4.1","release":"8.4"}' > "$HS_FAKE/version-pve1"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'pve1 runs PVE 8.4.1'

    setup pve1
    echo '{"registered-tags":"homestack-ws;other"}' > "$HS_FAKE/options.json"
    hs_fails install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'tag homestack-ws'

    setup pve1
    hs_fails install --snippet-storage local --yes || return
    expect_out '--storages'
}

test_warnings_do_not_stop_install() {
    setup pve1
    : > "$HS_FAKE/trusted"
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    expect_out 'warning: pve1: the API certificate does not verify'
    expect_out '/etc/pve/pve-root-ca.pem'
}

test_dry_run_and_status_change_nothing() {
    setup pve1 pve2
    hs_ok install --storages local-lvm --snippet-storage local --dry-run || return
    [[ $(changes) == 0 ]] || fail "--dry-run changed Proxmox"
    expect_out 'pveum user add homestack@pve'
    expect_no_snippets local pve2
    hs_fails status --storages local-lvm --snippet-storage local || return
    expect_out 'Missing or different'
    [[ $(changes) == 0 ]] || fail "status changed Proxmox"
}

test_refuses_without_confirmation() {
    setup pve1
    if PATH=$HS_FAKE/bin:$PATH "$script" install --storages local-lvm --snippet-storage local \
        --token-dir "$HS_FAKE" <<< 'n' > "$HS_FAKE/out" 2>&1; then
        fail "install went ahead after 'n'"
    fi
    [[ $(changes) == 0 ]] || fail "changed Proxmox after 'n'"
}

test_snippet_update() {
    setup pve1 pve2
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    echo 'old version' > "$HS_FAKE/nodes/pve2/vz/snippets/homestack-vendor-keep.yaml"
    hs status --storages local-lvm --snippet-storage local && fail "status missed an outdated snippet"
    hs_ok install --storages local-lvm --snippet-storage local --yes || return
    expect_snippets local pve2
}

names=("$@")
if ((${#names[@]} == 0)); then
    mapfile -t names < <(declare -F | awk '$3 ~ /^test_/ { sub(/^test_/, "", $3); print $3 }')
fi
for name in "${names[@]}"; do
    current=$name
    before=$failures
    "test_$name" || true
    if ((failures == before)); then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s\n' "$name"
    fi
done
if ((failures)); then
    printf '%d failure(s)\n' "$failures" >&2
    exit 1
fi
echo "homestack-pve.sh: all ${#names[@]} tests passed"
