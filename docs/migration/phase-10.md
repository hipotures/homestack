# Phase 10: `create` on another node (B2)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** B2 `create` with `node` other than Gold's: the workspace is made
on that node and verified there; event `workspace.created`
**Decisions used:** 3 = A (owner, 2026-10-13: a full clone on Gold's node,
then an offline migration to the target node with its disks, through the
API only), 1 = A, 2 = A, and the rules of the gate before phase 5 (only VMs
of the instance's pool)

## 1. Python behaviour (step 3)

`python/src/homestack/create_transfer.py` and `lifecycle.py`
(`build_create_plan`, `create_workspace`, `_revalidate_stream_source`,
`_verify_streamed_restore`, `_describe_restore_residual`).

### Plan

1. `node` absent or Gold's node: `transfer_method` `clone` (phase 5).
   Otherwise `stream`; an empty `node` is refused ("Create target node
   cannot be empty").
2. Gold exists and is on the configured node; the VMID is free; Gold's
   config has the Gold tag and the root disk.
3. `validate_cross_node_create`:
   1. both nodes are in `cluster_node_statuses` ("Configured Gold node 'X'
      is not in the Proxmox cluster", "Target node 'X' is not in the Proxmox
      cluster") and online ("... is offline"), checked before anything else
      on the nodes;
   2. `check_remote_requirements` on both nodes, `vzdump` on Gold's node and
      `bash` on the control node (the SSH transport);
   3. `capture_gold_metadata`: the root disk, at least one Cloud-Init drive
      (`ideN`/`sataN`/`scsiN`/`virtioN`, `cloudinit` volume, `media=cdrom`),
      no other disk and no `unusedN` ("Gold VM N has unsupported extra data
      disks: ..."), no `backup=0` on the root, the Cloud-Init drives, EFI or
      TPM ("... has backup=0; cross-node create requires it to be included
      in the streamed archive"), every `netN` with a bridge, `net0` among
      them; it keeps the root's config, the Cloud-Init slots, the bridges and
      the volumes by slot (no tags);
   4. the storage: `resolve_homestack_storage` for the target node, which
      must list it with `images` and the snippet storage with `snippets`,
      neither disabled nor inactive ("Target node does not provide storage
      'X'", "Target storage 'X' does not support images content", "...
      is disabled or inactive on node");
   5. every Gold bridge on the target node ("Target node 'X' is missing Gold
      network bridge(s): ...").
4. The plan: `node` the target, `source_node` Gold's, `root_storage` and
   `home_storage` the target storage, `source_metadata`.

### Execution

1. `_revalidate_stream_source`: Gold's config read again and captured again;
   any difference: "Gold VM source configuration changed after create
   confirmation; re-run create and review the new plan". Before the steps
   and again before the stream.
2. The VMID must still be free ("VMID N is no longer available on target
   node 'X'; no restore was attempted").
3. `vzdump GOLD --stdout ... | ssh root@TARGET qm create N --archive -
   --storage S --unique 1 --start 0 --template 0 --name NAME`, run from the
   control node. A failure: "Cross-node Gold restore pipeline failed with exit
   code N: ...", then "No target cleanup was attempted because restore
   ownership is uncertain. Residual inspection: ..." (what is left of the
   VM and its volumes); nothing is allocated, configured or started.
4. `_verify_streamed_restore`: the VM is stopped; its root exists and is on
   the target storage; every Cloud-Init slot is a Cloud-Init drive; the
   attached slots are exactly Gold's ("attached disk slots changed: missing
   ...; unexpected ..."); no volume is one of Gold's ("still references source
   volume"); every volume is on the target storage and belongs to the new
   VMID; the root's options are Gold's (`format` aside); the bridges are
   Gold's; the name; not a template; the boot order has the root.
5. Then the steps of the same-node create on the target node: the root
   renamed, the home, tags, snippets, Cloud-Init, boot, verification.

### Tests in scope

| Python test | Rust |
|---|---|
| `StreamCommandTests` (3) | excluded: decision 3 = A, no stream |
| `CrossNodeValidationTests` (3) | ported, on the simulator; the `backup=0` half is dropped (a migration copies every disk) |
| `CreatePlanTests` (2) | ported |
| `RestoreVerificationTests` (4) | ported, on the migrated clone |
| `CreateRestoreFailureTests` (1) | ported: a failed migration allocates no home and starts nothing |

## 2. Design (decision 3 = A)

Planning keeps Python's checks, through the API:

- the node statuses from `GET /nodes`, before any other read on the nodes;
- `capture_gold_metadata` without the `backup=0` rule;
- the target storage and the snippet storage from `GET
  /nodes/{target}/storage` (the token sees the storages it may audit or
  allocate on, which covers both);
- the bridges from `GET /nodes/{target}/network`.

The clone needs room on Gold's node for a while: `clone_storage` is the
first storage of the Gold node's layout. The plan's `transfer_method` is
`clone-migrate`.

Execution, on top of the same-node steps:

1. Gold's config read and captured again, as in Python.
2. `Full clone` on Gold's node into `clone_storage` and the pool.
3. `Migrate to target node`: `POST /nodes/{gold node}/qemu/{vmid}/migrate`
   with `target`, `with-local-disks=1` and `targetstorage`, the `qmigrate`
   task's log feeding the progress (phase 8's tracker and its retry on a
   taken lock).
4. `Verify migrated clone`: Python's `_verify_streamed_restore`, plus the VM
   being on the target node in `cluster/resources`.
5. The home, identity, Cloud-Init, boot and verification on the target
   node, as on Gold's.

A failure before the start destroys the clone wherever it is (it was named
by this job, it is in the pool and it is stopped), as the same-node create
does.

Capabilities (B2): `create` may now allocate on every layout's storages,
and needs `VM.Migrate` on the pool. Role `HomeStack` already holds it
(`migrate`), and the install grants the storages of every layout: no ACL
change.

## 3. Tests

| Kind | Count | Where |
|---|---|---|
| Python tests ported | 10 | `crates/hs-core/tests/create_transfer.rs` (cluster `data/create-transfer-cluster.toml`) |
| Python tests excluded | 3 | `StreamCommandTests`: decision 3 = A |
| New, `hs-core` | 3 | the same file: the whole create on another node (steps, the order clone → migration → home → start, every volume on the target storage, nothing on Gold's node); a failure after the migration removes the clone on the target; a target without Gold's bridge, with its storage disabled, or not in the cluster is refused |
| New, `hs-server` | 1 | `crates/hs-server/tests/lifecycle.rs`: the job's tasks `qmclone` then `qmigrate`, the result, `409` for an offline target |
| Changed | 3 | `same_node_default_plan_uses_clone_only` (another node is no longer refused as unavailable), the capability tests of `hs-core` and `hs-server` (`create` needs `VM.Migrate` and every layout's storages) |
| Live | 1 | `crates/hs-server/tests/live_create_node.rs` in `check.sh --live` |

Manifest: 593 Python tests, 136 ported, 380 todo, 77 excluded.

## 4. Live cluster (L4)

`crates/hs-server/tests/live_create_node.rs` passed on 2026-10-13 (70 s):
`workspace.create` of VM 240 with `node` pve3 through the server; the test
Gold 248 cloned on pve2 into `local-lvm`, the clone migrated to pve3
`local-lvm`, then configured and started there (tasks `qmclone`,
`qmigrate`, three `qmconfig`, `qmstart`; 59 s). The VM ran on pve3 with
`vm-240-disk-0`, `vm-240-cloudinit` and `vm-240-hs-home-user` on pve3's
`local-lvm` and nothing of VMID 240 on pve2; the guest was verified (name,
address, home, keys). Destroyed through the server, nothing left on either
node.

The first run failed in the test itself: the server's read-only token
lists the volumes of deleted VMs only, not those of a VM in the test pool.
The test now reads them with the test token while the VM exists.

Not checked live: pve1 (`worker-roots`) as the target, and a migration that
fails (the simulator covers it).

## 5. Deliberate differences from Python

1. **Clone and migration instead of a stream** (decision 3 = A): two
   copies of the root and room for one on Gold's node for a while, no SSH,
   no `vzdump`. `StreamCommandTests` and the SSH checks of the nodes
   (`check_remote_requirements`, `vzdump`, `bash`) have no counterpart.
2. **No `backup=0` rule**: it guarded what `vzdump` leaves out of an
   archive; a migration copies every disk.
3. **A failed migration removes the clone.** Python left a failed restore
   alone, since it could not tell whose it was. The clone is this job's (its
   name, the pool, stopped), and Proxmox leaves it on Gold's node when the
   migration fails.
4. **The VM is on the target node**: the check of the migrated clone also
   reads `cluster/resources`, before its power state.
5. **Messages of the check** name the migrated clone ("Migrated Gold clone
   VM N ...") instead of Python's "Streamed Gold restore ..."; the details
   are Python's. A clone that still uses one of Gold's volumes is reported
   before a volume on the wrong storage (Python reported the first slot in
   config order).
6. **Refusals**: the node, storage and bridge checks of the target are
   `409` (refused); an empty `node` is `422`, as is a target node without a
   storage layout. Python had one `AppError` for all.
7. **The clone storage**: the first storage of Gold's node's layout; the
   requested `storage` is the target's.

## 6. Open questions

None.
