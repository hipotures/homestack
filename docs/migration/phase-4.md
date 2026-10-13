# Phase 4: Cloud-Init and keys

**Date:** 2026-10-13
**Branch:** `rust-migration`, tag `rust-phase-4`
**Scope:** C1, C2, C5 (C3 is not needed under 7.1 A); mapping section 6 C
**Decisions used:** 1 = A (native Cloud-Init plus a static vendor snippet),
with the owner's choice of 2026-10-13: two static snippets
(`homestack-vendor-create.yaml`, `homestack-vendor-keep.yaml`) instead of a
flag in `smbios1`; 5 (guest reads at `Audit` + `FileRead`)

## 1. Python behaviour (step 3)

### Snippet files (`cloudinit.write_snippets`, `snippet_names`, `remote_write_text`; C1)

1. `mkdir -p <snippet_dir>` on the VM's node, then four files
   `homestack-<name>-{user,vendor,network,meta}.yaml`, each written as
   base64 through the node shell and `chmod 644`.
2. Without `replace`, any of the four already on the node is an error:
   `Cloud-Init snippet already exists: PATH` (create; refresh passes
   `replace=True`).
3. Without `preserve_home` and without keys:
   `Workspace create requires at least one user SSH public key`. Checked
   after the existence check, before anything is written.
4. **user-data:** `users: []`, `disable_root: false`, `ssh_pwauth: false`,
   `ssh_deletekeys: true`, `preserve_hostname: false`, `hostname: NAME`,
   `manage_etc_hosts: true`. No keys: root's come with Gold's root disk.
5. **vendor-data:** one `runcmd` script, `set -eu`, with the VM's
   `expected_serial` = `expected_label` = `HS_HOME_<vmid>`,
   `home_path=/home/<cfg.user.name>`, `allow_format` = `1` (create) or `0`
   (`preserve_home`: refresh, addkey):
   - exactly one disk with that serial (`lsblk -dn -o PATH,SERIAL`), else
     exit 70;
   - ext4 with the expected label: keep; blank (no type, no label): exit 71
     unless `allow_format=1` (`persistent home is blank during refresh;
     refusing mkfs`), exit 72 if it has children, exit 73 if `wipefs -n`
     finds signatures, else `mkfs.ext4 -m 0 -L LABEL`; anything else: exit 74;
   - disables and removes a systemd mount unit for the home path, replaces
     the home line of `/etc/fstab` with `LABEL=... ext4 defaults 0 2`,
     `daemon-reload`, mounts unless already a mount point;
   - only with `allow_format=1` (whether or not it formatted): removes
     `lost+found`, `chown cfg.uid:cfg.gid`, `chmod 700`, creates `.ssh`
     (700) and writes the keys given (base64 in the script) to
     `.ssh/authorized_keys` (600);
   - `loginctl enable-linger USER` when the home has a Herdr user service.
6. **network-config:** v2, one ethernet matched by the VM's MAC (lower
   case), `set-name: eth0`, `renderer: NetworkManager`, no DHCP, `IP/CIDR`,
   default route via the gateway, DNS from `[network] dns`. Refresh passes
   the VM's current CIDR and gateway (from `ipconfig0`).
7. **meta-data:** `instance-id: NAME-VMID-<12 random hex>` (a new instance on
   every write) and `local-hostname`.
8. `cicustom_value`: `user=`, `vendor=`, `network=`, `meta=` pointing at
   `<snippet_storage>:snippets/<file>`. Create sets `ipconfig0` and
   `cicustom` with `qm set`, then `qm cloudinit update`, then starts.
   Create keeps the `sshkeys` cloned from Gold; the keys in the home come
   from `get_workspace_authorized_keys` (possibly the guest fallback).

Tests: `test_proxmox.py::HomeDiskTests::test_create_snippet_formats_only_blank_new_home`
and `::test_refresh_snippet_refuses_blank_home` (phase 4 in the manifest)
check markers of the vendor script and of the user-data.

### Stale snippets (`stale_create_snippets`, `snippet_references`, `remove_stale_create_snippets`; C2)

- Create's plan looks for the four file names of the new name on the target
  node (`test -e`). If any exists, every QEMU VM's `cicustom` in the cluster
  is scanned for `<storage>:snippets/<file>`; a reference is an error
  (`... are referenced by VMIDs [..]; refusing to overwrite them.`), no
  reference makes them stale and the plan lists them.
