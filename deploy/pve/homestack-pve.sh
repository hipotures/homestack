#!/usr/bin/env bash
# HomeStack in a Proxmox VE cluster: check, install and remove what
# HomeStack needs in Proxmox and the server's API cannot make itself.
# Run as root on any node; in a cluster it reaches the other nodes itself.
#
#   homestack-pve.sh status    [--test] --storages IDS --snippet-storage ID
#                              --service-storages IDS --template-storage ID [--gold VMID]
#   homestack-pve.sh install   [--test] --storages IDS --snippet-storage ID
#                              --service-storages IDS --template-storage ID [--gold VMID]
#                              [--dry-run] [--yes] [--token-dir DIR]
#   homestack-pve.sh uninstall [--test] [--snippet-storage ID] [--gold VMID]
#                              [--dry-run] [--yes]
#
#   --test             the test part (pool homestack-test, token test)
#                      instead of the production part (pool homestack,
#                      token server); the shared part is the same
#   --storages IDS     comma-separated storages of [storage_layouts]
#   --snippet-storage  [cloud_init] snippet_storage
#   --service-storages IDS  comma-separated storages for the root disks of
#                      the server containers (content rootdir)
#   --template-storage ID  storage for the server image (content vztmpl)
#   --gold VMID        Gold (default: the only VM tagged homestack-gold)
#   --dry-run          show the plan, change nothing
#   --yes              apply the plan without asking
#   --token-dir DIR    where a new token's secret is written (default /root)
#
# The tokens read nodes, storages and SDN (PVEAuditor on /nodes, /storage,
# /sdn) and see VMs only through the roles of the HomeStack pools: a VM in
# no HomeStack pool is invisible to them.
# The deploy token makes and replaces the server containers through the
# API: in pool homestack-service only, from an OCI image it pulls into the
# template storage.
#
# HomeStack owns: the user homestack@pve, its tokens and ACL entries, the
# roles HomeStack, HomeStackGold, HomeStackStorage and HomeStackDeploy*,
# the pools homestack, homestack-test, homestack-gold (Gold's membership in
# it) and homestack-service (its containers stay on uninstall) and the
# vendor snippets on every node. Everything else belongs to the cluster's
# owner and is only checked: PVE version, storages and their content types,
# Gold and its bridge, tag rules, the nodes' API certificates and names.
# Uninstall never removes a VM, a container, a volume or Gold.
# docs/migration/phase-4b.md has the details.
set -euo pipefail
shopt -s extglob

readonly HS_USER=homestack@pve
readonly POOL_GOLD=homestack-gold
readonly POOL_SERVICE=homestack-service DEPLOY_TOKEN=deploy
readonly MIN_MAJOR=9 MIN_MINOR=2
declare -Ar ROLE_PRIVS=(
    [HomeStack]='Pool.Audit,VM.Allocate,VM.Audit,VM.Config.Cloudinit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,VM.Migrate,VM.PowerMgmt'
    [HomeStackGold]='Pool.Audit,VM.Audit,VM.Clone,VM.GuestAgent.Audit,VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted'
    [HomeStackStorage]='Datastore.AllocateSpace,Datastore.Audit'
    [HomeStackDeploy]='Pool.Audit,VM.Allocate,VM.Audit,VM.Config.CPU,VM.Config.Disk,VM.Config.Memory,VM.Config.Network,VM.Config.Options,VM.PowerMgmt'
    [HomeStackDeployNodes]='Sys.AccessNetwork'
    [HomeStackDeployTemplates]='Datastore.AllocateSpace,Datastore.AllocateTemplate,Datastore.Audit'
)
# Roles made by hand before this script (phases 0-4); removed once unused.
readonly LEGACY_ROLES=(HomeStackServerDisks HomeStackServerGoldGuest HomeStackTest HomeStackTestGold HomeStackTestStorage)
readonly ROLES=(HomeStack HomeStackGold HomeStackStorage HomeStackDeploy HomeStackDeployNodes HomeStackDeployTemplates)
readonly SNIPPETS=(homestack-vendor-create.yaml homestack-vendor-keep.yaml)
readonly TAB=$'\t'
# Fields are read with IFS=$US: a tab in IFS would merge empty fields.
readonly US=$'\037'

usage() {
    sed -n '2,/^set -euo/{/^#/p}' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

die() {
    printf 'homestack-pve: %s\n' "$*" >&2
    exit 1
}

say() { printf '%s\n' "$*"; }

# --- arguments -------------------------------------------------------------

command=${1:-}
case $command in
    status | install | uninstall) shift ;;
    -h | --help | '') usage ;;
    *) die "unknown command '$command' (status, install or uninstall)" ;;
esac
test_part=0 dry_run=0 assume_yes=0
gold_arg='' storages_arg='' snippet_storage='' token_dir=/root
service_storages_arg='' template_storage=''
while (($#)); do
    case $1 in
        --test) test_part=1 ;;
        --dry-run) dry_run=1 ;;
        --yes) assume_yes=1 ;;
        --gold) gold_arg=${2:?--gold needs a VMID}; shift ;;
        --storages) storages_arg=${2:?--storages needs storage IDs}; shift ;;
        --snippet-storage) snippet_storage=${2:?--snippet-storage needs a storage ID}; shift ;;
        --service-storages) service_storages_arg=${2:?--service-storages needs storage IDs}; shift ;;
        --template-storage) template_storage=${2:?--template-storage needs a storage ID}; shift ;;
        --token-dir) token_dir=${2:?--token-dir needs a directory}; shift ;;
        -h | --help) usage ;;
        *) die "unknown option '$1'" ;;
    esac
    shift
done
if ((test_part)); then
    readonly PART=test POOL=homestack-test TOKEN=test
else
    readonly PART=production POOL=homestack TOKEN=server
fi
IFS=, read -r -a storages <<< "$storages_arg"
IFS=, read -r -a service_storages <<< "$service_storages_arg"

# --- reading Proxmox -------------------------------------------------------

