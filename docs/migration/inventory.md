# Inventory: what is not implemented, on purpose or by accident

Date: 2026-10-14, after phase 13c step 1 (`homestack install` and `uninstall`).
The TUI (phase 13b and the 49 TUI tests) is left out, as asked.

Sources: `parity/manifest.toml` (all 593 Python tests), `api-mapping.md`
(functions A1–I2 and the open decisions of section 10), `PLAN.md`, the
"differences" and "open questions" of every phase report, `decisions.md`,
and a scan of the code. Nothing here was changed by the inventory.

## 1. Where the 593 Python tests stand

| Status | Count | Meaning |
|---|---|---|
| ported | 205 | a Rust test replaces it |
| excluded | 79 | with a reason in the manifest (section 3); 49 of them are TUI |
| todo | 309 | all in phases that have not started (section 2), plus 4 unclassified (section 4.1) |

## 2. Planned and not started (in the plan, nothing forgotten)

| Phase | What | Python tests waiting |
|---|---|---|
| 13c steps 2–3 | deployment of the server instances from `install`; the configuration wizard (F1: discovery, choice of Gold/storage/networks/FIDO keys, draft with stages and resume, writing `client.toml` and `server.toml`) | 29 (`test_install.py` 26, `InstallDraftTests` 3) |
| 13c gate | `--live`: install over SSH and over Herdr on a clean cluster, deployment, `homestack status`, uninstall (the owner runs the install part) | – |
| 14a | `setup` core: list, status, selection, state, alien workspaces, startup, Files and Credentials (G1–G3, G8) | 76 |
| 14b | Environment (G4); no separate Python tests, they sit in 14a/14c | – |
| 14c | Applications and structured config: documents, config files, app files (G5) | 55 |
| 14d | BK backup and retrieval keys (G6), including the cleanup of retrieval keys at `destroy`, which today is only listed as `backup_keys_left` | 94 |
| 14e | Herdr setup (G7) | 23 |
| 14f | `repo` and repository setup (H1–H3) | 28 |
| 15 | Python switched off, golden files instead of comparisons, README for Rust only | – |
| 16 | `resize` on LUKS, LVM, Btrfs and other layouts, live | – |
| 17 | `migrate` checks free space before shutting the VM down | – |

Also waiting for phase 14: the SSH master of `setup` and `repo`
(`master.rs`) connects by `user@ip` without the workspace's host config, so it
must be given the workspace's own `known_hosts` file (`decisions.md`).

## 3. Left out on purpose (decided, with the source)

- **Herdr as a transport to Proxmox** (12 tests): the REST API replaces it
  (mapping T1). Herdr remains only as one of two ways to reach root for
  `install`/`uninstall`, and in phase 14e.
- **SSH routing between nodes** (3): the API proxies `/nodes/{node}`.
- **Cross-node create by `vzdump` stream** (3 tests, `check_remote_requirements`):
  decision 3 = A, clone and migration (phase 10).
- **Root rename** (3 + 1): decision 2 = A, the root keeps Proxmox's name.
- **Per-VM Cloud-Init snippets, their stale-snippet handling and copying**
  (2 + 2 tests, C2, C3): decision 1 = A, two static vendor snippets.
- **Passphrase of `resize` over SSH** (1 test): decision 6, it goes to the guest
  agent of the VM named by VMID.
- **`cryptsetup` tests** (1): tests the tool, skipped in Python too.
- **`known_hosts` handling by `ssh-keygen`** (2 tests): replaced by a file per
  workspace (option C, decided 2026-10-13).
- **Authentication by bearer token only** (decision 8): no hardware-key
  signature for destructive operations (mapping question 8).
- **No start-up probe that disables modules** (phase 1, decision 9);
  capabilities are computed per request.
- **Phase 12 dropped**: no Python client through the server.
- **`homestack-pve.sh` modes `status` and `--dry-run`**: not carried to
  `homestack install` (a plan without `-y` is the dry run).