- Before writing, create checks that the stale set is still the planned one
  (`Stale Cloud-Init snippet state changed after confirmation; ...`), then
  `rm -f` each and fails if it is still there.
- Destroy removes the VM's four files (phase 5).

### Copy to another node (`sync_snippets_to_node`; C3)

- Migrate relays the four files through the control node (`ssh`/`scp`),
  checking they are non-empty at the source and at the target (phase 8).

### Workspace keys (`workspace_ssh.get_workspace_authorized_keys`, `validate_authorized_keys`, `parse_authorized_key_records`; C5)

1. Gold's cluster resource; none: `Gold VM N does not exist`; no node:
   `Gold VM N has no node in cluster inventory`.
2. Gold's `sshkeys` (URL-decoded) on Gold's node. Usable (see 5) → the keys,
   source `Gold VM N:qm sshkeys`. Unusable keys are ignored, not an error.
3. Gold not `running`: `Gold VM N is stopped and its Proxmox sshkeys setting
   does not contain usable public keys.`
4. `qm guest cmd N ping` on Gold's node fails: `Gold VM N QEMU Guest Agent
   is not available`.
5. For `/home/<user>/.ssh/authorized_keys`, then `/root/.ssh/authorized_keys`:
   `test -s F && cat F` through the agent; a non-zero exit or unusable
   content moves on; usable → source `Gold VM N:F`. None:
   `Gold VM N has no usable public keys in its Proxmox sshkeys setting,
   /home/<user>/.ssh/authorized_keys, or /root/.ssh/authorized_keys`.
6. `validate_authorized_keys`: blank lines and `#` comments skipped, each
   other line stripped and must start with `ssh-`, `ecdsa-` or `sk-`
   (`Unexpected line in authorized_keys source: '<80 chars>'`); none: `No
   SSH public keys found for workspace user`. Returns the lines joined by
   `\n` with a final `\n`.
7. `parse_authorized_key_records`: per validated line `type`, `blob`,
   `comment` (third field, stripped), `label` (comment, else `type
   <12 chars of blob>…`), `identity` (`type blob`); fewer than two fields:
   `Invalid authorized_keys line: '<80 chars>'`.

Test: `test_workspace_ssh.py::GoldKeyRoutingTests::test_guest_key_fallback_runs_on_gold_owner_node`:
every call goes to Gold's node, not the control node.

### Bug-like behaviour

None found. Two points that are kept as they are:

- With `allow_format=1` the vendor script rewrites `authorized_keys` even
  when the home was already formatted (a second run of the create snippet
  after a change of instance). Under 7.1 A that run happens when
  `ipconfig0`, `nameserver` or `sshkeys` change while `vendor=` still names
  the create file; phase 5 decides whether create switches to `keep` after
  verification.
- The guest fallback reads files through `exec` (`test -s && cat`), which
  needs `VM.GuestAgent.Unrestricted`; the port uses `file-read` (decision 5).

## 2. What was built

| Part | Where |
|---|---|
| The vendor snippet: one template, two files (`allow_format=1` / `0`); the per-VM Cloud-Init options that replace Python's user, network and meta files; `cicustom`; `sshkeys` quoted as Proxmox stores them | `crates/hs-core/src/cloudinit_vendor.yaml`, `crates/hs-core/src/cloudinit.rs` |
| The two files to install on the nodes, kept equal to the template by a test | `deploy/cloud-init/homestack-vendor-{create,keep}.yaml` |
| Workspace keys from Gold (`sshkeys`, then the user's and root's `authorized_keys` through `file-read`), key validation and records | `crates/hs-core/src/keys.rs` (`gold.rs` now uses its `unquote` and validation) |
| Simulator: `test -s F && cat F` through `exec` (Python's key fallback) | `crates/pve-sim/src/guest.rs` |
| Python harness: `func_driver workspace-keys [VMID]`; Rust side `@workspace-keys [VMID]` | `parity/python/homestack_parity/func_driver.py`, `crates/hs-parity/tests/scenarios.rs` |
| Live gate in `check.sh --live` | `crates/hs-server/tests/live_cloudinit.rs` |
| Installation of the snippets | `docs/migration/deploy.md`, section "Cloud-Init vendor snippets" |

**How a workspace gets its Cloud-Init now.** The VM keeps its `name`
(hostname) and gets `ipconfig0=ip=IP/CIDR,gw=GW`, `nameserver` from
`[network] dns`, `ciuser=root`, `ciupgrade=0`, at create `sshkeys` with the
workspace keys, and `cicustom=vendor=<snippet_storage>:snippets/homestack-vendor-<create|keep>.yaml`.
Proxmox generates user-data (`hostname`, `manage_etc_hosts`, `fqdn`,
`user: root`, `disable_root: False`, `ssh_authorized_keys`), network-config
v1 (`eth0` by the MAC of `net0`, static address, gateway, DNS) and
meta-data (`instance-id` = SHA1 of the two). The vendor data adds what the
Python user-data had (`users: []`, `ssh_pwauth: false`, `ssh_deletekeys:
true`, `preserve_hostname: false`), a `bootcmd` that makes NetworkManager
netplan's renderer, and Python's `runcmd` script, changed only where a
static file cannot know the VM:

- the home disk is the one disk whose serial matches `HS_HOME_<digits>`
  (exit 70 otherwise), and its expected label is that serial;
- the account is the image's only regular account (UID 1000–65533; exit 75
  otherwise), with its UID and GID from `/etc/passwd`;
