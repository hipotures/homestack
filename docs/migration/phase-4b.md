# Phase 4b: install and uninstall in the cluster

**Date:** 2026-10-13
**Branch:** `rust-migration`, tag `rust-phase-4b`
**Scope:** PLAN.md, phase 4b. No Python counterpart: Python runs as root
over SSH and needs no Proxmox objects of its own, so there is nothing in the
mapping or the manifest to port.
**Decisions used:** 1 = A (two static vendor snippets on every node),
9 = yes (Gold in its own pool `homestack-gold`), 10 = C (`VM.Config.Disk`
only on the HomeStack pools; the detached-volume scan says it cannot list
volumes of deleted VMs)

## 1. State before the phase (step 3)

Read on pve2 as root on 2026-10-13 (`pveum acl list`, `pveum role list`,
`pveum user token list`, `pvesh get /pools`, `/cluster/status`, `/storage`,
`/cluster/options`, `/nodes/*/certificates/info`). Everything below was made
by hand in phases 0-4 (`deploy.md`, `decisions.md`).

| Object | State |
|---|---|
| Cluster | `pve`, three nodes `pve1` `.10`, `pve2` `.11`, `pve3` `.12`, all online, PVE 9.2.21 |
| User | `homestack@pve` ("HomeStack server"), tokens `server` and `test` (both privsep) |
| Roles | `HomeStackServerDisks` (`VM.Config.Disk`), `HomeStackServerGoldGuest` (`VM.GuestAgent.FileRead`, `VM.GuestAgent.Unrestricted`), `HomeStackTest` (13 privileges), `HomeStackTestGold` (`VM.Audit`, `VM.Clone`), `HomeStackTestStorage` (`Datastore.AllocateSpace`, `Datastore.Audit`) |
| Pools | `homestack-test` (empty); no `homestack`, no `homestack-gold` |
| ACL, server token | `/` and `/vms`: `PVEAuditor`; `/vms`: `HomeStackServerDisks`; `/vms/101`: `PVEAuditor`, `HomeStackServerDisks`, `HomeStackServerGoldGuest` |
| ACL, test token | `/`: `PVEAuditor`; `/pool/homestack-test`: `HomeStackTest`; `/vms/101`: `HomeStackTestGold`; `/storage/local`, `/storage/local-lvm`: `HomeStackTestStorage`; `/sdn/zones/localnetwork/vmbr0`: `PVESDNUser` |
| ACL, user | the union of both, plus `PVEAuditor` on every path it has a role on |
| Gold | VM 101 on pve2, tag `homestack-gold`, `net0` on `vmbr0`, in no pool |
| Storages (owner's config) | layouts use `worker-roots` (pve1) and `local-lvm` (pve2, pve3); snippets on `local` (`dir`, `/var/lib/vz`, content includes `snippets`, not shared) |
| Snippets | `homestack-vendor-{create,keep}.yaml` on pve2 only (phase 4); Python's per-workspace `homestack-<name>-*.yaml` and Gold's `gold-*.yaml` next to them |
| Tags | no `registered-tags`, no `user-tag-access` (Proxmox's defaults: any user may set any tag) |
| Certificates | every node has `pveproxy-ssl.pem` from Let's Encrypt for `pveN.efez.net` (ACME in Proxmox); `openssl s_client -verify_hostname pve1.efez.net` verifies |

## 2. How Proxmox combines pool and path ACLs

`PVE::RPCEnvironment::permissions` (PVE 9.2.21, read on pve2): the roles of
a VM path are those of the most specific ACL entry on `/vms/<id>`, `/vms`
or `/`, and every role the principal has on `/pool/<pool>` of the VM's pool
is **added** to them (a pool `NoAccess` wins). A privilege-separated token
then gets the intersection of its privileges with its user's on that path.

So:

- a pool role adds to what a token inherits from `/`; it never takes
  anything away, unlike a role on `/vms/<id>`;
- a VM outside the pools gets nothing from the pool roles, so the server
  token can only read it (`PVEAuditor` on `/`);
- the user needs, on every path, at least what each of its tokens has. A
  user entry on a path replaces what the user inherits there, so the user's
  roles on a path must be the union of what each token *resolves* to there
  (its own entry, or the nearest parent's), not only of the tokens' entries
  on that path.

## 3. Target state

Roles (each holds the audit privileges it needs, so no `PVEAuditor` is
added per path):

| Role | Privileges | ACL path |
|---|---|---|
| `HomeStack` | `Pool.Audit`, `VM.Allocate`, `VM.Audit`, `VM.Config.Cloudinit`, `VM.Config.Disk`, `VM.Config.Options`, `VM.GuestAgent.Audit`, `VM.GuestAgent.FileRead`, `VM.GuestAgent.Unrestricted`, `VM.Migrate`, `VM.PowerMgmt` | `/pool/homestack` (server), `/pool/homestack-test` (test) |
| `HomeStackGold` | `Pool.Audit`, `VM.Audit`, `VM.Clone`, `VM.GuestAgent.Audit`, `VM.GuestAgent.FileRead`, `VM.GuestAgent.Unrestricted` | `/pool/homestack-gold` (both tokens) |
| `HomeStackStorage` | `Datastore.AllocateSpace`, `Datastore.Audit` | `/storage/<id>` for every layout storage and the snippet storage (both tokens) |
| `PVESDNUser` (Proxmox's) | `SDN.Audit`, `SDN.Use` | `/sdn/zones/<zone>/<bridge>` of Gold's `net0` (both tokens) |
| `PVEAuditor` (Proxmox's) | read everything | `/` (user and both tokens) |

The user `homestack@pve` gets the union described in section 2, computed by
the script from its tokens' entries.

Differences from the mapping's proposal (api-mapping.md, section 5):

- no `VM.Clone` in `HomeStack`: since Gold has its own pool, cloning needs
  `VM.Clone` on Gold and `VM.Allocate` on the target pool only. The boot
  test of phase 4 checks this in the gate (the test token gets `HomeStack`).
  Whether `import-from` of Gold's disk (refresh, phase 6) needs more on Gold
  is for phase 6 to probe;
- no `VM.Backup`, no `VM.Config.HWType` (decision 1 as chosen by the owner);
- `Pool.Audit` on both pools: without it `cluster/resources` does not show
  `pool` (phase 3);
- no `VM.Config.Disk` on `/vms` (decision 10 = C).

## 4. The script

`deploy/pve/homestack-pve.sh`, one file: Bash, the PVE tools and `perl`
(`JSON::PP`, part of Perl itself, which every PVE node has), `openssl`,
`ssh` and `sha256sum`. The two snippets are embedded, so the owner copies
only the script; a Rust test keeps the embedded copies equal to
`deploy/cloud-init/` and `deploy/pve/embed-snippets.sh` rewrites them.

```
homestack-pve.sh status    [--test] --storages IDS --snippet-storage ID [--gold VMID]
homestack-pve.sh install   [--test] --storages IDS --snippet-storage ID [--gold VMID]
                           [--dry-run] [--yes] [--token-dir DIR]
homestack-pve.sh uninstall [--test] [--snippet-storage ID] [--gold VMID]
                           [--dry-run] [--yes] [--force]

--storages IDS       the storages of [storage_layouts], comma-separated
--snippet-storage ID [cloud_init] snippet_storage (uninstall: to remove the
                     snippets)
--gold VMID          Gold (default: the one VM tagged homestack-gold)
--token-dir DIR      where a new token's secret goes, as
                     DIR/homestack-pve-token-<tokenid> (default /root)
```

### Parts

| Part | Objects | `install` | `install --test` | `uninstall` | `uninstall --test` |
|---|---|---|---|---|---|
| shared | user, roles `HomeStack*`, pool `homestack-gold` with Gold, user's `/` entry, snippets on every node | yes | yes | removed | kept |
| production | pool `homestack`, token `server`, its ACL | yes | untouched | removed | kept |
| test | pool `homestack-test`, token `test`, its ACL | untouched | yes | removed | removed |

A part left untouched keeps its token's entries as they are, and the user's
entries are recomputed from every token that remains. So `install --test`
today moves the test token to the target state and leaves the server
token's hand-made entries (`/vms`, `/vms/101`) alone; the production
install before phase 5 replaces those.

### Adoption

The hand-made objects are taken over: the user and the tokens are kept
(their secrets never change), the old roles `HomeStackServerDisks`,
`HomeStackServerGoldGuest`, `HomeStackTest`, `HomeStackTestGold` and
`HomeStackTestStorage` disappear once no ACL entry uses them, and the
snippets on pve2 are rewritten only if their checksum differs.

### What the script only checks (owner's)

Required (install stops): PVE 9.2 or later on every node; the layout
storages exist and hold `images`; the snippet storage exists and holds
`snippets`; Gold exists, is unique by tag (or given with `--gold`) and is in
no other pool; its bridge exists. Recommended (warning): no
`registered-tags` or `user-tag-access` rule that keeps the token from
setting `homestack-*` tags; each node's API certificate verifies with the
system's roots for a name starting with the node's name (otherwise the
script prints `/etc/pve/pve-root-ca.pem` for `HOMESTACK_PVE_CA_FILE`), and
that name resolves on the node.

### Nodes

`pvesh get /cluster/status` lists the nodes with their addresses and says
whether they are online. The local node is written directly; the others
through `ssh -o BatchMode=yes -o HostKeyAlias=<node> root@<ip>`, which works
in a cluster with the cluster's keys. A shared snippet storage is written
once. A node that is offline or does not answer is reported, the rest is
done, and the script exits with status 1; running it again finishes the job.
Snippets go only to the nodes the storage is enabled on (`nodes`).

### Uninstall

Removes the tokens' and the user's entries, the tokens, the user, the
`HomeStack*` roles, Gold's membership in `homestack-gold` (Gold itself
stays), the empty pools and the snippets. A pool that still holds VMs is
kept and listed with its VMIDs. A snippet that some VM's `cicustom` still
names is kept unless `--force`. Never touches a VM, a volume or anything of
section "What the script only checks". A token's secret file on the node is
not removed (the script says where it is).

## 5. Server

- `HOMESTACK_PVE_POOL` (default `homestack`) and `HOMESTACK_PVE_GOLD_POOL`
  (default `homestack-gold`): the instance's pools. A test instance uses
  `homestack-test`.
- `GET /v1/capabilities` checks workspace privileges on `/pool/<pool>` and
  Gold's on `/pool/<gold pool>` instead of `/vms` and `/vms/<gold_vmid>`,
  and adds `installation`: whether both pools exist, whether Gold is in the
  Gold pool, and the snippets as `unknown` (checking a snippet needs
  `Datastore.Allocate`, which the server does not get).
- The detached-volume scan needs `VM.Config.Disk` on the pool now; with
  decision 10 = C it reports itself incomplete for volumes of deleted VMs,
  as it already does without the privilege.
- Gold readiness gains a check of the snippet's requirements in Gold
  (NetworkManager, `netplan`, `udevadm`, `python3` with `yaml`), Rust only:
  a deliberate difference, removed from the Python comparison by the
  `gold-readiness` normalisation rule.

## 6. Tests (step 4)

- `shellcheck` on the script (`uvx --from shellcheck-py`, the same in CI).
- `deploy/pve/tests/run.sh`: the script on fakes of `pveum`, `pvesh`,
  `pvesm`, `ssh` and `openssl` in `PATH` (one file, `fake-pve`), which record every call
  and keep the cluster's state in files: install on a single node and in a
  three-node cluster, a second install changes nothing, uninstall after
  install returns to zero, the adoption of today's state, a pool with VMs
  stays, a snippet in use stays without `--force`, an offline node, the
  missing storage or Gold stopping the install, `--dry-run` changing
  nothing.
- Rust: the embedded snippets equal `deploy/cloud-init/`; capabilities on
  pool paths; `installation`; the new Gold check in the simulator.

## 7. Gate

With the owner's consent, on the cluster: `install --test --dry-run`, shown
to the owner; `install --test`; a second `install --test` with no change;
the snippets on all three nodes with the same checksum; `check.sh --live`
(the boot test now clones Gold from `homestack-gold` with `HomeStack` on the
test pool); `uninstall --test` leaves the production part; `install --test`
again, so the test pool is there for later phases.

## 8. What was built

- `deploy/pve/homestack-pve.sh` (`status`, `install`, `uninstall`,
  `--test`, `--dry-run`, `--yes`, `--force`, `--gold`, `--token-dir`), with
  both vendor snippets embedded; `deploy/pve/embed-snippets.sh`.
- Server: `HOMESTACK_PVE_POOL`, `HOMESTACK_PVE_GOLD_POOL`; capabilities on
  the pool; `installation` in `GET /v1/capabilities`; the orphan-scan gap
  message names the pool and decision 10; Gold readiness checks
  "Cloud-Init vendor snippet tools".
- `pve-sim`: pools (`GET /pools`, `pool` in `cluster/resources` with
  `Pool.Audit`, pool roles added to `/vms/<vmid>`), the `yaml` import check.
- Parity: rule `gold-readiness`; `rust_drop_items` takes `FIELD=PATTERN`
  for object items.
- `scripts/check.sh` runs shellcheck and the script's tests.

## 9. Tests

- New: 11 script tests on fakes (`deploy/pve/tests/run.sh`), shellcheck;
  19 Rust tests (capabilities on the pool, installation report, pool
  settings, embedded snippets, the Gold check, normaliser). 214 Rust tests in
  all, plus 7 live.
- Changed: `running_gold_checks_guest_contract` (ported from Python) also
  expects the new check after Python's ones.
- Nothing ported from Python: there is nothing to port (section 1).
- `scripts/check.sh`: all passed.

## 10. Parity with Python

Every scenario agrees. The Gold readiness cases (`guest-checks` #1, #2,
`status-cluster` #12) and `live-compare-gold.sh` use `gold-readiness`
instead of `same`.

## 11. Live cluster

Done on 2026-10-13 with the owner's consent:

- `install --test --dry-run`, then `install --test --gold 101` from pve2
  (a second VM, 111 `gold-backup`, is tagged `homestack-gold` too, so the
  script asked for `--gold`): 30 changes, exactly those of the adoption test
  on fakes. The server token's entries did not change.
- A second `install --test`: "Nothing to change".
- Both snippets on pve1, pve2 and pve3 with the same checksums as
  `deploy/cloud-init/` (`0c71bb07…`, `fb337682…`).
- `scripts/check.sh --live`: all passed. The boot test cloned Gold from
  `homestack-gold` with the test token's new roles (`HomeStackGold` on the
  pool, `HomeStack` on `homestack-test`, no `VM.Clone` there) and verified
  the workspace; jobs, status and Gold comparisons agree.

Completed later the same day (root shell on pve2, administration only):

- `uninstall --test`: 12 changes, all of the test part (the test token's
  ACL and the token, the user's entry on `homestack-test`, the empty pool
  `homestack-test`). The server token's ACL was compared before and after:
  unchanged; pools `homestack` and `homestack-gold` (with the test Gold 248)
  and the `HomeStack*` roles stayed. A second run: "Nothing to change".
- `install --test --gold 248` again: a new test token (secret written to
  `/root/homestack-pve-token-test`, moved to the desktop encrypted with a
  one-off RSA key, never shown in clear); the old secret now gets `401`.
  A second run: "Nothing to change". `live_lifecycle.rs` passes with the new
  token.

## 12. Differences from Python

- Gold readiness has one more required check (section 5).
- Changes need privileges on the instance's pool; workspaces outside it
  can only be read (gate before phase 5).

## 13. Open questions

- The live checks clone Gold 101 and run commands in it, and read every VM;
  the owner forbade touching their VMs. How the live gates of later phases
  test Gold-dependent operations needs the owner's answer.
- VM 111 `gold-backup` is tagged `homestack-gold`; Gold readiness and the
  script treat two tagged VMs as ambiguous unless Gold is named.
- `import-from` of Gold's disk (refresh, phase 6) may need more than
  `HomeStackGold`.
- The Python comparisons use Python's configured transport (Herdr).
