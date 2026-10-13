# Phase 9: `addkey` (C4)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** C4 `addkey`: one more SSH public key for a workspace, in
Proxmox's `sshkeys` and in root's and the user's `authorized_keys`; a
restart only on request; event `workspace.key_added`
**Decisions used:** 1 = A (static vendor snippets), 5 (guest reads with
`file-read`), the rules of the gate before phase 5 (only VMs of the
instance's pool), and the owner's decision of 2026-10-13 on `addkey`
(`decisions.md`): the snippets carry a key step, and a running VM also gets
the key at once through `agent/exec`

## 1. Python behaviour (step 3)

All in `python/src/homestack/addkey.py`.

### Plan (`build_addkey_plan`)

1. `read_public_key(path)`: exactly one key ("`<path>` must contain exactly
   one public key, found N"), with a comment ("`<path>` has no comment; give
   the key a name, for example ssh-keygen -C trezor-vm"); its private key
   must lie next to it (`private_key_for`).
2. `resolve_existing_workspace(require_network=True)`: the usual checks plus
   a static `ipconfig0` (the address goes to the local SSH config).
3. `cicustom` must name the VM's own vendor and meta snippets
   (`homestack-<name>-vendor.yaml`, `-meta.yaml`): "VM N does not use the
   HomeStack vendor snippet ...; refusing to add a key".
4. `merge_sshkeys`: the key is added to the unquoted `sshkeys` unless a line
   has the same blob (`key_already_in_sshkeys`). Without `--key` the
   existing `sshkeys` are applied; none: "VM N has no keys in its Proxmox
   sshkeys; give one with --key".
5. The steps (9 with a key, 7 without): sshkeys, `allow_format=0`, the
   ensure-keys step, a new `instance-id`, `qm cloudinit update` (and "the VM
   is NOT restarted here" or "stays stopped"), the known_hosts and the check
   only after a restart, and the two local steps.

### Execution (`add_key`)

1. Read the vendor and meta snippets (`cat` on the node).
2. `qm set --sshkeys <temporary file>` unless the key was there.
3. The vendor snippet rewritten: `allow_format=0` (a rerun must not rewrite
   `authorized_keys` or the home), every old ensure-keys step dropped, one
   new step appended (`ensure_keys_item`): for `/root` and `/home/<user>`,
   create `.ssh` (700) and `authorized_keys` (600) with the directory's
   owner, add a final newline, append each key whose blob `grep -qF` does
   not find; nothing removed.
4. A new meta snippet with a random `instance-id`, then `qm cloudinit
   update`. A failure in 2–4 writes the old snippets back (`sshkeys` keeps
   the key: "harmless on its own").
5. A running VM is restarted only when asked (`--restart`, or yes to the
   question asked last; `--yes` does not restart): shutdown, `qm start`, the
   agent, `cloud-init status --wait` must say `status: done` ("Cloud-Init on
   VM N did not report 'done' after the restart: ..."), then every key's
   blob in both files ("Not all keys are in root and <user> authorized_keys
   of VM N: <comment> in <file>; ..."). A stopped VM is never started.
6. Locally, only after a restart: the VM's address leaves `known_hosts`.
   With a key: its private key joins `identity_files` and the VM's SSH
   config is written again; a local failure becomes `local_error`.
7. The result: `ok`, `vmid`, `name`, `key` (the comment, or every comment),
   `restarted`, `pending_restart` (running and not restarted),
   `known_hosts_removed`, `verified`, `local`.

### Tests in scope

| Python test | Rust |
|---|---|
| `KeyTests` (2) | ported (the key as text; the file is the client's) |
| `VendorTests`, the ensure step (3) | ported, on the step the agent runs |
| `VendorTests`, the vendor rewrite (2) | excluded: no per-VM vendor snippet under decision 1 = A |
| `FlowTests` (11) | ported, on the simulator |
| `LocalTests` (6) | phase 13: `identity_files`, the SSH config and `known_hosts` are the client's (I1) |

## 2. What was ported (C4)

| Python | Rust |
|---|---|
| `read_public_key` | `hs_core::addkey::read_public_key` on the key's text: one key, with a comment |
| `merge_sshkeys` | `merge_sshkeys` |
| `ensure_keys_item` | `ensure_keys_script`: the same shell, run through `agent/exec`; the same loop (`ENSURE_LOOP`) is in both vendor snippets, over `/root` and the mounted home, with the keys of user-data |
| `update_vendor`, `new_meta`, `qm cloudinit update` | none: `cicustom` is set to the keep snippet (Python's `allow_format=0`), and a new `sshkeys` is a new instance (Proxmox's instance id is a hash of user-data and network-config) |
| `build_addkey_plan` | `plan_addkey`: the pool first, `resolve_existing_workspace`, a static `ipconfig0`, `cicustom` one of the two HomeStack snippets, the keys, the steps |
| `add_key` | `add_key`: tags, pool, node and power state checked again; `POST config` with `sshkeys` and/or `cicustom` (with `digest`) and both read back; on a running VM the key step through `agent/exec` and both files checked with `file-read`; on request shutdown, start, the agent, `/run/cloud-init/result.json` without errors, both files again |
| `register_identity`, `sync_local_ssh`, `forget_local_ssh_host` | the client (phase 13); the result gives `ip`, `restarted` and `new_instance_at_next_start` |
| CLI `addkey VMID [--key FILE] [--restart]` | operation `workspace.addkey` with `vmid`, `key` (optional, the text) and `restart` (optional, false); event `workspace.key_added` |

The vendor snippets (`crates/hs-core/src/cloudinit_vendor.yaml`, written to
`deploy/cloud-init/` and embedded in `deploy/pve/homestack-pve.sh`) read the
keys of user-data whatever the format flag (the create snippet still stops
with exit 76 without keys) and end with the key step after the home is
mounted. The owner's install on the nodes was run again on 2026-10-13
(`install --test --gold 248`): it rewrote only the two snippets on pve1,
pve2 and pve3.

`pve-sim` gained the key step of `agent/exec` (the keys of the `HS_KEYS`
heredoc appended in each directory of the `for dir in` line), and a start
whose Cloud-Init instance (the options Proxmox hashes) differs from the
last one: root gets the keys it lacks, the create snippet writes `sshkeys`
into the mounted home again, and the key step of both snippets appends what
root and the home lack. The first boot of a clone or a new root runs the
key step too.

The capability table (C4) adds `VM.GuestAgent.FileRead` (the check) and
`Datastore.AllocateSpace` on the snippet storage (`cicustom`). Role
`HomeStack` and `HomeStackStorage` already hold both: no ACL change.

## 3. Tests

| Kind | Count | Where |
|---|---|---|
| Python tests ported | 16 | `crates/hs-core/tests/addkey.rs` |
| Python tests excluded | 2 | the vendor rewrite: decision 1 = A |
| Python tests moved to phase 13 | 6 | `LocalTests`: the client's files |
| New, `hs-core` | 6 | the same file: the snippets carry the same step after the mount, the create snippet switched to keep keeps the user's own key, the create snippet rewrites the home on a new instance (what the switch prevents), a silent agent leaves the restart pending, the pool and the tags first, a changed power state |
| New, `hs-server` | 3 | `crates/hs-server/tests/addkey.rs`: the job's steps, task and event; the restart as a second request; the refusals when planning (422, 409, 404) |
| Live | 1 | `crates/hs-server/tests/live_addkey.rs` in `check.sh --live` |

Manifest: 593 Python tests, 126 ported, 393 todo, 74 excluded.

## 4. Live cluster (L4)

`crates/hs-server/tests/live_addkey.rs` passed on 2026-10-13 (86 s): VM 240
created on pve2 from the test Gold 248 (create snippet, the workspace key
`hst-test@homestack`); a new ed25519 key added to the running VM: `qmconfig`
with `sshkeys` and `cicustom`, then the key step through the agent; the key
in root's and the user's `authorized_keys` at once, the home's earlier keys
kept, `verified`, no restart. A second request with `restart: true`: shutdown,
start, Cloud-Init's result, both keys in both files, and a new SSH host key
(the new `sshkeys` was a new instance). Shut down; a second key added to the
stopped VM (`restarted` false, `verified` false); after a start the key step
of the vendor snippet installed on the node had put it in both files. VM
destroyed.

Seen on PVE 9.2:

- `sshkeys` and `cicustom` set on a running VM go into its config at once:
  `GET /pending` lists them with their new value and no `pending` field.
- `cluster/resources`, where the operations (and Python) read the power
  state from, still said `running` a few seconds after a shutdown. A test
  that shut the VM down and at once planned `addkey`, or `migrate` in the
  full live run, got a plan for a running VM (`migrate` then started the VM
  on the target). Owner, 2026-10-13: the state is still read from
  `cluster/resources`, and `shutdown_vm` returns only once that list also
  says the VM is stopped (up to 30 s; difference 10).
- A shutdown 10–15 s after a start with a new Cloud-Init instance failed in
  three of three runs: `can't lock file '/var/lock/qemu-server/lock-240.conf'
  - got timeout`, with no other task on the node; a second try 5 s later
  worked. In the full `check.sh --live` run that followed, the first try
  worked. The owner sees the same in the Proxmox UI; what holds the lock
  is not known. The server now tries such calls again (difference 11).


## 5. Deliberate differences from Python

1. **No per-VM snippets** (decision 1 = A): the key step is in the two
   static snippets and reads the keys of user-data, so it runs on every new
   Cloud-Init instance, also after `refresh` (a new root): the home then
   gains the keys of `sshkeys` it lacks. Python's step existed only in VMs
   that had gone through `addkey`.
2. **A running VM gets the key at once** (owner, 2026-10-13): the key step
   runs through `agent/exec` and both files are checked. `pending_restart`
   is true only when that could not be done (the agent did not answer;
   `note` says so); Python's was true for every running VM not restarted.
3. **The restart is a second request** (PLAN.md, phase 9): `restart: true`,
   usually without `key`, which applies the keys already in `sshkeys`. On a
   stopped VM it changes nothing, as in Python.
4. **New SSH host keys at the next start.** A new `sshkeys` is a new
   Cloud-Init instance, which regenerates the host keys (`ssh_deletekeys`).
   Python started that instance with the restart it asked about; here a VM
   not restarted by `addkey` gets new host keys at whatever start comes
   next. `new_instance_at_next_start` says so; the client removes the
   address from `known_hosts` when `restarted` is true and warns otherwise.
5. **`cicustom` must be one of the two HomeStack snippets.** A VM of
   Python's (four per-VM snippets) is refused; `refresh` moves it to native
   Cloud-Init first (phase 4, open question 6).
6. **The check reads the files with `file-read`** (decision 5), and
   Cloud-Init's state from `/run/cloud-init/result.json` (no errors) instead
   of `cloud-init status --wait`; Python's messages are kept.
7. **Nothing to put back after a failure.** A failed config change stops
   before the guest and the restart; `sshkeys` and `cicustom` are written in
   one change and read back.
8. **The key is text**, not a path: "The public key must be exactly one
   key, found N" and "The public key has no comment; ..." name no file. The
   private key next to it is the client's check.
9. **Steps**: Python's plan listed the snippet edits and the local steps;
   the server's plan lists the config change, the agent step, the check and
   the restart, and says when the host keys will change.
10. **A shutdown waits for the cluster list** (owner, 2026-10-13): after
    the VM has stopped, `shutdown_vm` (every operation and the live tests)
    waits until `cluster/resources` lists it as stopped too, up to 30 s.
    Python returned once `qm status` said `stopped`.
11. **A call refused for a taken lock is tried again** (owner,
    2026-10-13): a task or a call that fails with "can't lock file ... got
    timeout" changed nothing and is made again every 5 seconds, for up to 2
    minutes (`tasks::retry_while_locked`): start, shutdown, the config
    changes, unlink, resize, migrate and destroy. The refused tasks stay in
    the job. Python failed with the error of `qm`.

## 6. Open questions

- The client (phase 13) has to say, when `new_instance_at_next_start` is
  true, that the next start changes the VM's SSH host keys, and remove the
  address from `known_hosts` after a restart.
- Phase 10 (`create` on another node) is next; it needs decision 3 (7.3).
