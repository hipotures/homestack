# Phase 3: tasks, guest agent and the operation model

**Date:** 2026-10-12
**Branch:** `rust-migration`, tag `rust-phase-3`
**Scope:** T4, T5, T6, D2, E2; section 8.4 of [api-mapping.md](api-mapping.md)
**Decisions used:** 5 (guest verification at `Audit` + `FileRead`), and the
owner's grant of `VM.GuestAgent.Unrestricted` on Gold to the server token
(decisions.md, 2026-10-12)

## 1. Python behaviour (step 3)

### Guest command (`guest._complete_guest_exec`, `guest_exec_on_node`, `guest_out_on_node`; T6)

1. `qm guest exec VMID -- /bin/sh -lc CMD` (shell-quoted), bounded by
   `min(60, timeout)`. A failing `qm` call raises `QEMU Guest Agent exec
   failed for VM N on NODE: OUTPUT` when `check`, else becomes a result with
   `exited=1`, `exitcode=<qm's code>` and qm's output as `out-data`.
2. The last JSON object line of the output is the answer (`parse_qm_guest_exec`;
   failing that, the text between the first `{` and the last `}`; else
   `Could not parse QEMU Guest Agent response`).
3. With a `pid` and no `exited`, `qm guest exec-status VMID PID` is polled
   every 0.2 s until `exited` or the deadline (`timeout`, default 300 s):
   `Guest command did not finish in VM N on NODE within Ts: CMD`. An invalid
   PID or a failing status call are errors like step 1.
4. The final answer must have `exited` and `exitcode`, else `QEMU Guest Agent
   returned an incomplete response for ...: <dict>`; an exit code that is not
   an integer is an error too.
5. With `check`, an unfinished or non-zero exit raises `Guest command failed
   in VM N on NODE (exit=E): CMD` plus a line with stripped `err-data` or
   else `out-data`. `guest_out_on_node` returns stripped `out-data`.

### Waiting for the agent (`guest.wait_for_qga_on_node`, `qga_ping_command`; D1)

- `timeout -k 2s 5s qm guest cmd VMID ping` every 2 s until it exits 0 or the
  timeout (default 300 s; 600 after `create` and `refresh`, 180 after
  `migrate`). A transport error counts as a failed attempt. On timeout:
  `VM N on NODE did not expose a working QEMU Guest Agent within Ts: LAST`.

### Proxmox tasks and progress (T4)

- Python calls `qm start`, `qm shutdown`, `qm clone` and friends, which wait
  for their task. `lifecycle.run_transfer_with_progress` reads the output
  while it runs and shows the last `CLONE_TRANSFER_RE` match
  (`transferred A U of B U (P%)`): the percentage clamped to 0–100, the
  detail `A / B U` (same unit) or `A U / B V`, and an update only when the
  percentage changed. `MIGRATION_*_RE` are read by `migrate` (phase 8).
- `proxmox.shutdown_vm_on_node`: a stopped VM is left alone; any status other
  than `running` is refused (`VM N has unexpected status 'S'; refusing
  shutdown`); `qm shutdown N --timeout 90` without check; then the status is
  read again, and a failed command or a status other than `stopped` raises
  `VM N did not shut down cleanly on NODE. Current status: S. HomeStack will
  not force-stop it automatically.`
- `qm start N` (timeout 300) raises on a non-zero exit.

### Optimistic locking (`lifecycle._digest_args`; T5)

- `--digest D` is added to `qm set` and `qm resize` when the config read
  before had a non-empty `digest`; Proxmox refuses the change when the config
  changed in between.

### Guest verification (D2)

`create_workspace` (block "Verify workspace"), `_verify_refreshed_guest` and
the end of `migrate_workspace`, all after the VM was started:

