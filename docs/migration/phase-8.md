# Phase 8: `migrate` (B6)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** B6 `migrate`: offline migration of a workspace with all its local
disks to another node, the power state kept, the persistent home verified;
event `workspace.migrated`
**Decisions used:** 1 = A (static vendor snippets on every node, so C3, the
snippet copy, is not needed), the rules of the gate before phase 5 (only VMs
of the instance's pool), and the owner's decision of 2026-10-13 on
`migration_volume_key` (`decisions.md`)

## 1. Python behaviour (step 3)

All in `python/src/homestack/lifecycle.py`.

### Plan (`build_migrate_plan`)

1. `status.resolve_existing_workspace(require_network=False)`: not Gold,
   exists, has a node, the role tags (`homestack-alien` refused before any
   `qm` command, `test_resize_destroy_and_migrate_plans_reject_alien_before_guest_or_disk_work`),
   a valid name, root and home on different volumes, the home's serial
   `HS_HOME_<vmid>`.
2. `check_remote_requirements` on the source and the target node (`qm`,
   `pvesh`, `perl`, ... present): a check of the SSH transport, not of the
   workspace.
3. The target is the VM's node: "VM <vmid> is already on node <node>".
4. Any `unusedN` key in the config: "VM <vmid> has detached/unused disk
   references (unused0=<volume>, ...); Proxmox migration scans them too.
   Clean or resolve them before migration."
5. `resolve_homestack_storage(cfg, target_node, target_storage)`: the
   storage must belong to the target node's layout
   (`homestack-storage-<n>`); absent, the layout's first storage.
6. `pvesh get /nodes/<target>/status` must succeed: "Target node '<node>'
   is not available in the Proxmox cluster".
7. The plan: `vmid`, `name`, `source_node`, `target_node`,
   `target_storage`, `status` (the power state to keep), `home_disk`,
   `home_label`, `home_volume`, `volumes` (`migration_volume_plan`),
   `migration_mode`, `snippet_policy`.

`migration_volume_plan`: every volume of `status.vm_volume_inventory`
except `unused`, with `slot`, `role`, `volume`, `name` (the volume's base
name, `migration_volume_key`) and `size_bytes` (`int(size_gb * 1024**3)`
from `size=`, or `None`).

### Execution (`migrate_workspace`)

1. `require_workspace_tag` on the source node's config: an alien or untagged
   VM is refused before the snippets and the shutdown
   (`test_migrate_execution_rejects_alien_before_snippets_or_shutdown`).
2. `sync_snippets_to_node`: the four per-VM snippets copied to the target
   through the control node, before the shutdown; a failure leaves the VM
   running (`SnippetMigrationTests`).
3. A running VM is shut down (`shutdown_vm_on_node`, never forced); from
   here a failure starts it again (step 9).
4. `qm migrate <vmid> <target> --with-local-disks --targetstorage <storage>`
   on the source node, up to 7200 s. The output feeds
   `MigrationProgressTracker`: the last `Logical volume "<name>" created.`
   names the disk being copied (its role and size from the plan's
   `volumes` by `name`), the last `<bytes> bytes (...) copied, <s> s, <speed>`
   after it the bytes copied; the percentage is clamped to 0–100 and the
   detail is `<copied> / <total> · <speed>` (`human_bytes`), or only the
   copied bytes when the size is unknown.
5. `cluster/resources` must put the VM on the target: "Migration command
   completed but VM <vmid> is on '<node>', expected '<target>'".
6. The target's config must keep the home's `serial` = the plan's
   `home_label`: "Persistent home identity check failed after migration to
   <target>".
7. `qm cloudinit update <vmid>` on the target.
8. A VM that was running: `qm start` on the target, the agent within
   180 s, then `findmnt -n -o SOURCE,FSTYPE,TARGET /home/<user>` must
   contain `ext4` and the label of its device must be the plan's
   `home_label`: "Migration completed but persistent home verification
   failed on <target>: mount='...', label='...'".
9. The final status must be the plan's (`running` or `stopped`): "Migration
   completed but VM <vmid> power state is '<s>'; expected '<s>' to preserve
   its pre-migration state". A stopped VM is never started
   (`test_stopped_workspace_stays_stopped_after_migration`), a running one
   is started on the target (`test_running_workspace_is_started_after_migration`).
10. On any error after this job shut the VM down: when `cluster/resources`
    still puts the VM on the source node and it is not running, `qm start`
    on the source; the error gets "Recovery: VM <vmid> was restarted
    automatically on <source>." or "Recovery: VM <vmid> is still on
    <source>, but automatic restart failed: <output>", or "Recovery check
    failed for VM <vmid> on <source>: <error>". A VM already on the target
    is left as it is.
11. The result: the plan plus `ok`, `status`, `snippets_copied`,
    `home_preserved`, `power_state_preserved`. BK keys are not touched
    (`MigrationPowerStateTests.setUp`).

### A bug in Python

`migration_volume_key` strips the extension with
`re.sub(r"\\.(?:raw|qcow2|vmdk)$", "", tail)`. In a raw string `\\.` is a
backslash followed by any character, so `vm-200-disk-0.raw` keeps `.raw`.
Migrating from a directory storage to LVM then never matches the created
logical volume to the plan, and the progress shows role `disk` without a
size. The owner decided (2026-10-13) that the server strips the extension;
Python is not changed.

### Tests in scope

| Python test | Rust |
|---|---|
| `test_lifecycle.py::MigrationProgressTests` (2) | ported |
| `test_lifecycle.py::MigrationPowerStateTests` (2) | ported, on the simulator |
| `test_alien_workspace.py::...reject_alien_before_guest_or_disk_work` (migrate part) | ported |
| `test_alien_workspace.py::test_migrate_execution_rejects_alien_before_snippets_or_shutdown` | ported (no snippets: before the shutdown) |
| `test_proxmox.py::SnippetMigrationTests` (2) | excluded: no per-VM snippets under decision 1 = A (phase 4, open question 5) |

## 2. What was ported (B6)

| Python | Rust |
|---|---|
| `build_migrate_plan` | `hs_core::migrate::plan_migrate`: the pool first (gate before phase 5), then `resolve_existing_workspace`, the same node, `unusedN`, the target's layout storage, `GET /nodes/{target}/status` |
| `migration_volume_plan`, `migration_volume_key` | `migration_volume_plan`, `migration_volume_key` (the extension is stripped, decision of 2026-10-13) |
| `MigrationProgressTracker` | `MigrationTracker`; as a `LogTracker` of `tasks::wait_task_tracking` it turns the `qmigrate` log into the job's `progress` |
| `migrate_workspace` | `migrate_workspace`: tags, pool, node and home checked again; `status/shutdown`; `POST /migrate` with `target`, `with-local-disks=1`, `targetstorage`; the VM's node and the home's `serial` on the target; `status/start`; the home through `get-fsinfo` and its superblock (`verify::verify_migrated_home`); the final status; the restart on the source after a failure |
| `qm cloudinit update` | none: Proxmox writes the Cloud-Init drive at every start (decision 1 = A) |
| `sync_snippets_to_node` (C3) | none (decision 1 = A) |
| CLI `migrate VMID NODE --target-storage S` | operation `workspace.migrate` with `vmid`, `target_node`, `target_storage` (optional); event `workspace.migrated` |

`pve-sim` gained `POST /nodes/{node}/qemu/{vmid}/migrate`: a `qmigrate` task
that moves every disk (`unusedN` too) to the target storage on the target
node and writes the log lines Python's tracker reads; refusals before the
task (a running VM without `online`, the source as target, an unknown
target, a storage the target lacks or that holds no images, the missing
`Datastore.AllocateSpace`); a failed task when the target storage has not
enough free space or the target is offline, with nothing moved.

The capability table (`hs_core::capabilities`, B6) drops
`VM.Config.Cloudinit` (no `qm cloudinit update`) and adds
`VM.GuestAgent.FileRead` (the home's label is read from its superblock).
The roles of `homestack-pve.sh` already hold both, so no ACL changes.

## 3. Tests

| Kind | Count | Where |
|---|---|---|
| Python tests ported | 6 | `crates/hs-core/tests/migrate.rs` |
| Python tests excluded | 2 | `SnippetMigrationTests`: decision 1 = A, no snippet copy |
| New, `hs-core` on the simulator | 10 | the same file: the volume key, the tracker without a size and with a file volume, the plan, the refusals, the requested storage, a workspace moved after the plan, a failed migration restarted on the source, a failed restart, a failure on the target |
| New, `hs-server` | 3 | `crates/hs-server/tests/migrate.rs`: the job's steps, tasks and event; the refusals when planning (422, 409, 404); a failed job with the recovery note |
| Live | 1 | `crates/hs-server/tests/live_migrate.rs` in `check.sh --live` |

Manifest: 593 Python tests, 110 ported, 411 todo, 72 excluded.

## 4. Live cluster (L4)

`scripts/check.sh --live` passed in full on 2026-10-13, with
`live_migrate.rs` (134 s): VM 240 created on pve2 from the test Gold 248, a
marker written into its home, migrated running to pve3 (`local-lvm`):
tasks `qmshutdown`, `qmigrate`, `qmstart`, running on pve3, the marker
read back; shut down and migrated stopped back to pve2: only `qmigrate`,
stopped on pve2; started, the marker read back; destroyed. The vendor
snippets on every node (phase 4b) let the VM boot on pve3.

Seen on PVE 9.2 and copied into the simulator:

- The offline migration's log: the Cloud-Init drive is "found generated
  disk '...' (in current VM config)", the others "found local disk '...'
  (attached)"; per disk `Logical volume "<name>" created.`, `Logical volume
  pve/<name> changed.`, `dd` lines `<n> bytes (...) copied, <s> s, <speed>`
  with `records in/out`, "successfully imported", "volume '...' is '...' on
  the target"; then the source volumes "successfully removed" and
  "migration finished successfully". Every line but the removals carries a
  timestamp. Python's tracker patterns match these lines.
- The volume names stay the same on the target storage.

Not checked live: a migration that fails after the shutdown (the restart
on the source is tested on the simulator), and pve1 with `worker-roots`.

## 5. Deliberate differences from Python

1. **No snippet copy and no `qm cloudinit update`** (decision 1 = A). The
   result keeps `snippets_copied` as an empty list; the plan's
   `snippet_policy` says why.
2. **`migration_volume_key` strips `.raw`, `.qcow2` and `.vmdk`** (owner,
   2026-10-13); Python's pattern never does.
3. **The execution checks the plan against the VM again** before the
   shutdown: the node (`VM <vmid> is on <node>, not on <source> as planned;
   generate a new plan`) and the home volume. Python reads the source
   node's config only for the tags and fails later on a moved VM.
4. **The pool**: plan and execution refuse a VM outside the instance's
   pool (gate before phase 5), and the target node's name must be a valid
   node name (it goes into the API path).
5. **The home check** reads `get-fsinfo` and the ext4 superblock instead of
   `findmnt` and `blkid` (decision 5); an unmounted home gives the
   migration message with `mount=''` instead of the failed `findmnt`
   command.
6. **Errors of Proxmox tasks** name the task and its exit status (`Proxmox
   task qmigrate for VM N on NODE failed: ...`), as in phases 3–7; Python
   showed the output of `qm migrate`.
7. **Progress**: a job's progress needs a percentage, so a disk without a
   size in the plan shows only in the task log; the detail is Python's
   description without the leading `  ↳ `.
8. **The restart on the source** is its own job step ("Restart workspace on
   the source node").

## 6. Open questions

- The plan offers no check of free space on the target storage; Proxmox
  fails the task after the shutdown and the VM is started again on the
  source. Python does the same. `GET /qemu/{vmid}/migrate?target=...`
  (mapping B6) lists the local disks and could refuse earlier; not added,
  since it changes Python's behaviour. The owner added phase 17 to
  PLAN.md (2026-10-13): this check, after the whole implementation.
- Phase 9 (`addkey`) is next; it needs the third way of adding keys to the
  home noted in phase 4 (open question 4).
