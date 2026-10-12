# Phase 2: reads

**Date:** 2026-10-12
**Branch:** `rust-migration`, tag `rust-phase-2`
**Scope:** A1–A8, D1, D3, E1, E3 (read); [api-mapping.md](api-mapping.md)

## 1. Python behaviour (step 3)

Every function below was read and ported; behaviour and messages are
Python's unless section 6 says otherwise.

### Global status (`status.global_status`, A1)

1. `GET /cluster/resources?type=vm` (QEMU entries with an integer VMID, by
   VMID), then `GET /nodes` (`cluster_node_statuses`: status lower-cased,
   `online` unless `offline`, by name; no nodes is an error).
2. Managed VMs: the configured Gold VMID plus everything tagged
   `homestack-ws` or `homestack-alien`. Their configs are read only on online
   nodes (an offline node's VM has an empty config).
3. Gold: missing → warning `Configured Gold VM N does not exist.`; tags from
   the config, else from the inventory; `role_tag_ok` needs `homestack-gold`
   and not `homestack-alien`, with a warning naming the problem.
4. Workspaces (managed, not Gold), in VMID order: alien or offline → no home;
   no home disk → `missing <slot>` and `missing_count`; else a serial check
   (`<slot> serial 'X' != 'HS_HOME_<vmid>'`, Python `repr` quoting) and the
   home usage. Warnings become `Workspace VM N: ....`
5. Home usage (`guest.workspace_home_usage`, D3): size from the config's
   `size=`; when running and the agent pings, the label of the filesystem at
   `/home/<user>` must be `HS_HOME_<vmid>`, then `df` gives size, used, avail
   and the use percentage (`free_percent = 100 − pcent`).
6. Detached volumes (`orphaned_homestack_volumes`, A8): per node, an offline
   node with assigned storages makes the scan incomplete without a warning;
   the node's storage list is read even when it has no assigned storage;
   only assigned, enabled, active storages with `images` content are
   scanned; a missing assigned storage is a warning; volumes named
   `vm-<id>-hs-(root|home)-<name>` (extension stripped) and attached to no
   disk slot are detached. The scan is complete only without warnings.
7. Capacity (`homestack_storage_capacities`, A7): every storage of every
   node's layout; offline nodes are not asked; `percent = 100·used/total`.
8. Totals: `free_percent` of homes only when every workspace home's usage is
   known; `running`/`stopped` count aliens too.

### Per-VM status (`status.workspace_status`, A2) and targets (A3)

- Role from the config's tags (Gold by VMID; `ALIEN`, `WS`, or none with a
  warning); IP `prefix.<vmid>` for VMIDs 2–254; the home disk is ignored for
  aliens; `home_identity_ok` compares the config serial with the label.
- For a running VM with a pinging agent, Python runs through `exec`:
  `hostname`, `ip -4 ... eth0`, `findmnt` (source, type, target), `findmnt |
  blkid` (label) and `getent passwd ubuntu`.
- `resolve_workspace_target`: digits are a VMID; a name must match
  `[A-Za-z][A-Za-z0-9.-]{0,62}` and exactly one VM tagged `homestack-ws` or
  `homestack-alien` (configs read in inventory order); ambiguous, untagged
  and unknown names have their own messages.
- `resolve_existing_workspace` (for lifecycle operations): refuses Gold,
  aliens and untagged VMs, needs a name, a root and a home disk on different
  volumes and the right home serial, then reads the home usage.

### Discovery (A5), networks (A6), Gold (E1), tags (E3)

- `discover_environment`'s Proxmox part: `/version`, nodes, the
  inventory's `homestack-gold` VMs (`vmid`, `name`, `node`, `status`,
  `tags` where present) and storage definitions (`storage`, `type`,
  `content`, `nodes`, `shared` where present).
- `discover_workspace_networks`: Gold's `net0` bridge; DNS fallback from the
  Gold node's `/dns` (errors ignored); each workspace on that bridge with a
  static `ipconfig0` in a `.0` network of at most /24 (config errors
  skipped), then Gold's own `ipconfig0`, then the node's bridges; candidates
  with the same network and bridge merge, keeping the first source.
- `check_gold_readiness`, Proxmox part: role tag, root slot, no extra data
  disks or NICs, a MAC on `net0`, a Cloud-Init drive, the agent option, the
  root in the boot order, the power state; when stopped, usable `sshkeys`
  are required and the guest checks are skipped. The guest checks of a
  running Gold need `exec` (phase 3).
- Tag parsing (`parse_tags`, `has_tag`): semicolons, trimmed, exact names.

### Bug-like behaviour

None found that changes results. Python includes the shell command in
warnings about failed reads (`Remote command failed (255) on pve2: pvesh
get ...`); the server cannot, see section 6.

## 2. What was built