# json FIELD...: the FIELDs of every object of the JSON array (or object) on
# stdin, tab-separated, one object per line. Arrays are joined with commas
# (pool members by their vmid), booleans are 1 or 0.
json() {
    perl -MJSON::PP -e '
        local $/;
        my $data = JSON::PP->new->decode(<STDIN>);
        $data = [$data] if ref $data eq "HASH";
        sub text {
            my ($v) = @_;
            return "" if !defined $v;
            return $v ? 1 : 0 if JSON::PP::is_bool($v);
            return join(",", grep { $_ ne "" } map { ref $_ eq "HASH" ? ($_->{vmid} // "") : $_ } @$v) if ref $v eq "ARRAY";
            return "" if ref $v;
            $v =~ s/[\t\n]/ /g;
            return $v;
        }
        for my $item (@$data) {
            print join("\t", map { text($item->{$_}) } @ARGV), "\n";
        }' "$@"
}

get() { pvesh get "$@" --output-format json; }

fields() { tr '\t' "$US"; }

# sorted, comma-separated
sort_list() { tr ' ' '\n' <<< "${1//,/ }" | sed '/^$/d' | sort -u | paste -sd, -; }

in_list() {
    # in_list WORD LIST(comma or semicolon separated)
    local word=$1 item
    local -a items
    IFS=',;' read -r -a items <<< "$2"
    for item in "${items[@]}"; do
        [[ $item == "$word" ]] && return 0
    done
    return 1
}

declare -A node_ip=() node_online=()
nodes=() local_node='' cluster=0
declare -A vm_node=() vm_tags=() vm_pool=() vm_type=()
declare -A storage_type=() storage_content=() storage_nodes=() storage_shared=() storage_disabled=()
declare -A role_privs=()
declare -A pool_members=()
acl_all=
user_exists=0
declare -A token_privsep=()

load_state() {
    local type name ip online is_local id
    while IFS=$US read -r type name ip online is_local; do
        case $type in
            cluster) cluster=1 ;;
            node)
                nodes+=("$name")
                node_ip[$name]=$ip
                node_online[$name]=${online:-0}
                [[ $is_local == 1 ]] && local_node=$name
                ;;
        esac
    done < <(get /cluster/status | json type name ip online local | fields)
    [[ -n $local_node ]] || die "cannot tell which node this is (pvesh get /cluster/status)"

    local vmid node tags pool
    while IFS=$US read -r vmid node type tags pool; do
        [[ -n $vmid ]] || continue
        vm_node[$vmid]=$node
        vm_type[$vmid]=$type
        vm_tags[$vmid]=$tags
        vm_pool[$vmid]=$pool
    done < <(get /cluster/resources --type vm | json vmid node type tags pool | fields)

    local content shared snodes disable
    while IFS=$US read -r id type content shared snodes disable; do
        storage_type[$id]=$type
        storage_content[$id]=$content
        storage_shared[$id]=${shared:-0}
        storage_nodes[$id]=$snodes
        storage_disabled[$id]=${disable:-0}
    done < <(get /storage | json storage type content shared nodes disable | fields)

    local privs
    while IFS=$US read -r id privs; do
        role_privs[$id]=$(sort_list "$privs")
    done < <(pveum role list --output-format json | json roleid privs | fields)

    acl_all=$(pveum acl list --output-format json | json path type ugid roleid | sort -u)

    if pveum user list --output-format json | json userid | grep -qx "$HS_USER"; then
        user_exists=1
        local tokenid privsep
        while IFS=$US read -r tokenid privsep; do
            token_privsep[$tokenid]=${privsep:-1}
        done < <(pveum user token list "$HS_USER" --output-format json | json tokenid privsep | fields)
    fi

    local members
    while IFS=$US read -r id; do
        case $id in
            homestack | homestack-test | "$POOL_GOLD" | "$POOL_SERVICE")
                members=$(get /pools --poolid "$id" | json members)
                pool_members[$id]=$members
                ;;
        esac
    done < <(get /pools | json poolid | fields)
}

vm_config_field() {
    # vm_config_field VMID FIELD
    get "/nodes/${vm_node[$1]}/qemu/$1/config" | json "$2"
}

# --- checks of what belongs to the cluster's owner -------------------------

errors=()
warnings=()
gold=
sdn_path=
snippet_nodes=()

check_versions() {
    local node version major minor
    for node in "${nodes[@]}"; do
        if [[ ${node_online[$node]} != 1 ]]; then
            continue
        fi
        version=$(get "/nodes/$node/version" | json version) || {
            errors+=("$node: cannot read the PVE version")
            continue
        }
        IFS=. read -r major minor _ <<< "$version"
        if ((major < MIN_MAJOR || (major == MIN_MAJOR && minor < MIN_MINOR))); then
            errors+=("$node runs PVE $version; HomeStack needs $MIN_MAJOR.$MIN_MINOR or later")
        fi
    done
    return 0
}

check_storage() {
    # check_storage ID CONTENT
    local id=$1 content=$2
    if [[ -z ${storage_type[$id]+set} ]]; then
        errors+=("storage $id does not exist")
    elif [[ ${storage_disabled[$id]} == 1 ]]; then
        errors+=("storage $id is disabled")
    elif ! in_list "$content" "${storage_content[$id]}"; then
        errors+=("storage $id does not allow content type $content (the cluster's owner adds it: Datacenter > Storage > $id > Content)")
    fi
}

