# Phase 6: `refresh` and recovery (B3, B4, E3)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** B3 `refresh`, B4 recovery of an interrupted `refresh`, E3 the
`homestack-lock` tag; event `workspace.refreshed`
**Decisions used:** 1 = A (static vendor snippets, here `keep`), 4 = B (the
journal in the VM's `description`, owner's rules 1–5 and `force`), 5
(verification at `Audit` + `FileRead`), the owner's decision of 2026-10-13
on unlinking without `digest`, and the rules of the gate before phase 5
(only VMs of the instance's pool)

## 0. The opening probe

`scripts/live-probe-import-from.sh` (test token, VM 240 cloned from the test
Gold 248 into `homestack-test`, destroyed afterwards) tried both ways to put
a copy of Gold's root next to a workspace; result in `decisions.md`, "Probe
results". Both work. The server uses **variant A**: `import-from` into a
free slot while the workspace runs, then `PUT /unlink` without `force`,
which leaves the copy as `unusedN`. That keeps Python's order (the copy is
made while the workspace runs, then the same journal phases) and the same
short downtime; variant B would change both (PLAN.md 1.2).

The probe also showed that `PUT /unlink` has no `digest`, so Python's
`qm disk unlink ... --digest` (`lifecycle._unlink_disk`, `_digest_args`)
cannot succeed on PVE 9.2: Python's `refresh` fails at "Switch root disks",
its automatic recovery fails deleting the staged root the same way, and the
journal and the staged copy stay. Python's tests replace `_unlink_disk` with
a fake and do not see it. The owner decided (2026-10-13) that the server
unlinks without `digest`; Python is left as it is.

## 1. Python behaviour (step 3)

All in `python/src/homestack/lifecycle.py` unless named otherwise.

### Lock and role tags (E3, `proxmox.py`)

- `require_refresh_unlocked`: `homestack-alien` → "Refresh of VM N is
  blocked for alien workspace role tag 'homestack-alien'."; the exact tag
  `homestack-lock` (not `homestack-lock-extra`) → "Refresh of VM N is
  blocked by tag 'homestack-lock'. Remove this tag from the VM in Proxmox
  before refreshing."
- `verify_workspace_role_tags`: `homestack-ws` required, `homestack-gold`
  forbidden, nothing else but `homestack-lock` allowed.

### Plan (`build_refresh_plan`)

1. The VM exists and has a node (`cluster_vm_resource`).
2. `require_refresh_unlocked` on its config, **before the journal is read**.
3. A journal (`snippet_dir/homestack-vm<id>-refresh.json`, `_read_refresh_journal`)
   is validated (`_validate_refresh_journal`, below); if there is one, it
   must name the VM's node, and the plan is `mode="recover"` with `name`,
   `node`, `status`, `home_label`, `home_volume`, `recovery_phase` and the
   journal, **without** resolving the workspace again.
4. Otherwise `check_remote_requirements`, `resolve_existing_workspace(...,
   require_network=True)` (role tags, name, root and home disks, the home's
   serial, `ipconfig0` with a static address and gateway); the VM must be on
   Gold's node ("Migrate it back first"); the root's storage must be a
   HomeStack storage of the node; Gold exists on the configured node, has
   `homestack-gold` and no `homestack-alien`, and a root disk.