- with `allow_format=1` the keys for the new home are read from the
  user-data (`cloud-init query userdata`), and a user-data without keys
  stops the script before anything is formatted (exit 76).

The rest of the script (refusals 71–74, `mkfs.ext4 -m 0 -L`, the mount unit
and `/etc/fstab` handling, `lost+found`, ownership and modes, linger for
Herdr) is Python's text.

## 3. Tests

Written first and shown failing: `hs_core::cloudinit` and `hs_core::keys`
did not exist, so `tests/cloudinit.rs` and `tests/keys.rs` did not compile.

| | Count |
|---|---|
| Python tests of phase 4 | 3 |
| ported | 3: `HomeDiskTests` ×2 (against the static snippets; `expected_serial=HS_HOME_200` became the serial pattern), `GoldKeyRoutingTests` ×1 (`file-read` instead of `exec`) |
| excluded | 0 |
| new Rust tests | snippets and settings 7 (one checks both scripts with `sh -n`, one that the files in `deploy/` are the template's), keys 10, live 1 |
| Rust tests in the workspace | 195, plus 7 live (`cargo test -- --list`: 202) |
| Python harness tests | 18 (unchanged) |
| manifest | 593 Python tests: 54 ported, 475 todo, 64 excluded |

## 4. Parity with Python

`scenarios/workspace-keys.toml`: 7 comparisons of
`get_workspace_authorized_keys` and `workspace_authorized_keys`, all equal:
usable `sshkeys`; unusable `sshkeys` and the user's file (comments and
blank lines skipped); an empty user's file and root's; nothing usable; a
stopped Gold without keys; a Gold whose agent does not answer; no Gold.
Gold runs on another node than the configured one. Rule `workspace-keys`:
Python's `exec` and the server's `file-read` are left out, the cluster,
config, status and ping requests and the outcome are compared. The server's
token in the scenario has `VM.GuestAgent.FileRead` but no `exec`.

There is no byte comparison of snippets: under 7.1 A the files are new (the
plan says so for this variant); the reference files in `deploy/cloud-init/`
and the boot test replace it. All scenarios of phases 0–3 still agree.

## 5. Live cluster

Before writing the code, a probe by hand (decisions.md, probe results,
2026-10-13), with the owner's approval to put the two snippets into
`/var/lib/vz/snippets` on pve2: Gold cloned to VM 240 in `homestack-test`
by the test token, native Cloud-Init, a 1 GiB home. It showed the one
change native Cloud-Init needed beyond the analysis: Proxmox's
network-config names no renderer and netplan gave `eth0` to the masked
`systemd-networkd`. Two attempts at the fix failed on the way
(`netplan apply` restarts networkd; without a `udevadm trigger` the device
stays `NM_UNMANAGED`); the third worked on a first boot, after a reboot and
on a new instance.

`scripts/check.sh --live`: all passed, the gates of phases 0–3 included
(server in-process, server image, status and Gold readiness against
Python, jobs in the test pool).

