# Phase 5: `create` and `destroy`

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** B1 (same node as Gold), B7, server part; events
`workspace.created`, `workspace.destroyed`
**Decisions used:** 1 = A (native Cloud-Init, static vendor snippets),
2 = A (the root keeps Proxmox's name), 5 (guest reads at `Audit` +
`FileRead`), 9 (Gold in `homestack-gold`), 10 = C

**Gate before this phase (PLAN.md):** not met. Pool `homestack` does not
exist and the server token still has `VM.Config.Disk` on `/vms` (read
through the API on 2026-10-13). The owner asked twice to go on, so the
phase is built and tested on the simulator; nothing on the cluster is
changed until the owner runs the production install and says whether the
live gate may clone Gold 101 into the test pool (the owner forbade touching
their VMs).

## 1. Python behaviour (step 3)

### Plan (`lifecycle.build_create_plan`)

1. `validate_name`: `[A-Za-z][A-Za-z0-9.-]{0,62}`.
2. `derive_ip`: VMID 2–254, `<prefix>.<vmid>`.
3. The target node is `[node] name`; another node is the stream transfer
   (B2, phase 10). `check_remote_requirements` on the node (Python's tools;
   no counterpart over the API).
4. Gold exists, on the configured node; the VMID is free.
5. Gold's config has `homestack-gold` and no `homestack-alien`
   (`require_gold_tag`) and a root disk; its size.
6. Storage: `resolve_homestack_storage(cfg, node, requested)`.
7. Home size `parse_home_size`: `<n>G` or `<n>T`, upper-cased, in GiB.
8. Label `HS_HOME_<vmid>`; keys from Gold (C5) with their labels; stale
   snippets of the name (C2).

### Execution (`create_workspace`)

1. Remove stale snippets; read the keys again.
2. `qm clone GOLD VMID --name NAME --full 1 --storage S`; the VM is owned
   from here.
3. Rename the root to `vm-<id>-hs-root-default` (`perl
   PVE::Storage::rename_volume`).
4. `pvesm alloc S VMID vm-<id>-hs-home-<user> <n>G --format raw`, then
   `qm set --<home> VOL,discard=on,iothread=1,ssd=1,serial=HS_HOME_<id>`; if
   that fails, `pvesm free VOL`.
5. `qm set --tags homestack-ws`, check the tags (exactly `homestack-ws`,
   optionally `homestack-lock`, never `homestack-gold`); `net0` gives the
   MAC; the home's serial is checked.
6. Four snippets per VM; `qm set --ipconfig0`, `--cicustom`, `qm cloudinit
   update`; tags checked again.
7. `qm start`; from here nothing is cleaned up.
8. Verification (D1, D2): agent, cloud-init done, hostname, address on
   `eth0`, home mounted as ext4 with the label, the account, the user's keys
   equal to Gold's by identity, root's keys, one regular account, no `sudo`,
   `cloud-id`.
9. Locally: `known_hosts`, SSH config.
10. On any error before the start: `qm destroy VMID --purge 1` if the VM was
    made, and the snippets written are removed; cleanup errors are added to
    the message.

### Destroy (`build_destroy_plan`, `destroy_workspace`)