check_storages() {
    local id
    if ((${#storages[@]} == 0)); then
        errors+=("--storages is required: the storages of [storage_layouts]")
    fi
    for id in "${storages[@]}"; do
        check_storage "$id" images
    done
    if [[ -z $snippet_storage ]]; then
        errors+=("--snippet-storage is required: [cloud_init] snippet_storage")
    else
        check_storage "$snippet_storage" snippets
    fi
    if ((${#service_storages[@]} == 0)); then
        errors+=("--service-storages is required: the storages for the server containers' root disks")
    fi
    for id in "${service_storages[@]}"; do
        check_storage "$id" rootdir
    done
    if [[ -z $template_storage ]]; then
        errors+=("--template-storage is required: the storage for the server image")
    else
        check_storage "$template_storage" vztmpl
    fi
    return 0
}

find_gold() {
    local vmid found=()
    if [[ -n $gold_arg ]]; then
        [[ -n ${vm_node[$gold_arg]+set} ]] || { errors+=("Gold $gold_arg does not exist"); return 1; }
        gold=$gold_arg
        return 0
    fi
    for vmid in "${!vm_tags[@]}"; do
        in_list homestack-gold "${vm_tags[$vmid]}" && found+=("$vmid")
    done
    case ${#found[@]} in
        0) errors+=("no VM is tagged homestack-gold: prepare Gold (README, Preparing a Gold VM) or pass --gold"); return 1 ;;
        1) gold=${found[0]} ;;
        *) errors+=("several VMs are tagged homestack-gold (${found[*]}): pass --gold"); return 1 ;;
    esac
}

check_gold() {
    find_gold || return 0
    if [[ ${vm_type[$gold]} != qemu ]]; then
        errors+=("Gold $gold is not a QEMU VM")
        return 0
    fi
    if [[ -n ${vm_pool[$gold]} && ${vm_pool[$gold]} != "$POOL_GOLD" ]]; then
        errors+=("Gold $gold is in pool ${vm_pool[$gold]}; HomeStack needs it in $POOL_GOLD (take it out of ${vm_pool[$gold]} first)")
    fi
    local net0 bridge zone
    net0=$(vm_config_field "$gold" net0) || { errors+=("cannot read Gold $gold's configuration"); return 0; }
    bridge=$(sed -n 's/.*bridge=\([^,]*\).*/\1/p' <<< "$net0")
    if [[ -z $bridge ]]; then
        errors+=("Gold $gold's net0 has no bridge")
        return 0
    fi
    zone=$(get /cluster/sdn/vnets | json vnet zone | awk -F'\t' -v b="$bridge" '$1 == b { print $2 }')
    sdn_path=/sdn/zones/${zone:-localnetwork}/$bridge
}

check_tags() {
    local options registered access allow allow_list tag tags=(homestack-ws homestack-lock)
    ((test_part)) && tags+=(homestack-test)
    options=$(get /cluster/options)
    registered=$(json registered-tags <<< "$options")
    access=$(json user-tag-access <<< "$options")
    allow=$(sed -n 's/.*user-allow=\([a-z]*\).*/\1/p' <<< "$access")
    allow_list=$(sed -n 's/.*user-allow-list=\([^,]*\).*/\1/p' <<< "$access")
    for tag in "${tags[@]}"; do
        if in_list "$tag" "$registered"; then
            errors+=("tag $tag is in registered-tags, so only users with Sys.Modify on / may set it (Datacenter > Options > Tag Style)")
        elif [[ $allow == none ]] || { [[ $allow == list ]] && ! in_list "$tag" "$allow_list"; }; then
            errors+=("tag $tag is not allowed by user-tag-access ($access)")
        elif [[ $allow == existing ]]; then
            warnings+=("user-tag-access is 'existing': tag $tag must already exist in the cluster")
        fi
    done
    return 0
}

check_certificates() {
    local node info names name file
    for node in "${nodes[@]}"; do
        [[ ${node_online[$node]} == 1 ]] || continue
        info=$(get "/nodes/$node/certificates/info" | json filename san) || {
            warnings+=("$node: cannot read the certificate information")
            continue
        }
        file=pveproxy-ssl.pem
        grep -q "^pveproxy-ssl.pem$TAB" <<< "$info" || file=pve-ssl.pem
        names=$(awk -F'\t' -v f="$file" '$1 == f { print $2 }' <<< "$info")
        name=
        local candidate
        local -a sans
        IFS=, read -r -a sans <<< "$names"
        for candidate in "${sans[@]}"; do
            if [[ $candidate == "$node".* ]]; then
                name=$candidate
                break
            fi
        done
        if [[ -z $name ]]; then
            warnings+=("$node: the API certificate ($file) has no name $node.<domain> (names: ${names:-none}); the server must reach $node by a name on its certificate")
            continue
        fi
        if ! openssl s_client -connect "${node_ip[$node]}:8006" -servername "$name" -verify_hostname "$name" \
            -verify_return_error -brief < /dev/null > /dev/null 2>&1; then
            warnings+=("$node: the API certificate does not verify for $name with the system's CAs; give the server /etc/pve/pve-root-ca.pem in HOMESTACK_PVE_CA_FILE, or let Proxmox get a trusted one (ACME)")
        else
            say "ok: $node: API certificate verifies for $name"
        fi
        if ! getent hosts "$name" > /dev/null 2>&1; then
            warnings+=("$node: $name does not resolve on $local_node; the server needs it in its DNS or /etc/hosts")
        fi
    done
    return 0
}

# --- snippets on the nodes -------------------------------------------------

on_node() {
    # on_node NODE COMMAND: run COMMAND as root on NODE (stdin passed on)
    local node=$1
    if [[ $node == "$local_node" ]]; then
        bash -c "$2"
    else
        ssh -o BatchMode=yes -o ConnectTimeout=10 -o "HostKeyAlias=$node" "root@${node_ip[$node]}" "$2"
    fi
}

# BEGIN EMBEDDED SNIPPETS (deploy/pve/embed-snippets.sh writes this part)
snippet_text() {
    case $1 in
        homestack-vendor-create.yaml)
            cat <<'HOMESTACK_SNIPPET'
#cloud-config
# HomeStack vendor data (decision 1 = A). Proxmox generates user-data,
# network-config and meta-data from the VM config (ciuser=root, sshkeys,
# ipconfig0, nameserver, ciupgrade=0); this file adds what they lack,
# mounts the persistent home and appends the keys of sshkeys that root's
# or the user's authorized_keys lack. The same file serves every
# workspace: the home disk is the one whose serial is HS_HOME_<vmid>, the
# account is the image's only regular account. Installed once per node,
# never edited:
# homestack-vendor-create.yaml formats a blank new home,
# homestack-vendor-keep.yaml refuses to.
users: []
disable_root: false
ssh_pwauth: false
ssh_deletekeys: true
preserve_hostname: false
# Proxmox's network-config names no renderer, so netplan would hand eth0 to
# systemd-networkd, which the image masks; Python's network snippet said
# `renderer: NetworkManager`. Netplan's generator reads this file on every
# later boot; the first boot rendered before vendor data ran, so render again
# and let udev re-read netplan's rules, which mark the interface unmanaged
# for NetworkManager until then. NetworkManager is not ordered against
# cloud-init: restart it only if it already runs (`netplan apply` would
# restart the masked networkd and fail).
bootcmd:
  - |
      if [ ! -e /etc/netplan/90-homestack-renderer.yaml ]; then
          (umask 077 && printf 'network:\n  version: 2\n  renderer: NetworkManager\n' > /etc/netplan/90-homestack-renderer.yaml)
          netplan generate
          udevadm control --reload
          udevadm trigger --action=change --subsystem-match=net
          udevadm settle
          if systemctl is-active --quiet NetworkManager.service; then
              systemctl restart --no-block NetworkManager.service
          fi
      fi
runcmd:
  - |
      set -eu
      allow_format=1

      accounts="$(awk -F: '$3 >= 1000 && $3 < 65534 { print $1 ":" $3 ":" $4 }' /etc/passwd)"
      account_count="$(printf '%s\n' "$accounts" | sed '/^$/d' | wc -l)"
      if [ "$account_count" -ne 1 ]; then
          echo "HomeStack: expected exactly one regular account in the image, found $account_count" >&2
          exit 75
      fi
      user_name="${accounts%%:*}"
      user_ids="${accounts#*:}"
      user_uid="${user_ids%%:*}"
      user_gid="${user_ids#*:}"
      home_path="/home/$user_name"

      disks="$(lsblk -dn -o PATH,SERIAL | awk '$2 ~ /^HS_HOME_[0-9]+$/ { print $1 " " $2 }')"
      device_count="$(printf '%s\n' "$disks" | sed '/^$/d' | wc -l)"
      if [ "$device_count" -ne 1 ]; then
          echo "HomeStack: expected exactly one disk with serial HS_HOME_<vmid>, found $device_count" >&2
          exit 70
      fi
      device="${disks%% *}"
      expected_label="${disks#* }"

      fstype="$(blkid -s TYPE -o value "$device" 2>/dev/null || true)"
      fslabel="$(blkid -s LABEL -o value "$device" 2>/dev/null || true)"

      keys="$(cloud-init query userdata | python3 -c 'import sys, yaml; data = yaml.safe_load(sys.stdin) or {}; sys.stdout.write("".join(key + "\n" for key in data.get("ssh_authorized_keys") or []))')"
      if [ "$allow_format" = "1" ] && [ -z "$keys" ]; then
          echo "HomeStack: user-data has no ssh_authorized_keys for the new home" >&2
          exit 76
      fi

      if [ "$fstype" = "ext4" ] && [ "$fslabel" = "$expected_label" ]; then
          :
      elif [ -z "$fstype" ] && [ -z "$fslabel" ]; then
          if [ "$allow_format" != "1" ]; then
              echo "HomeStack: persistent home is blank during refresh; refusing mkfs" >&2
              exit 71
          fi
          if [ "$(lsblk -nr -o TYPE "$device" | wc -l)" -ne 1 ]; then
              echo "HomeStack: new home disk has child block devices; refusing mkfs" >&2
              exit 72
          fi
          if [ -n "$(wipefs -n "$device" 2>/dev/null)" ]; then
              echo "HomeStack: new home disk has filesystem signatures; refusing mkfs" >&2
              exit 73
          fi
          mkfs.ext4 -m 0 -L "$expected_label" "$device"
      else
          echo "HomeStack: unexpected filesystem on $device: type=$fstype label=$fslabel; refusing changes" >&2
          exit 74
      fi

      home_mount_unit="$(systemd-escape --path --suffix=mount "$home_path")"
      systemctl disable --now "$home_mount_unit" >/dev/null 2>&1 || true
      rm -f "/etc/systemd/system/$home_mount_unit"
      mkdir -p "$home_path"
      sed -i "\|[[:space:]]$home_path[[:space:]]|d" /etc/fstab
      printf '\nLABEL=%s %s ext4 defaults 0 2\n' "$expected_label" "$home_path" >> /etc/fstab
      systemctl daemon-reload
      mountpoint -q "$home_path" || mount "$home_path"

      if [ "$allow_format" = "1" ]; then
          rm -rf -- "$home_path/lost+found"
          chown "$user_uid:$user_gid" "$home_path"
          chmod 700 "$home_path"
          install -d -m 700 -o "$user_uid" -g "$user_gid" "$home_path/.ssh"
          printf '%s\n' "$keys" > "$home_path/.ssh/authorized_keys"
          chown "$user_uid:$user_gid" "$home_path/.ssh/authorized_keys"
          chmod 600 "$home_path/.ssh/authorized_keys"
      fi

      # HomeStack ensure-keys (addkey): the keys of user-data that root or
      # the user lack are appended; nothing is removed.
      for dir in /root "$home_path"; do
          [ -d "$dir" ] || continue
          owner=$(stat -c %U "$dir")
          group=$(stat -c %G "$dir")
          f="$dir/.ssh/authorized_keys"
          install -d -m 700 -o "$owner" -g "$group" "$dir/.ssh"
          [ -f "$f" ] || install -m 600 -o "$owner" -g "$group" /dev/null "$f"
          [ -z "$(tail -c1 "$f")" ] || echo >> "$f"
          printf '%s\n' "$keys" | while IFS= read -r key; do
              [ -n "$key" ] || continue
              blob=$(printf '%s' "$key" | awk '{print $2}')
              grep -qF "$blob" "$f" || printf '%s\n' "$key" >> "$f"
          done
      done
      echo 'HomeStack: ensured SSH keys from Cloud-Init'

      if [ -f "$home_path/.config/systemd/user/herdr.service" ]; then
          loginctl enable-linger "$user_name"
      fi
HOMESTACK_SNIPPET
            ;;
        homestack-vendor-keep.yaml)
            cat <<'HOMESTACK_SNIPPET'
#cloud-config
# HomeStack vendor data (decision 1 = A). Proxmox generates user-data,
# network-config and meta-data from the VM config (ciuser=root, sshkeys,
# ipconfig0, nameserver, ciupgrade=0); this file adds what they lack,
# mounts the persistent home and appends the keys of sshkeys that root's
# or the user's authorized_keys lack. The same file serves every
# workspace: the home disk is the one whose serial is HS_HOME_<vmid>, the
# account is the image's only regular account. Installed once per node,
# never edited:
# homestack-vendor-create.yaml formats a blank new home,
# homestack-vendor-keep.yaml refuses to.
users: []
disable_root: false
ssh_pwauth: false
ssh_deletekeys: true
preserve_hostname: false
# Proxmox's network-config names no renderer, so netplan would hand eth0 to
# systemd-networkd, which the image masks; Python's network snippet said
# `renderer: NetworkManager`. Netplan's generator reads this file on every
# later boot; the first boot rendered before vendor data ran, so render again
# and let udev re-read netplan's rules, which mark the interface unmanaged
# for NetworkManager until then. NetworkManager is not ordered against
# cloud-init: restart it only if it already runs (`netplan apply` would
# restart the masked networkd and fail).
bootcmd:
  - |
      if [ ! -e /etc/netplan/90-homestack-renderer.yaml ]; then
          (umask 077 && printf 'network:\n  version: 2\n  renderer: NetworkManager\n' > /etc/netplan/90-homestack-renderer.yaml)
          netplan generate
          udevadm control --reload
          udevadm trigger --action=change --subsystem-match=net
          udevadm settle
          if systemctl is-active --quiet NetworkManager.service; then
              systemctl restart --no-block NetworkManager.service
          fi
      fi
runcmd:
  - |
      set -eu
      allow_format=0

      accounts="$(awk -F: '$3 >= 1000 && $3 < 65534 { print $1 ":" $3 ":" $4 }' /etc/passwd)"
      account_count="$(printf '%s\n' "$accounts" | sed '/^$/d' | wc -l)"
      if [ "$account_count" -ne 1 ]; then
          echo "HomeStack: expected exactly one regular account in the image, found $account_count" >&2
          exit 75
      fi
      user_name="${accounts%%:*}"
      user_ids="${accounts#*:}"
      user_uid="${user_ids%%:*}"
      user_gid="${user_ids#*:}"
      home_path="/home/$user_name"

      disks="$(lsblk -dn -o PATH,SERIAL | awk '$2 ~ /^HS_HOME_[0-9]+$/ { print $1 " " $2 }')"
      device_count="$(printf '%s\n' "$disks" | sed '/^$/d' | wc -l)"
      if [ "$device_count" -ne 1 ]; then
          echo "HomeStack: expected exactly one disk with serial HS_HOME_<vmid>, found $device_count" >&2
          exit 70
      fi
      device="${disks%% *}"
      expected_label="${disks#* }"

      fstype="$(blkid -s TYPE -o value "$device" 2>/dev/null || true)"
      fslabel="$(blkid -s LABEL -o value "$device" 2>/dev/null || true)"

      keys="$(cloud-init query userdata | python3 -c 'import sys, yaml; data = yaml.safe_load(sys.stdin) or {}; sys.stdout.write("".join(key + "\n" for key in data.get("ssh_authorized_keys") or []))')"
      if [ "$allow_format" = "1" ] && [ -z "$keys" ]; then
          echo "HomeStack: user-data has no ssh_authorized_keys for the new home" >&2
          exit 76
      fi

      if [ "$fstype" = "ext4" ] && [ "$fslabel" = "$expected_label" ]; then
          :
      elif [ -z "$fstype" ] && [ -z "$fslabel" ]; then
          if [ "$allow_format" != "1" ]; then
              echo "HomeStack: persistent home is blank during refresh; refusing mkfs" >&2
              exit 71
          fi
          if [ "$(lsblk -nr -o TYPE "$device" | wc -l)" -ne 1 ]; then
              echo "HomeStack: new home disk has child block devices; refusing mkfs" >&2
              exit 72
          fi
          if [ -n "$(wipefs -n "$device" 2>/dev/null)" ]; then
              echo "HomeStack: new home disk has filesystem signatures; refusing mkfs" >&2
              exit 73
          fi
          mkfs.ext4 -m 0 -L "$expected_label" "$device"
      else
          echo "HomeStack: unexpected filesystem on $device: type=$fstype label=$fslabel; refusing changes" >&2
          exit 74
      fi

      home_mount_unit="$(systemd-escape --path --suffix=mount "$home_path")"
      systemctl disable --now "$home_mount_unit" >/dev/null 2>&1 || true
      rm -f "/etc/systemd/system/$home_mount_unit"
      mkdir -p "$home_path"
      sed -i "\|[[:space:]]$home_path[[:space:]]|d" /etc/fstab
      printf '\nLABEL=%s %s ext4 defaults 0 2\n' "$expected_label" "$home_path" >> /etc/fstab
      systemctl daemon-reload
      mountpoint -q "$home_path" || mount "$home_path"

      if [ "$allow_format" = "1" ]; then
          rm -rf -- "$home_path/lost+found"
          chown "$user_uid:$user_gid" "$home_path"
          chmod 700 "$home_path"
          install -d -m 700 -o "$user_uid" -g "$user_gid" "$home_path/.ssh"
          printf '%s\n' "$keys" > "$home_path/.ssh/authorized_keys"
          chown "$user_uid:$user_gid" "$home_path/.ssh/authorized_keys"
          chmod 600 "$home_path/.ssh/authorized_keys"
      fi

      # HomeStack ensure-keys (addkey): the keys of user-data that root or
      # the user lack are appended; nothing is removed.
      for dir in /root "$home_path"; do
          [ -d "$dir" ] || continue
          owner=$(stat -c %U "$dir")
          group=$(stat -c %G "$dir")
          f="$dir/.ssh/authorized_keys"
          install -d -m 700 -o "$owner" -g "$group" "$dir/.ssh"
          [ -f "$f" ] || install -m 600 -o "$owner" -g "$group" /dev/null "$f"
          [ -z "$(tail -c1 "$f")" ] || echo >> "$f"
          printf '%s\n' "$keys" | while IFS= read -r key; do
              [ -n "$key" ] || continue
              blob=$(printf '%s' "$key" | awk '{print $2}')
              grep -qF "$blob" "$f" || printf '%s\n' "$key" >> "$f"
          done
      done
      echo 'HomeStack: ensured SSH keys from Cloud-Init'

      if [ -f "$home_path/.config/systemd/user/herdr.service" ]; then
          loginctl enable-linger "$user_name"
      fi
HOMESTACK_SNIPPET
            ;;
        *) die "no embedded snippet $1" ;;
    esac
}
# END EMBEDDED SNIPPETS

snippet_sum() { snippet_text "$1" | sha256sum | cut -d' ' -f1; }

snippet_path() { pvesm path "$snippet_storage:snippets/$1"; }

choose_snippet_nodes() {
    local node
    local -a allowed
    IFS=, read -r -a allowed <<< "${storage_nodes[$snippet_storage]:-}"
    if [[ ${storage_shared[$snippet_storage]:-0} == 1 ]]; then
        snippet_nodes=("$local_node")
        return
    fi
    for node in "${nodes[@]}"; do
        if ((${#allowed[@]} == 0)) || in_list "$node" "${storage_nodes[$snippet_storage]}"; then
            snippet_nodes+=("$node")
        fi
    done
    return 0
}

declare -A node_sums=()
node_errors=()

read_snippet_sums() {
    local node paths=() name sums
    for name in "${SNIPPETS[@]}"; do
        paths+=("$(printf '%q' "$(snippet_path "$name")")")
    done
    for node in "${snippet_nodes[@]}"; do
        [[ ${node_online[$node]} == 1 ]] || continue
        if ! sums=$(on_node "$node" "for f in ${paths[*]}; do if [ -f \"\$f\" ]; then sha256sum \"\$f\"; fi; done" < /dev/null); then
            node_errors+=("$node does not answer over SSH (root@${node_ip[$node]}); run this again when it does")
            continue
        fi
        node_sums[$node]=$sums
    done
    return 0
}

# The snippets on NODE whose content is not the embedded one.
snippets_to_write() {
    local node=$1 name path sum
    for name in "${SNIPPETS[@]}"; do
        path=$(snippet_path "$name")
        sum=$(awk -v p="$path" '$2 == p { print $1 }' <<< "${node_sums[$node]}")
        [[ $sum == "$(snippet_sum "$name")" ]] || printf '%s\n' "$name"
    done
    return 0
}

snippets_present() {
    local node=$1 name path
    for name in "${SNIPPETS[@]}"; do
        path=$(snippet_path "$name")
        awk -v p="$path" '$2 == p { found = 1 } END { exit !found }' <<< "${node_sums[$node]}" && printf '%s\n' "$name"
    done
    return 0
}

# shellcheck disable=SC2317 # run through the plan (eval)
write_snippet() {
    # write_snippet NODE NAME
    local path
    path=$(snippet_path "$2")
    snippet_text "$2" | on_node "$1" "$(printf 'set -e; umask 022; mkdir -p %q; cat > %q.homestack-new; mv -f %q.homestack-new %q' \
        "$(dirname "$path")" "$path" "$path" "$path")"
}

# shellcheck disable=SC2317 # run through the plan (eval)
remove_snippet() {
    on_node "$1" "$(printf 'rm -f %q' "$(snippet_path "$2")")" < /dev/null
}

# --- the plan --------------------------------------------------------------

plan_desc=()
plan_cmd=()

plan() {
    # plan DESCRIPTION COMMAND...
    plan_desc+=("$1")
    shift
    plan_cmd+=("$(printf '%q ' "$@")")
}

managed_acl() {
    awk -F'\t' -v u="$HS_USER" '$3 == u || index($3, u "!") == 1' <<< "$acl_all"
}

token_target_acl() {
    # token_target_acl TOKENID POOL
    local who=$HS_USER!$1 id
    for id in /nodes /storage /sdn; do
        printf '%s\ttoken\t%s\tPVEAuditor\n' "$id" "$who"
    done
    printf '/pool/%s\ttoken\t%s\tHomeStack\n' "$2" "$who"
    printf '/pool/%s\ttoken\t%s\tHomeStackGold\n' "$POOL_GOLD" "$who"
    for id in "${storages[@]}" "$snippet_storage"; do
        printf '/storage/%s\ttoken\t%s\tHomeStackStorage\n' "$id" "$who"
    done
    printf '%s\ttoken\t%s\tPVESDNUser\n' "$sdn_path" "$who"
}

deploy_target_acl() {
    local who=$HS_USER!$DEPLOY_TOKEN id
    for id in /nodes /storage /sdn; do
        printf '%s\ttoken\t%s\tPVEAuditor\n' "$id" "$who"
    done
    printf '/nodes\ttoken\t%s\tHomeStackDeployNodes\n' "$who"
    printf '/pool/%s\ttoken\t%s\tHomeStackDeploy\n' "$POOL_SERVICE" "$who"
    printf '/storage/%s\ttoken\t%s\tHomeStackDeployTemplates\n' "$template_storage" "$who"
    for id in "${service_storages[@]}"; do
        [[ $id == "$template_storage" ]] && continue
        printf '/storage/%s\ttoken\t%s\tHomeStackStorage\n' "$id" "$who"
    done
    printf '%s\ttoken\t%s\tPVESDNUser\n' "$sdn_path" "$who"
}

# The user's entries for the token entries on stdin: on every path a token
# has an entry on, the union of what each token resolves to there (its own
# entry or the nearest parent's), since a token gets the intersection with
# its user and a user's entry replaces what the user inherits.
user_acl() {
    awk -F'\t' -v u="$HS_USER" '
        { roles[$3 SUBSEP $1] = roles[$3 SUBSEP $1] " " $4; paths[$1] = 1; tokens[$3] = 1 }
        END {
            for (p in paths) for (t in tokens) {
                a = p
                while (1) {
                    if ((t SUBSEP a) in roles) {
                        n = split(roles[t SUBSEP a], r, " ")
                        for (i = 1; i <= n; i++) out[p SUBSEP r[i]] = 1
                        break
                    }
                    if (a == "/") break
                    sub(/\/[^\/]*$/, "", a)
                    if (a == "") a = "/"
                }
            }
            for (k in out) { split(k, kk, SUBSEP); print kk[1] "\tuser\t" u "\t" kk[2] }
        }'
}

# Target entries of the user and its tokens after this run.
target_acl() {
    local keep_user=$1
    shift
    local -a scope=("$@")
    local tokens
    tokens=$(managed_acl | awk -F'\t' '$2 == "token"' | {
        local line tokenid skip
        while IFS= read -r line; do
            tokenid=$(cut -f3 <<< "$line")
            tokenid=${tokenid#*!}
            skip=0
            for t in "${scope[@]}"; do [[ $t == "$tokenid" ]] && skip=1; done
            ((skip)) || printf '%s\n' "$line"
        done
    })
    if [[ $command != uninstall ]]; then
        tokens+=$'\n'$(token_target_acl "$TOKEN" "$POOL")
        tokens+=$'\n'$(deploy_target_acl)
    fi
    tokens=$(sed '/^$/d' <<< "$tokens" | sort -u)
    if ((keep_user)); then
        { printf '%s\n' "$tokens"; user_acl <<< "$tokens"; } | sed '/^$/d' | sort -u
    else
        printf '%s\n' "$tokens" | sed '/^$/d' | sort -u
    fi
}

# Plan ACL changes from the current to the target entries, grouped per path
# and principal: additions for the user first, removals for tokens first.
plan_acl() {
    local target=$1 current changes kind path type ugid roles flag
    current=$(managed_acl | sort -u)
    for kind in add del; do
        if [[ $kind == add ]]; then
            changes=$(comm -13 <(printf '%s\n' "$current") <(printf '%s\n' "$target") | sed '/^$/d')
        else
            changes=$(comm -23 <(printf '%s\n' "$current") <(printf '%s\n' "$target") | sed '/^$/d')
        fi
        [[ -n $changes ]] || continue
        while IFS=$US read -r path type ugid roles; do
            flag=--users
            [[ $type == token ]] && flag=--tokens
            if [[ $kind == add ]]; then
                plan "give $ugid $roles on $path" pveum acl modify "$path" "$flag" "$ugid" --roles "$roles"
            else
                plan "take $roles on $path from $ugid" pveum acl delete "$path" "$flag" "$ugid" --roles "$roles"
            fi
        done < <(awk -F'\t' '{ k = $1 "\t" $2 "\t" $3; r[k] = r[k] (r[k] == "" ? "" : ",") $4 } END { for (k in r) print k "\t" r[k] }' <<< "$changes" |
            if [[ $kind == add ]]; then sort -t "$TAB" -k2,2r -k1,1; else sort -t "$TAB" -k2,2 -k1,1; fi | fields)
    done
}

roles_in_use() {
    # roles_in_use TARGET-MANAGED-ACL: roles any ACL entry uses after this run
    { awk -F'\t' -v u="$HS_USER" '!($3 == u || index($3, u "!") == 1) { print $4 }' <<< "$acl_all"
      cut -f4 <<< "$1"; } | sed '/^$/d' | sort -u
}

# shellcheck disable=SC2317 # run through the plan (eval)
create_token() {
    # create_token TOKENID FILE COMMENT: the secret goes only to FILE (mode 0600)
    local out value
    out=$(pveum user token add "$HS_USER" "$1" --privsep 1 --comment "$3" --output-format json)
    value=$(json value <<< "$out")
    [[ -n $value ]] || die "pveum returned no secret for $HS_USER!$1"
    (umask 077 && printf '%s!%s=%s\n' "$HS_USER" "$1" "$value" > "$2")
}

plan_token() {
    # plan_token TOKENID COMMENT
    local file=$token_dir/homestack-pve-token-$1
    if [[ -z ${token_privsep[$1]+set} ]]; then
        [[ -e $file ]] && die "$file exists; move it away so the new token's secret can be written there"
        plan "create token $HS_USER!$1 (secret to $file, mode 0600)" create_token "$1" "$file" "$2"
    elif [[ ${token_privsep[$1]} != 1 ]]; then
        plan "separate the privileges of token $HS_USER!$1" pveum user token modify "$HS_USER" "$1" --privsep 1
    fi
}

plan_install() {
    local role target used name node
    ((user_exists)) || plan "create user $HS_USER" pveum user add "$HS_USER" --comment 'HomeStack server'
    for role in "${ROLES[@]}"; do
        if [[ -z ${role_privs[$role]+set} ]]; then
            plan "create role $role" pveum role add "$role" --privs "${ROLE_PRIVS[$role]}"
        elif [[ ${role_privs[$role]} != "${ROLE_PRIVS[$role]}" ]]; then
            plan "set the privileges of role $role (were ${role_privs[$role]})" pveum role modify "$role" --privs "${ROLE_PRIVS[$role]}"
        fi
    done
    [[ -n ${pool_members[$POOL_GOLD]+set} ]] || plan "create pool $POOL_GOLD" pveum pool add "$POOL_GOLD" --comment 'Gold'
    [[ -n ${pool_members[$POOL_SERVICE]+set} ]] || plan "create pool $POOL_SERVICE" pveum pool add "$POOL_SERVICE" --comment 'Containers'
    if [[ -z ${pool_members[$POOL]+set} ]]; then
        if ((test_part)); then
            plan "create pool $POOL" pveum pool add "$POOL" --comment 'Test Workspaces'
        else
            plan "create pool $POOL" pveum pool add "$POOL" --comment 'Workspaces'
        fi
    fi
    [[ ${vm_pool[$gold]} == "$POOL_GOLD" ]] || plan "put Gold $gold in pool $POOL_GOLD" pveum pool modify "$POOL_GOLD" --vms "$gold"
    plan_token "$TOKEN" "HomeStack $PART"
    plan_token "$DEPLOY_TOKEN" 'HomeStack deploy'
    target=$(target_acl 1 "$TOKEN" "$DEPLOY_TOKEN")
    plan_acl "$target"
    used=$(roles_in_use "$target")
    for role in "${LEGACY_ROLES[@]}"; do
        if [[ -n ${role_privs[$role]+set} ]] && ! grep -qx "$role" <<< "$used"; then
            plan "delete role $role (made by hand, no longer used)" pveum role delete "$role"
        fi
    done
    for node in "${snippet_nodes[@]}"; do
        [[ -n ${node_sums[$node]+set} ]] || continue
        while IFS= read -r name; do
            [[ -n $name ]] && plan "write $name on $node" write_snippet "$node" "$name"
        done < <(snippets_to_write "$node")
    done
    return 0
}

# VMs whose cicustom names a snippet, as "NAME VMID" lines; unreadable
# configurations are reported.
snippet_users() {
    local vmid cicustom name
    for vmid in "${!vm_node[@]}"; do
        [[ ${vm_type[$vmid]} == qemu ]] || continue
        if ! cicustom=$(vm_config_field "$vmid" cicustom 2> /dev/null); then
            printf '? %s\n' "$vmid"
            continue
        fi
        for name in "${SNIPPETS[@]}"; do
            [[ $cicustom == *"snippets/$name"* ]] && printf '%s %s\n' "$name" "$vmid"
        done
    done
    return 0
}

kept=()

plan_uninstall() {
    local -a scope pools
    local target used role pool members vmid rest node name users
    if ((test_part)); then
        scope=(test)
        pools=(homestack-test)
    else
        scope=(server test "$DEPLOY_TOKEN")
        pools=(homestack homestack-test "$POOL_GOLD" "$POOL_SERVICE")
    fi
    if ((test_part)); then
        target=$(target_acl 1 "${scope[@]}")
    else
        target=$(target_acl 0 "${scope[@]}")
    fi
    plan_acl "$target"
    for name in "${scope[@]}"; do
        [[ -n ${token_privsep[$name]+set} ]] && plan "delete token $HS_USER!$name" pveum user token delete "$HS_USER" "$name"
    done
    if ((!test_part)); then
        ((user_exists)) && plan "delete user $HS_USER" pveum user delete "$HS_USER"
        used=$(roles_in_use "$target")
        for role in "${ROLES[@]}" "${LEGACY_ROLES[@]}"; do
            [[ -n ${role_privs[$role]+set} ]] || continue
            if grep -qx "$role" <<< "$used"; then
                kept+=("role $role: still used by an ACL entry of someone else")
            else
                plan "delete role $role" pveum role delete "$role"
            fi
        done
    else
        used=$(roles_in_use "$target")
        for role in "${LEGACY_ROLES[@]}"; do
            [[ -n ${role_privs[$role]+set} ]] && ! grep -qx "$role" <<< "$used" &&
                plan "delete role $role (made by hand, no longer used)" pveum role delete "$role"
        done
    fi
    for pool in "${pools[@]}"; do
        [[ -n ${pool_members[$pool]+set} ]] || continue
        members=${pool_members[$pool]}
        if [[ $pool == "$POOL_GOLD" ]]; then
            rest=
            for vmid in ${members//,/ }; do
                if [[ $vmid == "$gold_arg" ]] || in_list homestack-gold "${vm_tags[$vmid]:-}"; then
                    plan "take Gold $vmid out of pool $POOL_GOLD (Gold stays)" pveum pool modify "$POOL_GOLD" --vms "$vmid" --delete 1
                else
                    rest+=${rest:+,}$vmid
                fi
            done
            members=$rest
        fi
        if [[ -z $members ]]; then
            plan "delete pool $pool" pveum pool delete "$pool"
        else
            kept+=("pool $pool: ${members//,/ } (its VMs stay; move or destroy them first)")
        fi
    done
    ((test_part)) && return 0
    users=$(snippet_users)
    for name in "${SNIPPETS[@]}"; do
        # A VM without its snippet does not start: a snippet a VM names, or that
        # cannot be checked, is never removed (the owner, 2026-10-15).
        if grep -q '^? ' <<< "$users"; then
            kept+=("$name: cannot read every VM's configuration ($(awk '$1 == "?" { print $2 }' <<< "$users" | paste -sd' ' -)); it stays")
            continue
        fi
        vmid=$(awk -v n="$name" '$1 == n { print $2 }' <<< "$users" | sort -n | paste -sd' ' -)
        if [[ -n $vmid ]]; then
            kept+=("$name is used by VM $vmid (cicustom); it stays")
            continue
        fi
        for node in "${snippet_nodes[@]}"; do
            [[ -n ${node_sums[$node]+set} ]] || continue
            grep -qx "$name" <(snippets_present "$node") && plan "remove $name from $node" remove_snippet "$node" "$name"
        done
    done
    return 0
}

# --- main ------------------------------------------------------------------

command -v pvesh > /dev/null || die "pvesh not found: run this on a Proxmox VE node"

load_state
if ((cluster)); then
    say "$local_node: cluster of ${#nodes[@]} nodes (${nodes[*]})"
else
    say "$local_node: single node"
fi
for node in "${nodes[@]}"; do
    [[ ${node_online[$node]} == 1 ]] || node_errors+=("$node is offline; run this again when it is back")
done

if [[ $command == uninstall ]]; then
    if [[ -n $gold_arg ]]; then
        find_gold || true
    fi
else
    check_versions
    check_storages
    check_gold
    check_tags
    check_certificates
fi
if [[ -n $snippet_storage && -n ${storage_type[$snippet_storage]+set} ]]; then
    choose_snippet_nodes
    read_snippet_sums
elif [[ $command == uninstall && $test_part == 0 ]]; then
    kept+=("the vendor snippets: pass --snippet-storage to remove them")
fi

for message in "${warnings[@]}"; do
    say "warning: $message"
done
if ((${#errors[@]})); then
    for message in "${errors[@]}"; do
        say "error: $message"
    done
    die "nothing changed: the cluster's owner fixes the errors above first"
fi

case $command in
    install | status) plan_install ;;
    uninstall) plan_uninstall ;;
esac

for message in "${kept[@]}"; do
    say "kept: $message"
done

status=0
if ((${#plan_desc[@]} == 0)); then
    say "Nothing to change ($PART part)."
else
    if [[ $command == status ]]; then
        say "Missing or different from the target state ($PART part); install would:"
    else
        say "Plan ($command, $PART part):"
    fi
    for i in "${!plan_desc[@]}"; do
        printf '%3d. %s\n     $ %s\n' "$((i + 1))" "${plan_desc[$i]}" "${plan_cmd[$i]}"
    done
    if [[ $command == status ]]; then
        status=1
    elif ((dry_run)); then
        say "Dry run: nothing changed."
    else
        if ((!assume_yes)); then
            printf 'Apply these %d changes? [y/N] ' "${#plan_desc[@]}"
            read -r answer || answer=
            [[ $answer == @(y|Y|yes) ]] || die "nothing changed"
        fi
        for i in "${!plan_desc[@]}"; do
            say "- ${plan_desc[$i]}"
            eval "${plan_cmd[$i]}" || die "failed: ${plan_desc[$i]}; run this again to finish"
        done
        say "Done."
    fi
fi
if [[ $command == uninstall && $test_part == 0 ]]; then
    say "A token's secret file is not removed: look for $token_dir/homestack-pve-token-* on the node that made it."
fi
if ((${#node_errors[@]})); then
    for message in "${node_errors[@]}"; do
        say "error: $message"
    done
    status=1
fi
exit "$status"
