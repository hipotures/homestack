# Phase 13c: `homestack install` and `homestack uninstall`

Status: in progress (steps 1 and 2 done, step 2 not yet live: see sections 6 and 8). This file is written before the code (PLAN.md 1.1) and
updated as the phase goes; the report at the end says what was done and
what differs.

## 1. What it replaces

Until now the Proxmox side of HomeStack was installed by copying
`deploy/pve/homestack-pve.sh` to a node and running it as root by hand
(phase 4b, `docs/migration/phase-4b.md`). `homestack install` and
`homestack uninstall` do the same from the desktop, in Rust:

- the client reaches **root on one node once**, over SSH or through a Herdr
  pane (nothing else in the product uses Herdr);
- it checks that the access is root and that the node is Proxmox VE, and
  whether the node is part of a cluster;
- it reads the cluster, prints the plan, asks, and applies it with `pveum`,
  `pvesh` and `pvesm` on the node (other nodes through the node's own
  root SSH, as the script does);
- the secrets of new tokens come back to the desktop and are written to
  files (0600); they stay on no node.

The script stays in `deploy/pve/` as the reference until phase 15 (its tests
`deploy/pve/tests/run.sh` run in `check.sh` next to the Rust ones, so the two
implementations are compared on the same fake Proxmox).

## 2. Owner's decisions this follows

- Root is needed once; SSH (possibly a hardware key: wait for the touch, with
  the cues of phase 13) or Herdr as the alternative; afterwards everything is
  API only (`decisions.md`, "after phase 13").
- Uninstall removes only HomeStack objects and **never** a VM, a container or
  a volume; a pool that still holds machines stays and is listed.