| Step | Python (`exec` unless noted) | create message | refresh message |
|---|---|---|---|
| 1 | wait for the agent (600 s) | as D1 | as D1 |
| 2 | `command -v cloud-init && timeout 180 cloud-init status --wait \|\| true` (never fails) | | |
| 3 | `hostname` = VM name | `Verification failed: hostname is 'A', expected 'B'` | `Refresh verification failed: workspace hostname is incorrect` |
| 4 | `ip -4 -o addr show dev eth0` contains `IP/CIDR` | `... eth0 does not have IP/CIDR\nOUTPUT` | `... eth0 does not have IP/CIDR` |
| 5 | `findmnt -n -o SOURCE,FSTYPE,TARGET /home/U` contains `ext4` and the path | `... /home/U is not mounted as ext4\nOUTPUT` | `... /home/U is not ext4` |
| 6 | `findmnt \| blkid` label = `HS_HOME_<vmid>` | `... persistent home label is 'A', expected 'B'` | `... home label is 'A'` |
| 7 | `id U` contains `uid=UID(U)` | `... unexpected user identity: OUTPUT` | same with `Refresh` |
| 8 | `test -s /home/U/.ssh/authorized_keys` | `... user authorized_keys is missing` | `... persistent user authorized_keys is missing` |
| 8a | create only: the key identities in that file equal the requested keys | `... SSH public keys mismatch (missing: ...; unexpected: ...)` | |
| 9 | `test -s /root/.ssh/authorized_keys` | `... root authorized_keys is missing` | same |
| 10 | `awk` on `/etc/passwd`: exactly `U:UID:GID` among UIDs 1000–65533 | `... unexpected regular user accounts: LIST` (or `none`) | `... unexpected regular user accounts` |
| 11 | `command -v sudo` absent | `... sudo is installed in the workspace` | same |
| 12 | `cloud-id` (informational, never fails) | | returned |

Steps 3–11 use `check=True`: a command that exits non-zero (e.g. `findmnt`
on a path that is no mount point, `id` of a missing user) fails with the
guest-command message instead of the check's message. `migrate` checks only
steps 5 and 6 (`Migration completed but persistent home verification failed
on NODE: mount='...', label='...'`) after waiting 180 s for the agent.

### Gold guest contract (`gold.check_gold_readiness`, parts `guest` and `security`; E2)

Only when Gold runs (a stopped Gold is never started; phase 2 ported that
branch). After the agent ping (`QEMU Guest Agent response`; on failure every
later check is `skip` with `QEMU Guest Agent unavailable`), all through
`exec` without check:

1. `Required guest tools`: `for c in cloud-init sshd nmcli lsblk blkid wipefs
   mkfs.ext4 mount mountpoint systemctl systemd-escape install; do command -v
   "$c" ... || printf "%s\n" "$c"; done`; missing ones listed.
2. `rsync for Setup Files` (optional): `PRESENT`/`ABSENT`.
3. `git/ssh-keygen for homestack repo` (optional): the same loop for `git
   ssh-keygen`.
4. `Workspace account`: `getent passwd U || true` has name, UID and GID.
5. `Regular user policy`: the `awk` of D2 step 10 gives exactly `U:UID:GID`;
   the detail lists them or `none`.
6. `No sudo`: `command -v sudo`.
7. `Root authorized_keys`: `test -s /root/.ssh/authorized_keys`.
8. `Workspace public-key source`: usable Proxmox `sshkeys`, else `cat
   /home/U/.ssh/authorized_keys` validated by `validate_authorized_keys`.

The progress callback reports six steps (read config, check agent, tools,
accounts, keys, done).

### Plan and execution (8.4)

- Every changing command builds a plan first (`build_create_plan`,
  `build_refresh_plan`, ...), shows it and asks; `--yes` skips the question,
  `--json` without `--yes` returns `confirmation_required` (exit code 3).
- Before changing anything the plan is checked again: `_revalidate_stream_source`,
  a new plan in `resize_workspace`, a new `resolve_existing_workspace` in
  `destroy` and `refresh`.
- Progress is shown as named steps (`start_step` / `finish_step`) and, for
  transfers, a percentage.

### Bug-like behaviour

- D2 step 4 is a substring test: `10.0.0.20/24` is found in `inet
  110.0.0.20/24`. Expected addresses are full `prefix.<vmid>` addresses, so
  this cannot happen with a valid configuration; the server compares whole
  addresses (section 6).

## 2. What was built