- **Python's strictness bugs** (phase 1, section 1) and `qm disk unlink --digest`
  (phase 6): the owner chose not to fix them in Python.
- **Mapping section 9** (domain per VM, Pangolin, Vaultwarden, DNS provider):
  planned features outside the migration; nothing started.

## 4. Not implemented and not decided (found by the inventory)

1. **Four phase-10 tests are still `todo`** (`test_create_execution.py`:
   `test_stream_command_preserves_binary_payload_and_pipefail_failures`,
   `test_stream_failure_does_not_allocate_home_boot_or_destroy_unknown_restore`,
   `test_stream_success_precedes_target_home_allocation_and_failure_destroys_target`,
   `test_invalid_restored_disk_ownership_prevents_destroy`). The phase 10
   report excludes only the 3 `StreamCommandTests`; these 4 were never
   classified. The first three are about the stream (excluded by decision 3);
   the fourth has its behaviour in `create_transfer.rs` (ownership check) but
   no test under that name. Bookkeeping, no missing behaviour known.
2. **The server certificate is renewed by hand** (`homestack-deploy cert --push`,
   renewal when under 30 days remain). Nothing runs it. A certificate that
   expires stops the clients (decision 8 refuses non-TLS).
3. **A VM made by `create` keeps the create snippet** (`allow_format=1`), as in
   Python. Phase 4 proposed switching to the keep snippet after the create is
   verified (open question 3); it was never decided. `addkey` and `refresh`
   switch to keep.
4. **Leftovers of workspaces made by Python**: their four per-VM snippets and a
   refresh journal file stay on the nodes. `destroy` lists them
   (`node_files_left`); nothing removes them and `refresh` does not look for a
   journal file.
5. **Gold as an LXC container** (owner's remark of 2026-10-13): untested,
   nothing supports it. An Alpine-based image will not work.
6. **Only checked on the simulator, not live**: LUKS, LVM and Btrfs
   (phase 16); a migration that fails after the shutdown; migration to pve1
   with `worker-roots`; the HTTP 595 of an offline node; the whole
   `hs-install` on a real cluster; Herdr and SSH root accesses against real
   nodes; a single-node Proxmox (its `/cluster/status` may not carry `ip`).
7. **The `--live` gates cannot run now**: the cluster has no HomeStack
   installation (tokens, pools and roles were removed). They need the owner's
   install first.
8. **Leftovers outside the repository**: DNS records `api1`–`api3.hs.efez.net`;
   stale token files in `~/.config/homestack/`; on pve2 in `/root`:
   `homestack-pve.sh`, `homestack-pve-token-deploy`, `homestack-pve-token-test`
   (not rechecked since the cleanup).
9. **README and docs still describe Python** for the commands not yet in
   Rust; phase 15 rewrites them.
10. **`homestack install` is only the Proxmox part**: it does not deploy the
    server or write the configuration yet (13c steps 2–3), so an install is
    followed today by `homestack-deploy` and `hs-config split` by hand.

## 5. By function (api-mapping ID)

| IDs | State |
|---|---|
| A1–A8 reads (status, transport, discover, networks, storage, orphans) | done, server and client |
| B1–B7 create, create on another node, refresh, recovery, resize, migrate, destroy | done; B2 by clone and migration; resize layouts and migrate free space are phases 16 and 17 |
| C1–C5 Cloud-Init and keys | done by decision 1 = A (C2 and C3 not needed); item 4.3 and 4.4 above |
| D1–D3, E1–E3 guest verification, Gold, tags | done |
| F1 installer | root part done (13c step 1); deployment and wizard not done |
| F2–F3 config validation and upload | done |
| G1–G8 `setup` | not started (phase 14) |
| H1–H3 `repo` | not started (phase 14f) |
| I1–I2 local SSH files, FIDO, cues | done; `known_hosts` changed to a file per workspace |