- The user opens the access and names it: for SSH `USER@HOST`
  (usually `root@pve1`), for Herdr the **name of a tab or a pane**. The
  client searches for the name and refuses when it finds none or more than
  one (it lists them). Then it checks the access really is root, and that it
  is Proxmox, **before** any plan; a cluster or a single node is detected
  (owner's message of 2026-10-13, after the phase 13 report).

## 3. Command line

```
homestack install   (--ssh USER@HOST | --herdr NAME) [--identity FILE] [--test]
                    --storages IDS --snippet-storage ID
                    --service-storages IDS --template-storage ID
                    [--gold VMID] [--token-dir DIR] [--json] [-y]
homestack uninstall (--ssh USER@HOST | --herdr NAME) [--identity FILE] [--test]
                    [--snippet-storage ID] [--gold VMID]
                    [--token-dir DIR] [--json] [-y]
```

Exactly one of `--ssh` and `--herdr`. The other options are the script's.
Without `-y` the plan is printed and the question asked; with no terminal
and no `-y` the command stops with an error and exit code 1, or with `--json`
prints the plan and exits 3, as every other `homestack` command that needs a
confirmation. `--identity FILE` adds
`-i FILE -o IdentitiesOnly=yes` to the SSH of the root access (the user's
`~/.ssh/config` applies otherwise).

Token files: `pve-token` (production server token), `pve-token-test` (test
part) and `pve-token-deploy` (deploy token) in `--token-dir`, default
`~/.config/homestack/`. A file that exists is never overwritten: the command
stops with the name of the file before it changes anything. If the file
cannot be written after Proxmox made the token, the token is deleted again.

## 4. Design

New library crate `hs-install` (no I/O of its own except through `Root`):

| Part | Content |
|---|---|
| `Root` | `run(command) -> {code, stdout}`; implemented by the SSH root access and the Herdr root access (in `hs-cli`, which owns the SSH master, the touch cues and the process runner) and by a local shell with fake `pvesh`/`pveum` in the tests |
| `probe` | root check (`id -u`), Proxmox check (`pvesh`, `pveum`, `pvesm`, `pvesh get /version`), cluster check (`GET /cluster/status`: a `cluster` entry means a cluster; nodes with `online`, `local`, `ip`; `quorate` of the cluster entry) |
| `state` | what `homestack-pve.sh load_state` reads: nodes, VMs, storages, roles, ACL, user, tokens, pools |
| `checks` | the cluster owner's things that are only checked (versions, storages, Gold and its bridge, tags, certificates) |
| `plan` | install and uninstall plans: ordered steps, each with a description and the exact command; the ACL computation of the script (token entries, the user's union entries, additions before removals) |
| `apply` | runs the steps, writes token secrets to the desktop, writes snippets (chunked base64, since a pane cannot take a long line and a file cannot come over stdin there) |

The SSH root access is `hs-cli`'s `Master` (one authentication, so one
touch of the hardware key for the whole run) and child `ssh` commands
through it. The Herdr root access finds the tab or pane by name
(`herdr workspace list`, `tab list`, `pane list`), requires one pane in it,
refuses a pane that is not at a root shell prompt (last line ends with
`#`), and runs each command wrapped in begin/end markers with a random
token, waiting for the end marker with `pane wait-output`; the marker is
assembled by the shell, so the echo of the typed line never matches it. The
standard output and the standard error come back in base64, so terminal
escapes and line wrapping cannot damage them; the exit code travels at the
end of the output (`\nrc=N`, which also keeps the output's trailing
newlines). After a command that returns a secret (token creation) the
pane's screen and scrollback are cleared.

Differences from the script, all deliberate:

- the plan is a value (printed, or `--json`), not only text;
- no `--dry-run` and no `status` command: a plan without `-y` and without a
  terminal is the dry run (`--json`: exit 3), and the plan is always shown first;
- a cluster without quorum is an error (changes would fail on the node);
- the secret file is written on the desktop, not in `/root` of the node.

## 5. Tests

Written first and shown failing, then the implementation.

- `crates/hs-install/tests/` — the scenarios of `deploy/pve/tests/run.sh`
  on its fake Proxmox (`deploy/pve/tests/fake-pve`, driven through a local
  shell as the `Root`): single node, cluster and uninstall, adoption of the
  hand-made state of phases 0–4 with `--test`, a pool with VMs and snippets
  in use, the service pool with containers, an unreachable node, an offline
  node, every check that stops an install, warnings that do not, plan
  without confirmation, snippet update, idempotence (a second run changes
  nothing); plus the new ones: not root, not Proxmox, single node versus
  cluster, no quorum, a token file that exists, a token file that cannot be
  written (the token is deleted again), a secret never printed.
- `crates/hs-cli/tests/` — the Herdr access against a fake `herdr` (name
  matching: none, several, one; several panes; not at a root prompt;
  marker and length checks; the pane cleared after a secret) and the SSH
  access against a fake `ssh`; the `install`/`uninstall` commands end to end
  (arguments, plan, exit codes, `--json`).
- The 29 Python tests of `test_install.py` and `InstallDraftTests` belong to
  the wizard (section 6) and are ported in that step.

## 6. Steps of the phase

1. Root access, probe, plan and apply of `install`/`uninstall` (this file's
   sections 3–5).
2. Server deployment after the Proxmox part, through the API only
   (`hs-deploy`), with the token files this phase wrote.
3. The configuration wizard (F1): discovery through the server, choice of
   Gold, storage, networks and local FIDO keys, draft with stages and resume,
   `client.toml` and `server.toml` (`PUT /v1/config`).

Each step is committed on its own; the tag `rust-phase-13c` comes with the
last one, after the `--live` gate (a clean cluster: install over SSH and over
Herdr, deployment, `homestack status`, uninstall with no trace and no change
to foreign VMs). The gate changes ACLs on the real cluster, so it runs only
with the owner's approval.

## 7. The interactive installer (owner's requests of 2026-10-14)

- **`homestack install` is a TUI wizard by default** (ratatui, `wizard.rs`);
  giving the options on the command line is the manual way, and whatever is
  given is not asked. The wizard is used when the command runs on a terminal
  without `--json` and `-y` and not every option is given.
- **Steps:** how to reach root (SSH: the node typed as `USER@HOST`; Herdr: a
  tab from the tabs with one pane, with its pane's title), then one step per
  option still open: workspace disks, Cloud-Init snippets, server disks,
  server image, Gold. A step shows a heading, the step number, a sentence on
  what is chosen, and **only what can be chosen** (storages with the content
  type it needs, VMs that can be Gold). No command-line option name is shown.
- **Not asked:** an option with a single possible value is taken (the summary
  lists it under "Chosen without asking"); Gold is not asked when one VM is
  tagged `homestack-gold` or one is named `hs-gold`.
- **Defaults:** for disks the block storages are ticked (not `dir`, `nfs`,
  `cifs`, `cephfs`, `glusterfs`, `pbs`); for the snippets and the image,
  `local` when it is there.
- **Summary, the last step:** where root comes from, every choice, the
  warnings and every change the plan would make, with the buttons
  **[ Install ]** and **[ Back ]**. Esc, or Back, returns to the choices;
  nothing is changed before Install. A problem (an error of the checks) is
  shown at the top and Install cannot be pressed. After Install the command
  prints the equivalent command line (`Same as: ...`) and applies the plan.
- Where the terminal cannot be drawn on, the same steps are asked by number
  (Enter keeps the marked defaults) and the summary ends with `Install? [y/n]`.
  Without a terminal the missing options are printed with their defaults
  (`--json`: a `missing` array with `defaults`), exit code 1.
- **Gold:** the default Gold is the VM named `hs-gold`; the installer tags the
  Gold it uses `homestack-gold` (other tags kept) and puts it in pool
  `homestack-gold`, both as steps of the plan. Order: `--gold`; the only VM
  tagged `homestack-gold`; the only QEMU VM named `hs-gold` (other tagged VMs
  are named in a warning and left alone); otherwise a step. A Gold tagged
  `homestack-ws` or `homestack-alien` is refused. Uninstall takes Gold out of
  the pool and leaves its tag.
- The fake Proxmox (`fake-pve`) learned `pvesh set ... --tags`.
- **Revised the same day (owner): one form instead of steps.** The installer
  shows one screen with every setting and its value: how root is reached,
  workspace disks, Cloud-Init snippets, server disks, server image and Gold,
  with the buttons **[ Install ]** and **[ Cancel ]** (`form.rs`). Enter on a
  setting opens its list (only what can be chosen; space ticks, enter takes,
  esc undoes); Gold can always be changed, its first choice being the VM the
  installer would take. Install opens **Confirm the installation** with the
  choices, warnings and every change, and the buttons **[ Install ]** and
  **[ Back ]**; Esc returns to the form. A setting given on the command line
  is shown with "(given on the command line)" and cannot be changed there.
  Where nothing can be drawn, the form is asked by number.
- **Kept between runs (owner, 2026-10-14):** the choices of the form are
  written to `~/.config/homestack/install-draft.toml` (mode 0600) whenever the
  form is left (Install, Cancel, Esc) and are where the next run starts; a
  choice no longer possible, or a file that cannot be read, is ignored.
- **A token file left by an earlier installation** (its token is gone from
  Proxmox) no longer stops the install: the plan keeps it as
  `NAME.replaced-<seconds>` and writes the new secret under the name; it is
  never overwritten, and it is put back if the token cannot be created.

## 8. Step 2: the servers, and `homestack status` at the end (owner, 2026-10-14)

The owner's rule: **an installation ends with the client connecting to the
server and `homestack status` answering correctly**; no file is written by
hand. `homestack install` therefore goes on, after the Proxmox part, to
what `homestack-deploy` did from a hand-written `deploy.toml`:

1. the Proxmox part (sections 3–7);
2. the certificate `*.<domain>` (Let's Encrypt, DNS-01 at OVH; a stored one
   that is still valid for 30 days is used);
3. one server container per node (`hs_deploy::deploy::apply`, deploy token,
   pool `homestack-service`);
4. `client.toml` and `deploy.toml` written on the desktop (an existing file
   that differs is kept as `NAME.replaced-<seconds>`);
5. `homestack status` with the new `client.toml`; its exit code is the
   install's.

**Asked:** one more row of the form, **Server domain** (typed, kept in the
draft; `--server-domain` on the command line). Everything else is found:

| What | Where it comes from |
|---|---|
| instances | one per online node that has a chosen server-disk storage: `api1`, `api2`, ... in the order of the nodes |
| address | the desktop's DNS answer for `apiN.<domain>`; a name that does not resolve is a problem (Install cannot be pressed) |
| network | `[network]` of the server configuration (`cidr`, `gateway`, the first `dns`); an address outside the gateway's network (`gateway` and `cidr`) is a problem (2026-10-14; it was a text match on `prefix`) |
| bridge | Gold's `net0` |
| CTID | the last two octets of the address, the third times 1000 plus the fourth (`192.168.102.1` is CT 102001; owner's decision of 2026-10-14). The container of `homestack-service` with that number and host name is replaced; that number held by anything else, the host name on another number, or a number under 100 is a problem |
| root disk | the first chosen server-disk storage on that node |
| Proxmox endpoints | `https://<name on the node's API certificate>:8006` |
| image | `ghcr.io/hipotures/homestack-server:<version>-<commit>` of this client's build (`--server-image` overrides) |
| server configuration | `server.toml` from `~/.config/homestack/config.toml` (`hs-config split`); without it the install stops with a problem (the configuration wizard is step 3) |
| tokens | `pve-token`, `pve-token-deploy` of the Proxmox part; the client token `api-token`, made (32 random bytes, hex, 0600) when missing |
| certificate | zone = the longest of `<domain>` and its parents that has NS records in the public DNS (asked of 1.1.1.1 and 8.8.8.8, not the local DNS); its DNS at OVH takes the key `~/.config/homestack/ovh-dns`, at Spaceship `~/.config/homestack/spaceship-dns` (`SPACESHIP_API_KEY`, `SPACESHIP_API_SECRET`), anywhere else is a problem (2026-10-14); kept in `~/.config/homestack/acme` |

The confirmation gets a section **Servers** with one line per instance
(`api1.hs.example.net  192.0.2.97  pve1  CT 120  local-zfs`), the image and
the certificate, and the last line **Then: homestack status**.

`--test` installs the test part only, with no servers. On the command line
the manual way needs `--server-domain` or `--no-servers` (the Proxmox part
only); without either it is listed as missing. `uninstall` does not remove
the containers (owner's rule: never a container); the plan lists them as
kept, `homestack-deploy remove` removes them.

Tests: the instances, addresses, CTIDs, problems and the confirmation are
computed from the fake cluster and a fake resolver; the deployment and the
final `status` go through `Context::servers`, a fake in the tests that
records the `DeployConfig` and the `client.toml` it gets. The real
deployment is the `--live` gate.

As built (2026-10-14): `crates/hs-cli/src/servers.rs` (the plan of the
servers, `client.toml`, files kept aside), `Context::servers` (`RealServers`:
the system resolver, `hs_deploy::deploy::cert` then `apply`), the commit of
the build from `crates/hs-cli/build.rs`, `app::status_of` for the last
check. `server.toml` is the old file's server part with `[node] gold_vmid`
set to the installed Gold. The deployment's progress is shown as it comes.
Tests: 7 new in `install_cli.rs` (the whole way to a real `status` against a
server on the simulator), 3 in `form.rs` (the typed setting).