| Part | Where |
|---|---|
| Guest agent: `exec` + `exec-status` with Python's checks and deadlines, bounded ping, waiting for the agent, `file-read`, the ext4 label from the superblock, `command -v` through `file-read` | `crates/hs-core/src/guest.rs` |
| Proxmox tasks: UPID, status and log polling, every log line once, transfer progress (`CLONE_TRANSFER_RE`) | `crates/hs-core/src/tasks.rs` |
| `qm start` and `shutdown_vm_on_node` | `crates/hs-core/src/power.rs` |
| `set_vm_config` with `digest` (T5) | `crates/hs-core/src/inventory.rs` |
| Guest verification D2 at `Audit` + `FileRead`, with the create and refresh messages | `crates/hs-core/src/verify.rs` |
| Gold guest contract E2 through `exec` | `crates/hs-core/src/gold.rs` |
| Plans, jobs, events (DTOs) and the error codes `plan_changed`, `plan_applied`, `busy` | `crates/hs-model/src/jobs.rs`, `error.rs` |
| Operation registry, plan store, apply with a second plan, jobs on their own task, one job per VM | `crates/hs-server/src/operations.rs` |
| Routes `POST /v1/plans`, `GET /v1/plans/{id}`, `POST /v1/plans/{id}/apply`, `GET /v1/jobs`, `GET /v1/jobs/{id}`, WebSocket `GET /v1/jobs/{id}/events` | `crates/hs-server/src/jobs.rs` |
| Simulator: `status/start`, `status/shutdown`, `status/stop` as tasks (a guest may ignore ACPI), `agent/file-read`, `PUT config` with `digest`, guest accounts, programs and files, the commands of the Gold contract and of the refresh check, PVE's empty-log line | `crates/pve-sim` |
| Python harness: `func_driver` `gold-readiness VMID`, `verify-refresh VMID`; `live_driver` | `parity/python/homestack_parity` |
| Parity runner: server functions without a route (`rust = "@verify-refresh 220"`) | `crates/hs-parity/tests/scenarios.rs` |
| Live: `scripts/live-compare-gold.sh`, `crates/hs-server/tests/live_jobs.rs` (both in `check.sh --live`) | |

