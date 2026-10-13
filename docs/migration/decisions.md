# Migration decisions

Decisions from section 10 of [api-mapping.md](api-mapping.md) and the results
of probes on the live cluster. Only the project owner records a decision here;
an agent stops when a phase needs one that is still open
([PLAN.md](PLAN.md), 1.1 and the gate before phase 4).

## Decisions

| # | Question | Options | Needed by | Status |
|---|---|---|---|---|
| 1 | Cloud-Init snippets | 7.1 A native Cloud-Init + static vendor snippet, B shared snippet storage, C SSH helper | phase 4 | **decided 2026-10-13: A.** Proxmox generates hostname, network, DNS and keys from the VM config; one generic vendor snippet, installed once per node, finds and mounts the home. If the boot test in the test pool (gate of phase 4) fails, fall back to B. No SSH key on the server (C rejected). **2026-10-13 (owner, phase 4):** the per-VM `allow_format` flag is carried by the choice of file, not `smbios1`: two static snippets, `homestack-vendor-create.yaml` (formats a blank new home) and `homestack-vendor-keep.yaml` (refuses), selected through `cicustom vendor=`; no `VM.Config.HWType` |
| 2 | Keep the Proxmox name of the root volume (no rename) | 7.2 A yes, B rename through the helper | phase 4 | **decided 2026-10-12: A** (the owner left it to the agent). Roots keep the name Proxmox gives them (`vm-<id>-disk-N`); homes keep `vm-<id>-hs-home-<user>`, which the API accepts at allocation. The detached-volume scan recognises roots by a VMID that no longer exists. No node access is needed for it |
| 3 | `create` on another node | 7.3 A clone + migrate, B backup storage (PBS), C Gold replica per node, D helper stream | phase 10 | **decided 2026-10-13: A.** A full clone of Gold on its node into the first storage of that node's layout, migrated offline with its disks to the target node and storage (`POST migrate`), then made a workspace there. Only the API; no PBS or shared backup storage exists in the cluster (NFS1 holds only snippets). Costs room for one root on Gold's node for a while and two copies of it |
| 4 | Where the `refresh` journal lives | 7.4 A shared storage, B VM `description`, C helper | phase 4 | **decided 2026-10-13: B**, in the VM's `description`, written with `digest`. Owner's rules: (1) the journal is written before each step it describes; (2) it is removed only after the result is verified; (3) a later `refresh` of the VM finds it, checks every recorded stage against the VM's actual state, and continues or rolls back only when they agree; (4) every mismatch is reported in full and nothing is deleted while the state disagrees with the journal; (5) new option `force` (not in Python): attach the new root even when the state is inconsistent, never deleting a volume, since any root (old or new) works with the home. `force` is a deliberate difference for phase 6 |
| 5 | Guest verification without `VM.GuestAgent.Unrestricted` (`Audit` + `FileRead`) | yes / no | phase 3 | **decided 2026-10-12: yes.** D2 uses the `get-*` agent commands and `file-read`; `exec` (`VM.GuestAgent.Unrestricted`) stays only for `resize` and the Gold guest contract (E2) |
| 6 | LUKS passphrase for `resize` | stays in the client / sent to the server for `input-data` | phase 7 | **decided 2026-10-13: sent to the server.** The client sends the passphrase with the request that applies the plan, never in the plan; the server passes it to `cryptsetup --key-file -` as `input-data` of `agent/exec` (stdin, not the arguments) and keeps it only in memory for that job. A LUKS root unlocked by a key file stays as it is (in the guest, through QGA) |
| 7 | Setup and repo stay in the client in the first stage (7.5 A) | yes / no | phase 14 | open |
| 8 | Client authentication to the server | bearer token / plus hardware-key signature for destructive operations | not set by the plan | open |
| 9 | Gold in its own pool with narrower privileges | yes / no | before phase 5 | **decided 2026-10-13: yes.** Gold goes into its own pool `homestack-gold`, where the server token gets only what cloning and the Gold checks need (`VM.Audit`, `VM.Clone`, the guest agent for E2), granted on `/pool/homestack-gold` instead of `/vms/<gold_vmid>`. Gold is not in pool `homestack`, so the server cannot change or delete it |
| 10 | Detached-volume scan once write privileges are limited to pool `homestack` (PLAN.md, gate before phase 5) | A keep `VM.Config.Disk` on `/vms` (cannot delete a VM, but can detach and delete the disks of every VM, the desktop included); B `Datastore.Allocate` on the HomeStack storages (complete scan, but may delete any volume on them); C `VM.Config.Disk` only on the pool: volumes of deleted VMs are not listed, the scan is reported incomplete | before phase 5 | **decided 2026-10-13: C.** `VM.Config.Disk` only on the HomeStack pools; `VM.Config.Disk` on `/vms` is removed. The scan reports that volumes of deleted VMs cannot be listed |