- Plan: `resolve_existing_workspace` (alien and untagged VMs refused, the
  home's serial and label), capacity of the root and home storages, home
  usage and risk.
- Execution: resolve again (an alien VM is refused before anything,
  `test_destroy_execution_rejects_alien_before_shutdown`), `qm shutdown`
  (never forced), resolve again and compare the home label, `qm destroy
  --purge 1`, check the VM is gone, remove the four snippets and the refresh
  journal of the VM on every node, then the local SSH files and BK keys.

## 2. On the API

| Python | Server |
|---|---|
| `qm clone ... --full 1 --storage S` | `POST /nodes/{n}/qemu/{gold}/clone` `newid`, `name`, `full=1`, `storage`, `pool` |
| root rename | none (decision 2 = A) |
| `pvesm alloc` | `POST /nodes/{n}/storage/{s}/content` `vmid`, `filename`, `size`, `format=raw` |
| `pvesm free` | `DELETE /nodes/{n}/storage/{s}/content/{volid}` |
| `qm set` | `POST /nodes/{n}/qemu/{vmid}/config` (task) |
| snippets, `qm cloudinit update` | `cloud_init_settings` (phase 4): `ciuser`, `sshkeys`, `ipconfig0`, `nameserver`, `ciupgrade`, `cicustom vendor=...create.yaml`; Proxmox writes the Cloud-Init drive at start |
| `qm start` | `POST .../status/start` |
| verification | `verify_guest` (phase 4) |
| `qm destroy --purge 1` | `DELETE /nodes/{n}/qemu/{vmid}?purge=1` |
| `qm shutdown` | `shutdown_vm` (phase 3) |
| snippets and journal on every node | none: the server writes no per-VM files (decisions 1 and 4); see section 4 |

## 3. Server rules added in this phase

- The new VM is cloned straight into the instance's pool
  (`HOMESTACK_PVE_POOL`). The plan fails when the pool does not exist.
- `destroy` refuses a VM outside the pool before planning, with its own
  error code `outside_pool`, even when the tags are right (gate before
  phase 5). The pool is read from `cluster/resources` (needs `Pool.Audit`).
- Before deleting anything after a failed create, the server checks the VM
  is the one it cloned (name, pool) and that it was never started.
- Results carry what the client needs locally: VMID, name, IP, user,
  `host_key_changed` (create: a new host key at that address), and the
  event (`workspace.created`, `workspace.destroyed`).

## 4. Differences from Python

- No per-VM snippets, so no stale-snippet step and nothing to remove on the
  nodes after a failed create or a destroy. Workspaces made by Python still
  have their four snippets (and maybe a refresh journal) on the nodes; the
  API cannot delete node files, so `destroy` lists them in the result
  (`node_files_left`) for `homestack-pve.sh` or the owner.
- The root keeps Proxmox's name (`vm-<id>-disk-N`).
- No `check_remote_requirements` (it checks Python's tools on the node).
- The local steps (SSH config, `known_hosts`, BK keys) stay in the client.
- B2 (create on another node) is phase 10; the plan refuses `node`.

## 5. Tests (step 4)

- Ported: `test_same_node_default_plan_uses_clone_only`,
  `test_create_name_cannot_start_with_digit`, `test_home_label_and_size`,
  `test_logical_volume_names` (home part), `test_disk_option`,
  `test_boot_order_contains_root_disk`,
  `test_destroy_execution_rejects_alien_before_shutdown`,
  `test_create_result_has_no_sync_command`.
- Moved: the four stream tests of `test_create_execution.py` to phase 10
  (B2), `test_gib_value_is_compact` to phase 13 (client display).
- Excluded (decision 2 = A, no root rename):
  `test_pve_rename_volume_uses_proxmox_storage_layer` and both
  `VolumeRenameCleanupTests`. `test_logical_volume_names` keeps its home
  part; `test_named_raw_volume_allocation_uses_requested_name` is covered
  by the simulator test of the API call.
- New, on the simulator: create and destroy end to end; a failure injected
  after each step of create, checked against Python's cleanup rules (VM
  removed only before the start, nothing else left); `destroy` outside the
  pool and of an alien VM refused before any change.
- Python cannot run `create` on the simulator: it writes snippet files
  through the node's shell and renames the root with `perl`, neither of
  which is an API call. The cleanup comparison is against Python's rules
  (section 1), not a Python run.

## 6. Gate

`create` → verification → `destroy` of an `hst-*` VM in the test pool,
twice in a row, nothing left (volumes, VMID). Waits for the production
install and the owner's answer about Gold.

## 7. What was built

- `hs-core::lifecycle`: `validate_name`, `derive_ip`, `parse_home_size`,
  `home_volume_name`, `plan_create`, `create_workspace`, `plan_destroy`,
  `destroy_workspace`, `LifecycleTiming`; `CoreError::OutsidePool`.
- `hs-server::lifecycle`: operations `workspace.create` and
  `workspace.destroy`, registered by default; `OpContext` has the pools.
  New error codes `outside_pool` and `operation_refused` (409); planning
  errors of these operations keep their message. `POST
  /v1/plans/{id}/apply` needs `HomeStack-Config-Version` once a
  configuration is stored.
- `pve-sim`: `POST .../clone` (into a pool), `POST .../storage/{s}/content`,
  `DELETE .../content/{volid}`, `POST .../config` (a task, privileges per
  key), `DELETE /nodes/{n}/qemu/{vmid}`, and the first start of a clone with
  a HomeStack vendor snippet boots Gold's guest as native Cloud-Init would.
- `crates/hs-server/tests/live_lifecycle.rs`: the gate, not yet run.

## 8. Results

- Tests: 9 Python tests ported (`parity/manifest.toml`: 63 ported, 463 to
  do, 67 excluded); 14 simulator tests in `hs-core` (create, the cleanup
  after a failure in each of six steps, a failure after the start,
  create/destroy twice, the refusals) and 3 API tests. `scripts/check.sh`:
  all passed.
- Parity with Python: no new scenario (section 5); the existing ones agree.
- Live (2026-10-13, test token, pool `homestack-test`): `live_lifecycle.rs`
  passed: create, verification and destroy of VM 240 (`hst-cycle1`, then
  `hst-cycle2`, 1 GiB home), twice; after each destroy no VM 240, no
  `vm-240-*` volume (read with the server's read-only token) and no pool
  entry. Now part of `scripts/check.sh --live`. The gate before the phase
  is still not met: no pool `homestack`, the server token still has
  `VM.Config.Disk` on `/vms`.

## 9. Open questions

- The production `install` (pool `homestack`, narrowed server token) must
  be run by the owner on a node.
- May the live gates clone Gold 101 into the test pool (VMIDs 240–249)?
  `check.sh --live` also does so today (`live_cloudinit.rs`).
- `destroy` leaves the node files of workspaces Python made
  (`node_files_left`); `homestack-pve.sh` could remove them.
- `create` still lists `VM.GuestAgent.Unrestricted` in its capabilities,
  though the verification uses `Audit` + `FileRead` (decision 5).