- `live_cloudinit.rs` (new, in the test pool, test token, VM 240
  `hst-boot`): Gold cloned on its node, `cloud_init_settings(Create)` with
  the keys from `workspace_authorized_keys`, a home disk with serial
  `HS_HOME_240`, started; `verify_guest` (D2, create) passed: hostname,
  `192.168.100.240/24` on `eth0`, the home mounted from `HS_HOME_240` with
  that label, the user's account, the user's `authorized_keys` equal to
  Gold's keys, root's keys, one regular account, no `sudo`, cloud-id
  `nocloud`. VM removed; a second clone with the `keep` snippet and a blank
  home: no home mount, the disk unformatted, cloud-init's result naming the
  failed `runcmd`. VM removed, no volumes left.

## 6. Decisions taken in this phase and differences from Python

1. **Decision 1 = A, with two static snippets** (owner, 2026-10-13). No
   files per workspace; Proxmox generates user-data, network-config and
   meta-data. Differences that follow:
   - root's `authorized_keys` gets the workspace keys from `sshkeys` (added
     to the keys root has in Gold); Python left root's keys as Gold had
     them. The verification needs root keys either way;
   - `user: root` makes cloud-init write `/etc/sudoers.d/90-cloud-init-users`
     with a rule for root (`sudo` is not installed; verification still
     refuses `sudo`) and report `degraded done` for a deprecated key; root's
     password was already locked in Gold;
   - the instance id is a hash of user-data and network-config, not random:
     a new root (refresh) or new keys or network start a new instance, an
     unchanged VM does not;
   - `/etc/resolv.conf` also gets the node's search domain (`efez.net`) and
     the FQDN is the name, since no `searchdomain` is set;
   - the vendor script finds the home disk by the serial pattern and the
     account in `/etc/passwd` instead of having them written in, and reads
     the keys from user-data; it adds exits 75 and 76 (section 2);
   - netplan's renderer is chosen by the vendor data instead of the network
     snippet.
2. **C2 is not needed.** The server writes no files, so there are no stale
   snippets of a name to find, refuse or remove. Python's snippets of the
   workspaces made by Python stay where they are; the server does not change
   those VMs (gate before phase 5).
3. **C3 is not needed.** Every node has the same two files. `migrate`
   (phase 8) cannot check for them through the API (it needs
   `Datastore.Allocate`, deploy.md); a missing file fails the start.
4. **C5 reads Gold's files with `file-read`** (decision 5), so the server
   needs `VM.GuestAgent.FileRead` on Gold, not `exec`. A refused read is an
   error, a missing or empty file moves on, as `test -s` did.
5. **`sshkeys` is set at create** to the workspace keys, also when they came
   from Gold's files; Python kept the `sshkeys` cloned from Gold and wrote
   the keys into the vendor script.

## 7. Open questions for the next phases

1. **The vendor snippets on the nodes.** For the live gate they are on pve2
   only (the owner allowed them there for the test and asked for their
   removal afterwards; `check.sh --live` now needs them on Gold's node).
   For production the owner installs them on every node that runs
   workspaces (deploy.md).
2. **Gold readiness (E2) does not check what the snippet needs**
   (NetworkManager, `netplan`, `udevadm`, `python3` with PyYAML). Adding
   them changes Gold readiness against Python's; proposed for phase 5 with
   `create`.
3. **Create and the create snippet.** As in Python, `allow_format=1` rewrites
   the home's `authorized_keys` whenever a new instance starts with the
   create snippet (new keys or network). Proposal for phase 5: switch
   `cicustom` to the keep snippet once create is verified.
4. **Phase 9 (`addkey`)** becomes a change of `sshkeys` plus a restart, but
   the keep snippet does not add keys to the home's `authorized_keys`
   (Python's `ensure-keys` step did, for root and the user). Phase 9 needs a
   third way (a key step in the keep snippet reading user-data, or `exec`).
5. **Phase 8** (`migrate`): the two Python tests of the snippet copy
   (`SnippetMigrationTests`) have no counterpart under 7.1 A and will be
   excluded with that reason.
6. **Phase 6** (`refresh`) of a workspace made by Python moves it to native
   Cloud-Init (`ciuser`, `ciupgrade`, `nameserver`, `cicustom` with the keep
   snippet); only for VMs the owner has moved into the pool.
7. Before phase 5: pool `homestack`, the narrowed token and decisions 9 and
   10 (PLAN.md, gate before phase 5).