### Owner's rules of 2026-10-13 (phase 5)

- The HomeStack tokens see no VM outside the HomeStack pools, not even to
  read it: `PVEAuditor` only on `/nodes`, `/storage`, `/sdn`, VMs only through
  the pool roles (`homestack-pve.sh`). The server also refuses reads and
  operations on a VM outside its pools (`outside_pool`). Workspaces made
  before the pool (200–210) are invisible until the owner moves them into
  `homestack`.
- VM 101 is the owner's production Gold and VM 100 the desktop: the agent
  and the live tests never touch them. VM 101 was taken out of
  `homestack-gold` again; production `create` needs the owner to put the
  Gold of their choice there.
- The live tests use the agent's own test Gold, VM 248 `hst-gold` (Ubuntu
  24.04 cloud image prepared per README, in `homestack-gold`), and
  `crates/hs-server/tests/data/live-server.toml`. Python's live status
  comparison is no longer run (it reads every workspace of the owner).
- Herdr's root shell on pve2 is allowed only for administration on the node
  (`homestack-pve.sh`), never to build or test the application.

### Owner's decision of 2026-10-13 (phase 6)

- `PUT /nodes/{node}/qemu/{vmid}/unlink` has no `digest` (probe below), so
  Python's `qm disk unlink ... --digest` cannot succeed. The owner: the
  server unlinks without `digest`. It reads the config again right before
  the call and checks that the slot holds the expected volume, and checks
  the result after it; the journal write just before each unlink carries
  the config's `digest`, so a change made by someone else before that write
  is still caught. Python is not changed by the agent.

### Owner's decision of 2026-10-13 (phase 8)

- Python's `migration_volume_key` keeps `.raw`, `.qcow2` and `.vmdk` (its
  pattern `r"\\.(?:raw|..."` matches a backslash, not a dot), so the
  migration progress of a disk from a directory storage loses its role and
  size. The owner: the server strips the extension, as Python meant to;
  Python is not changed by the agent.

### Owner's decision of 2026-10-13 (phase 9)

