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