5. `mode="replace"` plan: VMID, name, node, status, IP, CIDR, gateway, Gold,
   root storage and slot, root size, Gold's root config and volume, home
   slot, storage, label and volume, and the two policies ("preserve
   persistent disk exactly; never format during refresh", "verify by boot
   and restore pre-refresh power state").

### Journal (`_validate_refresh_journal`)

`version` 1; `transaction_id` 32 lowercase hex; `phase` one of `prepared`,
`imported`, `switching`, `switched`, `verifying`, `verified`;
`initial_status` `running` or `stopped`; `node` a node name; `root_disk` and
`home_disk` equal to the configured slots, and different; `new_unused_key`
`unusedN`; the old root volume has a storage and is the volume of
`old_root_spec`; the home volume has a storage and differs from the old
root; `home_label` is `HS_HOME_<vmid>`; a staged root, when set, has a
storage and is neither the old root nor the home ("staged root identity");
every phase after `prepared` has a staged root; `old_unused_key`, when set,
is `unusedN`.

### Execution (`refresh_workspace`, `mode="replace"`)

1. `initial_status` must be `running` or `stopped`.
2. Resolve again; `require_refresh_unlocked`; the power state and the home
   volume must equal the plan's; the boot order must contain the root slot;
   `net0` gives the MAC; a free `unusedN` is chosen.
3. Journal `prepared` (old root spec and volume, home volume and label,
   `boot`, `ipconfig0`, `cicustom`), **before** anything changes.
4. `pvesm path` of Gold's root, `qm disk import VMID PATH STORAGE
   --target-disk unusedN`; read the config; home identity (volume and
   serial) unchanged; lock checked again; the new volume must be in
   `unusedN` and differ from the old root and the home. Journal `imported`
   with the new volume.
5. Shut down if running (`shutdown_vm_on_node`, never forced).
6. Read the config; lock; home identity; root still the old volume. Journal
   `switching`; `qm disk unlink --idlist <root>` (old root → `unusedM`);
   journal records `old_unused_key`; `qm set --<root> <new>,<Gold's
   iothread, discard, ssd, cache, aio, backup, replicate, ro>`; the new root
   is attached and not also unused; home identity. Journal `switched`.
7. Snippets with `preserve_home=True` (the vendor script refuses to format a
   blank home); `qm set --boot <old boot> --ipconfig0 ... --cicustom ...`;
   role tags verified; `qm cloudinit update`.
8. Journal `verifying`; `qm start`; `_verify_refreshed_guest` (agent,
   cloud-init, hostname, address, home ext4 with the label, the account, the
   user's and root's keys present, one regular account, no `sudo`,
   `cloud-id`).
9. Stop again if the VM was stopped before.
10. Journal `verified`; home identity; delete the old root (`unlink --force`
    of its `unusedM`, which must exist); remove the journal.
11. The final power state must equal the initial one; locally
    `known_hosts`. Result: the plan plus `status`, `mac`, `home_preserved`,
    `guest_verified`, `power_state_preserved`, `cloud_id`,
    `rollback_performed: false`, `cleanup_pending: false`, SSH commands.
12. On any error after the journal was written: `_recover_refresh`; the
    message is "Refresh failed: X. Automatic recovery succeeded: Y." or
    "... Automatic recovery also failed: Z. Transaction journal retained at
    PATH." Before the journal, the error goes out as it is.

### Recovery (`_recover_refresh`, also `mode="recover"`)

1. Validate the journal; shut the VM down if it runs; home identity.
2. A staged root missing from the journal is taken from `new_unused_key` if
   that slot holds a volume other than the old root and the home.
3. `verified`: the root must be the new volume; delete the old root if it is
   still unused; start if it was running; remove the journal; "completed
   verified cleanup".
4. Otherwise roll back: if the new volume is the root, unlink it (to
   unused); the root must now be empty or the old volume, else "unexpected
   root volume"; if empty, re-attach the old root from `old_root_spec` (it
   must be unused); the old root attached and not also unused; home
   identity; delete the new volume (its unused reference must exist);
   restore `boot`, `ipconfig0`, `cicustom`; `qm cloudinit update`; start if
   it was running; remove the journal; "rolled back to original root".
5. `mode="recover"` checks the lock first (`test_direct_recovery_rejects_lock_before_recovering`).

## 2. On the API

| Python | Server |
|---|---|
| journal file on the node | a block `<!-- homestack-journal: <base64 JSON> -->` in the VM's `description`, written with `PUT config` and the `digest` of the config the step was checked against (decision 4) |
| `pvesm path` + `qm disk import ... --target-disk unusedN` | `POST config` `scsiK=<storage>:0,import-from=<Gold's root>` into a free slot (highest free `scsiK`, recorded as `staging_slot`), then `PUT /unlink idlist=scsiK` → `unusedN` |
| `qm disk unlink --idlist X [--force 1] --digest D` | `PUT /unlink idlist=X [force=1]`, no `digest` (owner's decision); the config is read and checked right before |
| `qm set --<root> ...`, `qm set --boot ...` | `POST config` (task) with `digest` |
| snippets `preserve_home=True`, `qm cloudinit update` | `cloud_init_settings(.., HomeFormat::Keep, ..)`: `ipconfig0`, `nameserver`, `ciuser`, `ciupgrade`, `cicustom vendor=...keep.yaml`; the keys stay as they are; Proxmox writes the Cloud-Init drive at start |
| `qm shutdown`, `qm start` | `shutdown_vm`, `start_vm` (phase 3) |
| `_verify_refreshed_guest` | `verify_refreshed_guest` (phase 4) |

## 3. Server rules from the decisions

- **Pool:** `refresh` refuses a VM outside the instance's pool before the
  plan and again before the execution (`outside_pool`).
- **Journal (decision 4):**
  1. written before each step it describes, with the `digest` of the config
     that step was checked against, so a change made by someone else in
     between fails the write and nothing happens;
  2. removed only after the result is verified (`verified` and the old root
     deleted, or the rollback finished);
  3. a later `refresh` finds it (`mode="recover"`) and compares every
     recorded stage with the VM's config before doing anything;
  4. every mismatch is listed (in the plan and in the error), and nothing is
     changed while there is one;
  5. `force` (`{"vmid": N, "force": true}`, only with a journal): when the
     state disagrees, attach the staged new root anyway, if the journal
     names it and the VM still holds it, and keep every other volume as
     `unusedN`; never delete a volume. The home must still be the journal's;
     the VM is booted and verified and its power state restored, and the
     journal is removed. Without a staged root `force` is refused.
- **What agrees with each phase** (the root slot, the staged volume, the old
  root):

  | Phase | Root slot | Staged volume | Old root |
  |---|---|---|---|
  | `prepared` | old | not yet, or in the staging slot (journal not updated) | root |
  | `imported` | old | staging slot or `unusedN` | root |
  | `switching` | old, empty, or new | `unusedN`, or root | root, or `unusedN` |
  | `switched`, `verifying` | new | root | `unusedN` |
  | `verified` | new | root | `unusedN`, or gone |

  Plus, always: the home slot holds the journal's volume with serial
  `HS_HOME_<vmid>`, no other slot holds the old root, the staged volume or
  the home, and the VM is on the journal's node.
- **Unlink without `digest`:** right before each `PUT /unlink` the config is
  read and the slot must hold the expected volume; after it the volume must
  be where Proxmox puts it (`unusedN`, or gone with `force`).
- **Rollback restores** every option `refresh` sets (`boot`, `ipconfig0`,
  `nameserver`, `ciuser`, `ciupgrade`, `cicustom`): the journal keeps their
  old values, an option that was not set is deleted again.

## 4. Differences from Python

- The journal is in the VM's `description`, not a file on the node
  (decision 4); a journal Python left on a node cannot be seen by the
  server. `destroy` already names that file in `node_files_left`.
- The copy of Gold's root goes through a staging slot (`import-from` cannot
  target `unusedN`); the journal records it as `staging_slot`, and
  `new_unused_key` is known only after the unlink.
- Unlink without `digest` (owner's decision; Python's call cannot succeed).
- Rule 4 of decision 4: recovery checks the whole state before changing
  anything, also before the shutdown, and lists every mismatch; Python
  stops at the first mismatch, possibly after shutting the VM down.
- `force` is new (decision 4, rule 5).
- No per-VM snippets and no `qm cloudinit update` (decision 1 = A); the
  rollback restores all Cloud-Init options `refresh` set, not just
  `ipconfig0` and `cicustom`.
- No `check_remote_requirements`, no `pvesm path` (a path works only for
  `root@pam`).
- The result names the event `workspace.refreshed` and
  `host_key_changed: true`; the local `known_hosts` step is the client's.