- `addkey` (C4) under decision 1 = A, both ways at once (phase 4, open
  question 4):
  - the two static vendor snippets get a key step: on every new Cloud-Init
    instance they append the keys of user-data that are missing to root's
    and the user's `authorized_keys`, removing nothing (Python's
    `ensure-keys`); the owner installs them again on the nodes with
    `homestack-pve.sh install`;
  - `addkey` sets `sshkeys` and switches `cicustom` to the keep snippet
    (Python's `allow_format=0`), so a stopped VM gets the key at its next
    start;
  - a running VM also gets the key at once through `agent/exec`, without a
    restart; a restart happens only when asked for.
- No ACL change: role `HomeStack` already holds
  `VM.GuestAgent.Unrestricted`.
- `cluster/resources` lists a VM as running for a few seconds after its
  shutdown. The power state is still read from it, as in Python; the
  server's shutdown returns only once that list also says the VM is
  stopped (up to 30 s).
- After some starts PVE keeps a VM's lock for 10–25 s ("can't lock file
  ... got timeout"; the owner sees it in the Proxmox UI too). Every call
  refused that way is made again every 5 seconds, for up to 2 minutes.

### Owner's decisions of 2026-10-13 (phase 11)

- **Deployment only through the Proxmox API.** Herdr, SSH to a node or a
  root shell are never part of deploying or running the Rust server; any
  user deploys it the same way. The agent's Herdr access only helps
  checking.
- **Image:** CI publishes a public OCI image
  `ghcr.io/hipotures/homestack-server:<version>`. A deploy command, run on
  the desktop, pulls it on each node (`POST
  /nodes/{node}/storage/{storage}/oci-registry-pull`), creates or replaces
  that node's container (`POST /nodes/{node}/lxc`) and starts it.
- **One instance per node:** CTID 997 on pve1 (`192.168.100.97`, root
  disk on `local-zfs`), 998 on pve2 (`.98`, `local-lvm`), 999 on pve3
  (`.99`, `local-lvm`).
- **TLS:** Caddy in the image, in front of the server. One wildcard
  certificate `*.hs.efez.net` (instances `api1`–`api3.hs.efez.net`, A
  records in the local zone `hs.efez.net`) from Let's Encrypt by DNS-01 in
  the public zone `efez.net` (OVH). The deploy command obtains and renews
  it on the desktop and sends the certificate and its key to each instance
  through the server's API; the OVH key stays on the desktop. The OVH key
  is the one already configured in Proxmox's ACME settings; for phase 11
  the agent copies it once as root on pve2 into the desktop's
  `~/.config/homestack/ovh-dns` (mode `0600`), never printing it.
- **Secrets through the server's API:** the container's environment holds
  the SHA-256 of the client token; the deploy command sends the Proxmox
  token with write-only `PUT /v1/secrets/{name}` (api-mapping 8.1), kept by
  the server mode `0600`.
- **The certificate at creation** (owner, after the probe below): the image
  is public, and the API cannot put a file into a container, so the
  certificate and its key go into the container's `env` (base64) before
  its first start; the server stores them in its own files (`0600`) and the
  deploy command removes them from the container's configuration right
  after. Only the deploy token and root can read that configuration (pool
  `homestack-service`). TLS is then the real certificate from the first
  start, and the tokens follow through the API over it.
- **Old server images stay** in the template storage: removing one needs
  `Datastore.Allocate`, which would allow removing anything on that
  storage. The deploy command lists them; the owner removes them.
- **A separate deploy token:** `homestack-pve.sh install` adds pool
  `homestack-service`, role `HomeStackDeploy` and token
  `homestack@pve!deploy`, used only by the deploy command (ACL change
  approved). The owner runs the installer and uninstaller; for phase 11 the
  agent runs it on pve2 once, as before.
- **Live gate on test instances:** the three instances run with the test
  token and pool `homestack-test`; the gate removes the containers after.
  Production uses the same command with the server token.
- **Lock between instances** (agent's choice within PLAN.md, 8.3): a lease
  in the VM's `description` (instance, job, expiry), written with `digest`,
  as the `refresh` journal of decision 4.

## Probe results

| Date | Probe | Result |
|---|---|---|
| 2026-10-12 | PVE version | 9.2.21 (`repoid 4f6e0ac86f9e8c7f`) on all three nodes |
| 2026-10-12 | Pools | none exist yet (neither `homestack` nor `homestack-test`) |
| 2026-10-12 | Test resources for phase 3 (created with the owner's approval) | pool `homestack-test` (empty; VMIDs 240–249 free); roles `HomeStackTest` (the privileges of `hs-core` capabilities), `HomeStackTestGold` (`VM.Audit`, `VM.Clone`), `HomeStackTestStorage` (`Datastore.AllocateSpace`, `Datastore.Audit`); token `homestack@pve!test` (privsep) with `PVEAuditor` on `/`, `HomeStackTest` on `/pool/homestack-test`, `HomeStackTestGold` on `/vms/101`, `HomeStackTestStorage` on `/storage/local` and `/storage/local-lvm`, `PVESDNUser` on `/sdn/zones/localnetwork/vmbr0`; the user `homestack@pve` has the same roles plus `PVEAuditor` on each of those paths. Secret in `~/.config/homestack/pve-token-test` (0600) |
| 2026-10-12 | Local DNS for workspaces | master zone `hs.efez.net` on the Synology router (DNS Server, `192.168.100.1`); updates limited to TSIG key `homestack` (HMAC-SHA512, secret in `~/.config/homestack/dns-tsig.key`, 0600). `nsupdate -k` added and removed `hs-test.hs.efez.net`; both router addresses served it; an update without the key was `REFUSED`. Public `efez.net` (OVH) is untouched. No phase uses it yet |
| 2026-10-12 | Node names in local DNS | master zones `pve1.efez.net`, `pve2.efez.net`, `pve3.efez.net` on the router's DNS Server, each with an apex A record (`192.168.100.10/11/12`); both router addresses answer and TLS to `https://pveX.efez.net:8006` verifies with the answers. A Synology zone created without "Master DNS server" has no NS record and fails with `SERVFAIL` |
| 2026-10-12 | Guest agent at audit level (phase 2) | On all running HomeStack VMs (qemu-ga 10.2), `get-fsinfo` reports the disk serial as `0QEMU_QEMU_HARDDISK_drive-<slot>`, not the `serial=` of the VM config, and has no filesystem label. `total-bytes-privileged`, `used-bytes` and `total-bytes − used-bytes` equal `df`'s size, used and avail for `/home/user` on VMs 200, 204, 206, 207 and 210 (201 changed between the reads) |
| 2026-10-12 | Listing VM volumes (phase 2) | `GET /nodes/{n}/storage/{s}/content` returns nothing to a PVEAuditor token: `PVE::Storage::check_volume_access` requires `Datastore.Audit` on the storage and `VM.Config.Disk` on the owner VM, or `Datastore.Allocate` on the storage. The orphan scan therefore needs `VM.Config.Disk` on `/vms` |
| 2026-10-12 | Server token may list VM volumes (owner's decision after phase 2) | role `HomeStackServerDisks` (`VM.Config.Disk`); `/vms`: `PVEAuditor` and `HomeStackServerDisks` for `homestack@pve` and `homestack@pve!server`; `/vms/101`: `HomeStackServerDisks` added for the user. The server token now has `VM.Config.Disk` on every VMID (existing or not) and still only audit privileges elsewhere; the test token is unchanged. 28 volumes visible on pve2 `local-lvm` |
| 2026-10-12 | ACL inheritance | a role for a user on a specific path replaces what the user inherits from `/` there, and a privilege-separated token gets the intersection with its user: giving `homestack@pve` only the test roles took `PVEAuditor` away from `homestack@pve!server` on those paths until `PVEAuditor` was added for the user on each of them |
| 2026-10-13 | `import-from` variants (phase 6, `scripts/live-probe-import-from.sh`, test token, VM 240 cloned from the test Gold 248 in `homestack-test`, `local-lvm` on pve2) | Both variants work with the test token's privileges (`VM.Clone` on the Gold through `homestack-gold`, `VM.Config.Disk` on the pool, `Datastore.AllocateSpace` on the storage). **A**, VM running: `POST config` with `scsi29=local-lvm:0,import-from=local-lvm:vm-248-disk-0` and `digest` is a task (UPID); after it, `scsi29` is hot-plugged (not pending), 8 GiB in about 5 s; `PUT /unlink idlist=scsi29` without `force` moves it to `unused0` and leaves `scsi0` alone; `PUT /unlink idlist=unused0 force=1` deletes it. **B**, VM stopped: `import-from` into `scsi0` (with the old root's options) attaches the copy and moves the old root to `unused0`, about 4 s. `PUT /unlink` takes only `idlist` and `force`: with `digest` it fails `400 ... digest: property is not defined in schema` (qemu-server `API2/Qemu.pm` `unlink`; `qm disk unlink` is the same method, so Python's `qm disk unlink ... --digest` cannot succeed, see `phase-6.md`). A config write with a stale `digest` fails `500 checksum mismatch (file change by other user?)`. `description` takes at most 8192 characters (`400 value may only be 8192 characters long`), and 8192 are kept as written. A `DELETE` with its parameters in the body fails (`Unexpected content for method 'DELETE'`); they go in the query. VM 240 destroyed |
| 2026-10-12 | `GET /access/permissions?path=P` (phase 1) | answers for any path, existing or not, with the privileges inherited from parents (e.g. `/` → `/vms`), as `{"data":{"P":{"Priv":1,...}}}`. A privilege-separated token may query its own permissions with no extra privilege. The Administrator role has 47 privileges on PVE 9.2.21 |
| 2026-10-12 | Server token may run commands in Gold (owner's decision before phase 3, for E2) | role `HomeStackServerGoldGuest` (`VM.GuestAgent.Unrestricted`, `VM.GuestAgent.FileRead`); `/vms/101`: added for `homestack@pve`; for `homestack@pve!server` `PVEAuditor`, `HomeStackServerDisks` and `HomeStackServerGoldGuest` (a token ACL on `/vms/101` replaces what the token inherits from `/vms`). The server token ran `hostname` through `agent/exec` and read `/etc/hostname` through `agent/file-read` on Gold; `file-read` on VM 200 is refused (`VM.GuestAgent.FileRead|VM.GuestAgent.Unrestricted`). The test token is unchanged (`VM.Audit`, `VM.Clone` on Gold) |
| 2026-10-12 | `file-read` on Gold (phase 3) | `GET agent/file-read` takes `file`, `offset`, `count` (default and maximum 16 MiB) and `decode` (`0` returns base64): reading 256 bytes at offset 1024 of `/dev/dm-1` (Gold's root) gave the ext4 magic `0xEF53` and the same (empty) label as `blkid`. `/run/cloud-init/result.json` and `/run/cloud-init/cloud-id` (`nocloud`, as `cloud-id` prints) are readable; a missing file fails with `500 Agent error: failed to open file '...' (mode: 'r'): No such file or directory`, a directory with `500 Agent error: failed to read file: Is a directory`. `command -v` in `/bin/sh -lc` as root searches `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin` |
| 2026-10-12 | Task logs (phase 3) | `GET tasks/{upid}/log` answers an empty log (a task that has not written yet) with one made-up line `{"n":1,"t":"no content"}` (`PVE::Tools::dump_logfile`); a log with lines answers `start` past its end with `[]`. A token's task has `user` `homestack@pve`, `tokenid` `test` and the UPID ends in `homestack@pve!test:`. Shutting down a VM without a guest fails after `timeout` with `VM quit/powerdown failed - got timeout` (log `TASK ERROR: ...`) |
| 2026-10-12 | Test token in its pool (phase 3) | creating a VM with `pool=homestack-test` works with `VM.Allocate` on the pool, but `memory` or `cores` need `VM.Config.Memory` / `VM.Config.CPU`, which the test token does not have (HomeStack clones Gold and sets neither); a bare VM gets 512 MiB and one core. Without `Pool.Audit` the token does not see `pool` in `cluster/resources`. Start, shutdown, stop and `DELETE ?purge=1` of VM 240 worked with the test token |
| 2026-10-13 | Native Cloud-Init boot test (phase 4 gate, owner's approval to install the vendor snippets on pve2) | Gold 101 cloned by the test token to VM 240 in `homestack-test` with `ciuser=root`, `ciupgrade=0`, `nameserver`, `ipconfig0`, `cicustom=vendor=local:snippets/homestack-vendor-create.yaml` and a 1 GiB `scsi1` with `serial=HS_HOME_240` (setting `cicustom` needed no new privilege). Proxmox's user-data: `hostname`, `manage_etc_hosts`, `fqdn`, `user: root`, `disable_root: False`, `ssh_authorized_keys`, `chpasswd`, no `users:`; meta-data `instance-id` = SHA1 of user-data and network-config (vendor data not included); network-config v1 with no renderer. First boot: hostname and mounted, formatted home with the keys right, but no IPv4: netplan gave `eth0` to the masked `systemd-networkd` (Python's network snippet said `renderer: NetworkManager`). Fixed in the vendor snippet: a `bootcmd` writes `/etc/netplan/90-homestack-renderer.yaml` (`renderer: NetworkManager`), runs `netplan generate` and `udevadm trigger` (netplan's udev rule had left the device `NM_UNMANAGED`) and restarts NetworkManager only if it already runs (`netplan apply` fails restarting the masked networkd). Then: address, default route and DNS (`192.168.100.1 1.1.1.1`, plus `search efez.net` from the node, since no `searchdomain` is set), reachable from the desktop; the same after a reboot; `keep` with a changed `sshkeys` ran again (new instance) and left the home and its `authorized_keys` alone, adding the key to root only; `keep` on a blank disk stopped with `persistent home is blank during refresh; refusing mkfs` and left it blank. Side effects of `user: root`: cloud-init writes `/etc/sudoers.d/90-cloud-init-users` with a rule for root (sudo is not installed) and reports `degraded done` for the deprecated string `user`; root's password was already locked in Gold. VM 240 destroyed, no volumes left |
| 2026-10-13 | Server containers from an OCI image through the API (phase 11, deploy token `homestack@pve!deploy` after `homestack-pve.sh install --test` on pve2, CT 998) | `POST /nodes/pve2/storage/local/oci-registry-pull` with `reference=docker.io/library/alpine:3.24` is a task (`ociregistrypull`) that writes `local:vztmpl/alpine_3.24.tar` (Datastore.AllocateTemplate on the storage, Sys.AccessNetwork on the node). `POST /nodes/pve2/lxc` with that `ostemplate`, `pool=homestack-service`, `rootfs=local-lvm:1`, `unprivileged=1` and `net0=name=eth0,bridge=vmbr0,ip=192.168.100.98/24,gw=192.168.100.1` logs "Detected OCI archive" and "Auto-Enabling host-managed network", and sets `ostype alpine`, `cmode console`, `lxc.signal.halt SIGTERM`; `entrypoint` and `env` come from the image (the `entrypoint` given at creation was ignored). `PUT config` changes both; `env` is NUL-separated (`%00`). Started, the CT had `192.168.100.98/24` (`GET .../interfaces`) and answered ping from the desktop. Stop and `DELETE ?purge=1&destroy-unreferenced-disks=1` worked; no volume left. Removing the image (`DELETE .../content/local:vztmpl/...`) needs `Datastore.Allocate` on the storage and was refused; root removed it |