| Part | Where |
|---|---|
| Reads, status, targets, orphans, capacity | `crates/hs-core/src/{inventory,status}.rs` |
| Discovery and network profiles | `crates/hs-core/src/discovery.rs` |
| Gold readiness (Proxmox part, agent ping) | `crates/hs-core/src/gold.rs` |
| `Pve` trait (real client and test doubles) | `crates/hs-core/src/pve.rs` |
| Response types (the CLI's JSON shapes) | `crates/hs-model/src/status.rs` |
| Routes `/v1/status`, `/v1/workspaces/{target}`, `/v1/storage`, `/v1/orphans`, `/v1/discovery`, `/v1/networks`, `/v1/gold/readiness` | `crates/hs-server/src/reads.rs` |
| Capability `orphan_scan` | `hs-model`, `hs-core` capabilities |
| `PveClient::get_value` / `post_value` | `crates/hs-pve/src/client.rs` |
| Simulator: VMs, configs, storages, volumes, network, DNS, guest agent (audit reads and `exec`) | `crates/pve-sim` |
| Python harness: `qm guest` commands, `func_driver` | `parity/python/homestack_parity` |
| Several comparisons per scenario, richer rules, `hs-parity compare` | `crates/hs-parity` |
| Scenario with 12 comparisons | `scenarios/status-cluster.toml` |
| Live comparison | `scripts/live-compare-status.sh` (in `check.sh --live`) |

**Guest at audit level.** The plan allows only `ping`, `get-fsinfo`,
`get-host-name` and `network-get-interfaces`. Measured on the live cluster:
`get-fsinfo`'s `total-bytes-privileged`, `used-bytes` and `total-bytes −
used-bytes` are exactly `df`'s size, used and avail, so home usage keeps
Python's numbers. The agent reports no filesystem label, and its disk serial
is `0QEMU_QEMU_HARDDISK_drive-<slot>` rather than the config's `serial=`; the
home mount is identified by the QEMU drive of the home slot instead of the
label.

## 3. Tests

Written first and shown failing (none of `hs_core::{pve, inventory,
status, discovery, gold}` existed).

| | Count |
|---|---|
| Python tests of phase 2 | 37 |
| ported | 31 |
| moved | 6: `GlobalSummaryTests` ×4 and the discovery progress test to phase 13 (terminal output, Herdr scan), `test_missing_rsync_is_optional` to phase 3 (guest `exec`) |
| excluded | 0 |
| Rust tests in the workspace | 154 (100 before), plus 5 live |
| Python harness tests | 18 (15 before) |

The ports use `FakePve`, which answers from a table and panics on any other
request, as the Python tests' fake sessions raise `AssertionError`.

## 4. Parity with Python

`scenarios/status-cluster.toml`: one cluster with Gold (stopped), a healthy
workspace, a stopped one, one without an answering agent, one without home,
one on an offline node, one with a wrong home serial, an alien VM, an
unmanaged VM and two detached volumes. 12 comparisons, all equal after
normalization: global status, six per-VM statuses (by VMID and by name),
storage, orphans, discovery, networks and Gold readiness. The operations
match too, apart from the guest calls each side makes in its own way (the
pings are compared). A deliberately weakened rule makes the comparison fail
(tried).

## 5. Live cluster

`scripts/check.sh --live`, all passed:

- `live-compare-status.sh`: Python `status --json` (its own config and
  Herdr transport) and the server's `/v1/status` at the same time, then
  `status VMID` and `/v1/workspaces/VMID` for all 10 HomeStack VMs (Gold,
  the alien VM and every workspace, including a stopped one, one on pve3
  and one with `homestack-lock`). All agree under `status-global-live` and
  `status-workspace`.
- The phase 0 and 1 gates (image, failover, configuration persistence,
  capabilities) passed in the same run. Nothing was written to Proxmox.

## 6. Decisions taken in this phase and differences from Python

1. **Audit-level guest reads** (plan, phase 2): no `mounted_home_label`
   (`get-fsinfo` has no label; `home_mount_ok` uses the home slot's QEMU
   drive instead), no `ubuntu_user_present` (needs `exec`; belongs to the
   Gold guest contract, phase 3). For a home on a device-mapper device
   (LUKS, LVM), `home_mount` shows `/dev/dm-N` where `findmnt` shows
   `/dev/mapper/NAME`.
2. **Orphan scan and privileges.** Proxmox hides VM volumes from a token
   without `VM.Config.Disk` on the VM. The server checks its own privileges
   and reports an incomplete scan with the reason instead of an empty list
   (`/v1/status`, `/v1/orphans`); capability `orphan_scan` names the need.
   With the current read-only token the live scan is incomplete; there are no
   detached volumes on the cluster today (Python sees none either).
3. **Response shapes** are the CLI's JSON without `ok`, `command`, `scope`,
   `config`, `execution` and the display settings (`client.toml`).
4. **Warning texts** after a failed Proxmox read name the Proxmox error, not
   the `pvesh` command line Python ran.
5. **Errors.** A target that is unknown, unmanaged or absent is `404`; an
   invalid or ambiguous one `422`; messages do not repeat the target.
6. **Gold readiness of a running Gold** pings the agent; the guest contract
   is reported as `skip` (`needs guest exec ...; checked from phase 3`).
   `/v1/gold/readiness` and `/v1/networks` take a VMID, so the installer can
   use them before a configuration exists.

## 7. Open questions for the next phases

1. Answered after the tag: the owner gave the server token `VM.Config.Disk`
   on `/vms` (decisions.md). The live comparison now also requires the orphan
   scans to agree.
2. Decision 5 (guest verification without `Unrestricted`) is needed before
   phase 4; this phase shows that usage and the home mount can be read at
   audit level, but the filesystem label and accounts cannot.
3. Phase 3 needs the guest `exec` path for the Gold contract
   (`test_missing_rsync_is_optional`, `test_running_gold_checks_guest_contract`).
