# Phase 13c: `homestack install` and `homestack uninstall`

Status: functionally complete and accepted by the owner on 2026-10-18
after repeated E2E installation and uninstallation cycles. Remaining UI
cleanup and parity accounting are tracked separately below.
This file is written before the code (PLAN.md 1.1) and
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
  a volume. Since 2026-10-15 it takes every guest out of the HomeStack pools
  and deletes the pools; the guests keep their tags, and the next install
  finds them by the tags and puts them back (autodiscovery, section 7).
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
                    [--gold VMID] [--adopt VMIDS] [--token-dir DIR] [--json] [-y]
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
  hand-made state of phases 0–4 with `--test`, pools emptied and deleted
  with snippets in use, guests no tag brings back named, autodiscovery
  (offered, adopted, refused, uninstall then install), an unreachable node, an offline
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

**Removed 2026-10-15 (owner):** `homestack install` is a plain command
again. Without `--ssh` or `--herdr` it lists the options it needs and stops;
with them, the choices of the missing ones are read from the cluster and
listed. The form and the wizard (`form.rs`, `wizard.rs`) are gone; the TUI's
install screen is the interactive way. What follows is kept as history.

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
- **Gold is optional:** install the server before preparing a base VM. With
  no unambiguous Gold, the installer warns and continues without tagging or
  adding any VM to the Gold pool. `server.toml` omits `node.gold_vmid`;
  status remains available, while create and refresh stay unavailable until
  Gold is configured. A selected Gold is still validated. The default Gold
  is the VM named `hs-gold`; the installer tags the
  Gold it uses `homestack-gold` (other tags kept) and puts it in pool
  `homestack-gold`, both as steps of the plan. Order: `--gold`; the only VM
  tagged `homestack-gold`; the only QEMU VM named `hs-gold` (other tagged VMs
  are named in a warning and left alone); otherwise installation proceeds
  without Gold, and the optional form field offers the QEMU VMs. A Gold tagged
  `homestack-ws` or `homestack-alien` is refused. Uninstall takes Gold out of
  the pool and leaves its tag.
- **Autodiscovery (owner, 2026-10-15).** A guest in no pool whose tags name
  a pool of the part being installed is offered in the form, under "Back into
  pools", one checkbox each, all ticked; `--adopt VMIDS` on the command
  line, and without it a warning names them. The tag decides, never the
  address: a QEMU VM tagged `homestack-ws` or `homestack-alien` goes to
  `homestack`, or to `homestack-test` when it also carries `homestack-test`
  (the test instance tags its workspaces so); a container tagged
  `homestack-server` (`homestack-deploy` tags every server container, and an
  unchanged one gets the tag on its next `apply`) goes to
  `homestack-service`. Gold is not among them (it has its own setting). A
  VMID that cannot be adopted stops the install before any change. The
  uninstall names each guest it takes out that no tag brings back. What was
  ticked is not kept in the draft of the form. The choices are read once,
  at the connection; F5 in the TUI's settings (rescan cluster) reads them
  again, keeping what is ticked and the domain typed, a guest newly found
  ticked.
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
| bridge | existing bridges common to every online node; selected in **Server network bridge** or with `--bridge NAME`, taken automatically when only one exists; a selected Gold must use the same bridge |
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
set to the selected Gold, or omitted when Gold is not configured. The
deployment's progress is shown as it comes.
Tests: 7 new in `install_cli.rs` (the whole way to a real `status` against a
server on the simulator), 3 in `form.rs` (the typed setting).

## 9. First installation, reinstallation and API updates

The TUI settings form now includes root and home disk slots, default home
size, workspace IP prefix, network prefix length, gateway, DNS servers,
guest user name/UID/GID and snippet directory. Network addresses and guest
user name start empty without a seed; technical disk defaults and numeric
IDs are shown for confirmation. Gold, storage, bridge and domain use the
existing choices. The optional Workspace SSH keys selector lists local public
keys with a matching private identity, preserves configured identities and
saves the selection to the client configuration. It does not upload keys
to Gold.

`config.toml` is optional initial input. Answers are kept in the private
draft, preserved by F5, validated in the review, and saved only after
confirmation. The deployment builds native server and client configuration;
it does not call the Python-era config reader or require Herdr fields.
The guest user is also the generated workspace SSH user.