**The operation model.** `POST /v1/plans` takes an operation name and its
parameters and returns a plan with an ID, a digest (SHA-256 of the
operation, parameters, target VMIDs and summary) and an expiry (15 min).
`POST /v1/plans/{id}/apply` with the digest the client reviewed plans again
and starts the job only when the new digest is the same (`409
plan_changed` otherwise, `plan_applied` for a second apply, `busy` while
another job works on one of the VMs). The job answers `202` with
`Location: /v1/jobs/{id}`; it runs on its own task, so a client may
disconnect. A job has named steps (Python's `start_step`/`finish_step`),
the Proxmox tasks it waited for (UPID, type, VMID, status, exit status),
the latest transfer progress, the last 200 log lines, and a result or an
error. The WebSocket sends a `snapshot`, then the whole job after each
change and a `heartbeat` after five quiet seconds, each with
`schema_version`, `server_instance_id`, `stream_sequence` and
`server_time` (as mypowers), and closes after the final state. No
operation is registered yet; the tests register their own.

## 3. Tests

Written first and shown failing: `hs_core::{guest, tasks, power, verify}`,
`hs_model::jobs`, `hs_server::operations` and the new `Pve` trait did not
exist, so the test crates did not compile.

| | Count |
|---|---|
| Python tests of phase 3 | 10 |
| ported | 7: `QgaWaitTests` ×4, `ProgressTests` ×1, `GoldReadinessTests` ×2 |
| moved | 3 `GuestIdentityTests` (`test_alien_workspace.py`) to phase 14a: they test `setup_guest.verify_identity`, which runs inside the guest over SSH (mapping G, setup), not through the guest agent |
| excluded | 0 |
| new Rust tests | guest agent 4, Gold 1 (replacing phase 2's test of a running Gold), D2 4, tasks and power 6, plans and jobs 5, live 1 |
| Rust tests in the workspace | 175, plus 6 live (`cargo test -- --list`: 181) |
| Python harness tests | 18 (unchanged) |

The ports keep using `FakePve`, which now also answers in sequence, fails
before reaching Proxmox, never answers (for the bounded ping) and maps
HTTP statuses as the client does. Time is paused in the guest tests, as the
Python tests patch `time.sleep`.

## 4. Parity with Python

`scenarios/guest-checks.toml`: 10 comparisons, all equal.

- Gold readiness of a running Gold that meets the contract, and of one that
  breaks most of it (tools missing, `sudo`, a second regular user, no root
  keys, no `sshkeys`, an unusable key file). The operations are identical:
  both run the same commands through `exec`.
- The refresh verification (`_verify_refreshed_guest`) for a healthy guest
  and for a wrong hostname, a missing user `authorized_keys`, `sudo`, an
  extra regular user, a wrong address, another VM's home label and a missing
  root `authorized_keys`. Rule `guest-verify`: the guest reads differ by
  decision 5, the pings and VM reads are compared.
- Two cases are not compared, because Python fails there with a guest
  command's error that the server never runs (section 6): no home mount and
  another UID. `crates/hs-core/tests/verify.rs` checks their messages.
- The first run showed the comparison catches a difference (the server's
  `file-read` calls before the rule dropped them).

## 5. Live cluster

`scripts/check.sh --live`, all passed:

- `live-compare-gold.sh`: Python's `check_gold_readiness` (the installer's
  call, through Herdr) and `GET /v1/gold/readiness` on Gold 101 (running):
  equal, all 18 checks run, the guest contract included.
- `live_jobs.rs` in pool `homestack-test` with the test token: VM 240
  (`hst-task`, no disk, no network) created; a `test.start` job followed
  through the WebSocket from its `qmstart` UPID to `OK`; a `test.shutdown`
  job failing as Python does (`VM 240 did not shut down cleanly on pve2.
  Current status: running. HomeStack will not force-stop it
  automatically.`, task `VM quit/powerdown failed - got timeout`); the VM
  stopped and destroyed afterwards. The test refuses VMIDs outside
  240–249, a VMID in use, and any VM without its name and marker.
- The live run found a bug the simulator did not have: Proxmox answers an
  empty task log with a made-up line `no content`, which the first version
  counted as line 1 and so lost the task's real first line. Fixed, modelled
  in the simulator and tested (decisions.md, probe results).
- The gates of phases 0–2 passed in the same run.

## 6. Decisions taken in this phase and differences from Python

1. **Decision 5 applied to D2.** The checks, their order and the refresh
   and create messages are Python's; the data comes from `get-host-name`,
   `network-get-interfaces`, `get-fsinfo` and `file-read` (`/etc/passwd`,
   the `authorized_keys` files, the home device's ext4 superblock,
   `/run/cloud-init/result.json` and `cloud-id`, a `sudo` file on the
   `PATH`). Differences that follow:
   - addresses are compared whole, not as a substring (section 1); the
     create message lists `eth0`'s addresses instead of `ip addr` output;
   - a missing home mount or user fails with the check's message (`... is not
     ext4`, `unexpected user identity: <passwd line>`) instead of the failing
     `findmnt` / `id` command;
   - `unexpected user identity` shows the `/etc/passwd` line, not `id`
     output;
   - `sudo` counts when a file of that name is readable in a `PATH`
     directory (`command -v` also needs it to be executable);
   - when cloud-init is installed, the server waits for `result.json`
     (`cloud-init status --wait` waits for the same).
   `create` compares the key identities in `authorized_keys` (step 8a) in
   phase 5; the verification returns the file for it. `migrate`'s two checks
   (phase 8) use the same reads.
2. **Gold guest contract (E2)** runs Python's commands through `exec`, so
   the results are the same. Without `VM.GuestAgent.Unrestricted` on Gold the
   contract is `skip` with the reason (Python runs as root). A refused
   `exec` is an error even without `check`.
3. **`/v1/gold/readiness`** uses `[user]` of the configuration, or the
   installer's defaults (`user`, 1000:1000) before one is uploaded.
4. **Task failures** name the task and its exit status (`Proxmox task
   qmstart for VM 240 on pve2 failed: VM 240 already running`) where Python
   shows `qm`'s output. A shutdown task is followed for its timeout plus 30
   seconds, like Python's `qm shutdown` call.
5. **A failed `exec` without `check`** becomes exit code 1 with the error as
   output (Python: `qm`'s exit code and output).
6. **Plans and jobs** live in one server instance's memory (phase 11 decides
   how several instances share them).

## 7. Open questions for the next phases

1. The gate before phase 4 still needs decisions 1 (snippets), 2 (root
   name) and 4 (refresh journal); decision 5 is taken.
2. Phase 5 registers the first real operations and adds the create-only key
   comparison (D2 step 8a) and the `create` verification messages to the
   parity scenarios.
3. The workspace token needs `VM.GuestAgent.FileRead` on the HomeStack
   pool for D2 (mapping 5); today's server token has it on Gold only.