Without a client configuration the screen identifies a first setup. An
installed profile opens its status view; Menu > Reinstall HomeStack opens
the full form with saved values. Menu > Update API containers uses only
the saved deployment and deploy token, offers published images, then updates
the selected instances. It does not repeat root access or Proxmox account
installation and does not require a running HomeStack API to read the saved
instance list. Enter on a server still selects that instance, or the marked
instances, for the image update.

Regression tests cover installing from an absent seed file through the TUI
worker, entering the network and guest account, rescanning without losing
the typed values, refusal before mutation when settings are incomplete,
native configuration output, final status, first/reinstall titles, the
separate update menu request and text editing at 80 by 24.


## 10. Completion checklist (2026-10-18)

- [x] First-install settings form works without an initial `config.toml`;
  covered by the installer worker regression test.
- [x] Collect root/home disk slots, default home size, network and guest
  account settings; validate them before changing Proxmox.
- [x] Offer Gold, storage and bridge choices from cluster discovery.
- [x] Offer optional local workspace SSH identities, preserve the selection
  across rescans and save it in client configuration.
- [x] Keep form answers in the draft and preserve edits during rescans.
- [x] Distinguish first setup, reinstallation and API container updates.
- [x] Generate native client/server configuration and deploy the API;
  installation through Herdr was exercised in the isolated E2E lab.
- [x] E2E uninstall through SSH removes the HomeStack user, tokens, roles,
  ACLs and pools while preserving Gold 100 and API container 101.
- [ ] Verify vendor snippet cleanup with snippet storage supplied; the
  observed TUI uninstall left both vendor snippets because no snippet
  storage was passed to the uninstall plan.
- [x] Owner accepted phase 13c after repeated E2E install/uninstall cycles
  (2026-10-18). This acceptance closes the functional phase; the specific
  directly observed runs and cleanup limitations remain recorded below.
- [x] Audit and resolve all remaining phase-13c parity entries against
  current behavior and regression coverage; see [the audit report](parity-todo-audit.md).

Related phase-13b verification: manual Gold power uses the configured Gold
pool. The isolated E2E API was updated to image
`0.1.202-a00222404973`, its Gold role was reconciled, and VM 100 was started
and gracefully stopped through the API. The final state was stopped. A stop
issued immediately after boot timed out; retrying once the guest was ready
succeeded. No forced stop was used. Simulator tests cover Gold power, role
and pool restrictions, template/lock refusal, and workspace protection.

E2E uninstall evidence: run `b2e4128c-29f4-4c56-9e9b-20e0352513c1`,
2026-10-18 at 11:15:48 local log time, `root@pve-e2e` over SSH,
39 changes completed. A read-only cluster check found no HomeStack users,
roles, ACLs or pools. Gold 100 and API container 101 were still running.
`homestack-vendor-create.yaml` and `homestack-vendor-keep.yaml` remained;
this run does not close the complete cleanup gate. The subsequent
"no server answers" message is expected after server token revocation.

Owner acceptance (2026-10-18): repeated E2E install/uninstall cycles worked;
phase 13c is closed functionally. The missing-client-file notice shown when
returning from a successful uninstall is a separate UI follow-up: offer the
installer instead of a filesystem diagnostic. This follow-up has not yet
been implemented. The 29 parity `todo` entries still require an evidence-based
audit before phase 15; functional acceptance does not automatically mark
those tests as ported or excluded. Vendor snippet cleanup remains the
explicit follow-up recorded above.

Copy audit (2026-10-18): removed ellipses from application-owned UI text,
CLI help/errors, setup, E2E messages and display truncation. Git range syntax,
YAML document markers and matching third-party package output retain their
machine-readable punctuation. Installer rendering and worker progress events
are checked by Rust tests; the progress-description parity entry is now
ported. The manifest has 28 remaining phase-13c TODO entries and four in
phase 10 (32 total).

Remaining parity audit completed (2026-10-18): all 32 outstanding entries
across phases 13c and 10 are accounted for in
[parity-todo-audit.md](parity-todo-audit.md). Twenty-four entries map to
behavioral Rust tests and eight explicitly exclude removed mechanisms or
owner-approved behavior changes. The audit also repaired selection ordering,
empty draft answers, local discovery before configuration, and failed-clone
cleanup ownership checks. There are no remaining manifest TODO entries.
This does not claim that the previously recorded UI/snippet follow-ups or
phase-15 removal of Python have been completed.
