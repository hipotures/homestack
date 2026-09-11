# HomeStack

HomeStack is an installable Python application for managing disposable Proxmox VE workspace roots while keeping each workspace home directory on a separate persistent disk. It runs on a trusted desktop and currently reaches Proxmox through an already authenticated root SSH shell in Herdr.

## Installation

HomeStack requires Python 3.12 or newer and [uv](https://docs.astral.sh/uv/). Install the locked environment from the repository root:

```bash
uv sync
```

Run either entry point:

```bash
uv run homestack --help
uv run python -m homestack --help
```

The desktop also needs `herdr` and `ssh`. File transfers additionally require `rsync`. GitHub repository provisioning additionally requires an authenticated `gh` CLI on the trusted desktop. Proxmox nodes need `qm`, `pvesh`, `pvesm`, `perl`, `base64`, `ssh`, and `scp`.

## Configuration

Inspect the local Herdr/Proxmox environment without reading a HomeStack config or changing Proxmox resources:

```bash
uv run homestack discover
uv run homestack discover --json
```

Create or reconfigure the runtime config with the interactive installer:

```bash
uv run homestack install
```

HomeStack reserves one Herdr workspace named exactly `PVE` for Proxmox administration. Put one or more tabs in that workspace, with each usable tab containing exactly one pane running an interactive root SSH session to a Proxmox node and left at that node's root shell prompt. Naming tabs after their nodes, for example `pve1`, `pve2`, and `pve3`, is recommended. Other Herdr workspaces are outside HomeStack's discovery scope and are not probed.

The installer uses this dedicated `PVE` workspace for read-only discovery, selects a verified root Herdr session, then asks only for configuration choices that cannot be discovered safely. A first-time installation is checkpointed stage by stage into the requested config path as an explicit install draft. After each completed stage the selected values are atomically saved; if a later stage fails, is interrupted, or needs manual preparation (for example Gold readiness), rerunning `homestack install` with the same `--config` path offers to continue and skips completed stages. The draft is deliberately not accepted by normal runtime commands and becomes a normal configuration only after final validation and confirmation. Restarting from scratch is explicit. Reconfiguring an already valid runtime config keeps the old config intact until the replacement is fully validated.

The installer discovers workspace network profiles from existing HomeStack VMs and read-only PVE network/DNS data on the Gold VM's inherited bridge, lets the user choose when more than one profile is available, and asks only for missing network fields. Hardware-backed SSH identities are selected by number instead of being repeated as a long comma-separated path default.

Manual configuration from `config.example.toml` remains available when needed.

The default is `~/.config/homestack/config.toml`, or `$XDG_CONFIG_HOME/homestack/config.toml` when `XDG_CONFIG_HOME` is set. Override it for any invocation with `--config PATH`:

```bash
uv run homestack --config /path/to/config.toml status
```

Only the neutral example belongs in this repository. A real `config.toml` and private sync paths, credentials, node names, addresses, storage IDs, Herdr names, and SSH identity paths must remain outside it.

Configure the generated workspace SSH entry in `[workspace_ssh]`: `user` must match `[user] name`, `identity_files` lists the desktop's hardware-backed private-key paths, `identities_only` controls OpenSSH's `IdentitiesOnly`, and `log_level` selects a supported OpenSSH log level. HomeStack writes these configured values without hardcoded key paths and does not add password or implicit key fallback authentication.

The selected remote backend is explicit:

```toml
[transport]
type = "herdr"

[transport.herdr]
workspace = "PVE"
tab = "pve1"
debug = true
```

Herdr is the only implemented backend. A small transport factory owns backend setup and verification, while the transport protocol exposes remote execution and generic execution metadata. Lifecycle, status, and Proxmox operations do not depend on Herdr pane internals, so another backend can be added later without rewriting their orchestration. Direct SSH is intentionally not implemented.

## Trust and transport model

HomeStack remains a trusted-desktop tool. Bootstrap discovery inspects only the Herdr workspace named `PVE`; it does not execute probes in unrelated Herdr workspaces. After configuration, HomeStack drives one already authenticated SSH root shell in the configured Herdr workspace, tab, and pane. Before remote work it verifies that:

- the configured workspace and tab each match exactly once;
- the tab has exactly one pane;
- the foreground process is SSH to the configured control node;
- the terminal is at the expected root prompt;
- an end-to-end probe returns the expected hostname and UID 0.

Commands run in an isolated child shell with an explicit timeout and unique begin/end envelopes, so a remote `exit` cannot close the long-lived SSH shell and every completed command has an exit marker. `debug = true` retains command output in Herdr scrollback; `false` clears it after parsing. HomeStack stores no Proxmox token or private credential on a PVE host and never adds password, fallback-key, or `sudo` authentication paths.

The configured control node is only the transport entry point. Operations tied to a VM or local storage are explicitly routed to the node that owns that resource, including when it differs from the control node.

## Commands

```bash
uv run homestack discover
uv run homestack install
uv run homestack transport
uv run homestack create 200 example-workspace
uv run homestack create 200 example-workspace --home-size 20G
uv run homestack create 200 example-workspace --storage example-storage
uv run homestack refresh 200
uv run homestack migrate 200 pve-example-2 --target-storage example-storage
uv run homestack sync 200
uv run homestack repo example-workspace
uv run homestack repo example-workspace owner/repository
uv run homestack destroy 200
uv run homestack status
uv run homestack status 200
```

Lifecycle targets may be a numeric VMID or exact workspace name. `create`, `refresh`, `migrate`, `sync`, and `destroy` resolve and display a plan before confirmation. Use `--yes` to accept a plan and `--json` for machine-readable plans/results.

## Preparing a Gold VM

Gold is a user-maintained base VM, not a HomeStack-generated operating-system image. HomeStack intentionally leaves the OS, system packages, development tools, and other root-filesystem contents under the user's control, but the VM must satisfy a small contract so that cloning, refresh, persistent home, networking, SSH, and guest verification remain deterministic.

### Proxmox VM contract

Prepare a QEMU VM with the following properties:

- add the exact HomeStack role tag `homestack-gold`; do not also tag the VM `homestack-ws`;
- attach the disposable OS root as `scsi0` and include `scsi0` in the VM boot order;
- do not attach a persistent-home disk or other data disks to Gold; HomeStack creates workspace `scsi1` itself, and a full clone would otherwise copy unrelated disks;
- configure exactly the primary workspace NIC as `net0`; HomeStack derives the workspace MAC and bridge from it;
- attach a Proxmox Cloud-Init drive;
- enable the Proxmox QEMU Guest Agent option;
- avoid host-specific passthrough devices in Gold;
- configure at least one valid public SSH key in Proxmox `sshkeys` if Gold will normally remain stopped. This lets HomeStack obtain workspace public keys without booting Gold.

Gold may live on any storage available to its node. Its source root volume name is discovered from `scsi0` and does not need a HomeStack workspace volume name.

### Guest contract

Current HomeStack guest initialization targets a systemd Linux guest using Cloud-Init and NetworkManager. For Debian/Ubuntu-family Gold images, the required baseline is:

```bash
apt-get update
apt-get install -y cloud-init qemu-guest-agent network-manager openssh-server e2fsprogs util-linux
```

Install `rsync` only if `homestack sync` will be used:

```bash
apt-get install -y rsync
```

`homestack repo` additionally requires `git` and `ssh-keygen` in the workspace. These are optional Gold capabilities because repository provisioning is independent of the VM lifecycle.

Run administrative preparation as `root`; HomeStack does not use `sudo`. The Gold readiness table labels every check as `required` or `optional`. Missing required checks block the installer stage; missing optional capabilities are reported but do not block final configuration.

The guest must contain the configured workspace account before cloning. With the default HomeStack settings this is:

```text
root
user  UID 1000  GID 1000
```

The workspace account must not have root escalation. The intended HomeStack security model has no `sudo` package in Gold or workspaces. There must be no additional regular user accounts with UID 1000-65533. System/service accounts are unaffected by this rule.

Root must have a usable `/root/.ssh/authorized_keys`. Workspace public keys are taken first from the Proxmox `sshkeys` setting and, when Gold is running with QEMU Guest Agent available, may fall back to `/home/<USER>/.ssh/authorized_keys`. Only public keys belong in Gold or Proxmox configuration; private hardware-key material remains on the trusted desktop.

HomeStack-generated Cloud-Init explicitly disables SSH password authentication and requests regeneration of cloned SSH host keys. The current network snippet uses `renderer: NetworkManager`, so NetworkManager is a current Gold requirement rather than an arbitrary recommendation.

### Finalize Gold before normal use

After installing and configuring the guest, enable the services required on cloned workspaces:

```bash
systemctl enable qemu-guest-agent
systemctl enable NetworkManager
systemctl enable ssh
```

For an image that will normally remain stopped, clean Cloud-Init state and the machine ID immediately before the final shutdown:

```bash
cloud-init clean --logs --machine-id
poweroff
```

Do not boot the finalized Gold again unless you intend to update and re-finalize it. `cloud-init clean --machine-id` prevents clones from inheriting the Gold machine identity. HomeStack's per-workspace Cloud-Init metadata supplies a new instance identity and hostname, while `ssh_deletekeys: true` regenerates SSH host keys.

Gold's root may contain any system-wide packages and configuration that should reappear after every `refresh`. Project data and user state should not be baked into Gold: workspace `/home/<USER>` is a separate persistent ext4 disk and survives root refreshes and migrations.

### Installer readiness check

Gold readiness is a required, checkpointed installer stage. Transport selection, Gold selection, and workspace-account values are saved before it runs. If the check fails, fix the Gold VM and rerun the same installer command; those completed stages are loaded from the draft and are not asked again.

After Gold selection and workspace-account selection, `homestack install` performs a non-destructive readiness check. Required checks validate the PVE role tag, root/data-disk layout, `net0`, Cloud-Init drive, QEMU Guest Agent option, and boot order. If Gold is running, required guest and security checks also cover QEMU Guest Agent access, the mandatory guest tools, the configured user/UID/GID, the no-`sudo` policy, regular-user policy, root SSH keys, and a workspace public-key source. `rsync` is an optional capability needed only for `homestack sync`; `git` and `ssh-keygen` are optional capabilities needed only for `homestack repo`.

The installer never starts Gold just to inspect it. A stopped Gold can therefore pass the PVE-side contract when it has valid Proxmox `sshkeys`, but the installer reports guest checks as not inspected. Runtime `create` and `refresh` verification still fail closed if the resulting workspace violates the account, SSH, persistent-home, or guest requirements.

## Storage and lifecycle invariants

- Gold is the configured `gold_vmid` and must have the exact `homestack-gold` tag.
- Workspaces have the exact `homestack-ws` role tag.
- `scsi0` is the disposable root; `scsi1` is persistent home. The configured root and home slots must always be different.
- Persistent home is ext4 with label `HS_HOME_<VMID>`.
- Persistent-home volumes are named `vm-<VMID>-hs-home-<USER>`. A newly created root is named `vm-<VMID>-hs-root-default`; a refreshed root may retain the collision-free name allocated by Proxmox during staging.
- Create placement and migration targets must belong to the node's configured `storage_layouts` list.
- Refresh stages a new root while the old root remains recoverable, keeps home attached, boots and verifies the guest, and deletes the old root only after verification succeeds. A workspace that was stopped is temporarily booted for verification and stopped again afterward.
- Refresh writes an atomic transaction journal beside the Cloud-Init snippets. On failure it rolls back to the original root; if recovery itself is interrupted, the next `refresh` detects the journal and offers recovery instead of starting another replacement.
- Migration is offline, copies cloud-init snippets before shutdown, preserves home, and restores the previous power state.
- Cleanup removes only verified HomeStack volumes and exact stale references; unrelated `unusedN` entries and volumes are untouched.
- Destroy re-resolves the workspace role and persistent-home identity before deleting the VM and attached home.

Gold's source root name is discovered from its configured `scsi0`; Gold itself does not need the workspace root naming convention. Refresh imports that source as a staged unused disk, preserves the VM configuration, MAC, role tag, Cloud-Init disk, and persistent home, and waits for QEMU Guest Agent commands to report explicit process completion before accepting their results. The full Gold preparation and readiness contract is described above.

## Workspace SSH and synchronization

After a successful create, HomeStack writes and fsyncs a mode-`0600` temporary workspace entry below `~/.ssh/config.d/homestack/`, atomically publishes it, and only then removes stale entries for the same VMID. A publication failure therefore preserves the previous working alias. If this local step fails after VM verification, HomeStack reports that the VM was created successfully and does not roll it back. Known-host removal is likewise narrow.

Synchronization is explicit and never runs as part of create, refresh, or migration. `sync` is a Files-only shortcut through the setup engine. With no selectors it selects all configured Files; `sync WORKSPACE files=ID,ID` narrows the selection. Legacy `[sync] paths` remain selectable Files; an ending slash denotes a directory. Legacy `[sync] commands` are selectable Applications under `setup` and **never run under sync**. `[sync] verbose = true` reports additional file preparation, transfer and verification phases without printing file contents.

One SSH ControlMaster connection is established and reused for checks, directory creation, rsync, verification, and all selected setup handlers. Fresh authentication methods are disabled on child connections, so a broken control socket fails instead of requesting another hardware-key interaction. HomeStack does not use password authentication, a fallback key, `sudo`, or credentials stored on PVE hosts.

## Unified workspace setup

`setup` is an explicit user-space operation, independent of `install`, `create`, `refresh`, and `migrate`. It runs on the trusted desktop, resolves the exact workspace through verified Herdr/PVE infrastructure, and operates as the configured unprivileged guest account. Nothing is selected automatically. Applications and repositories do not require selecting Files or copying GitHub authentication.

```bash
# Desktop catalog discovery: no Herdr or guest SSH connection.
uv run homestack setup list
uv run homestack setup list --json

# Read-only live state inspection of one workspace.
uv run homestack setup status hermes
uv run homestack setup status hermes --json

# Full-screen selection. One SSH ControlMaster is authenticated before the TUI
# and remains open until setup exits.
uv run homestack setup hermes

# Numeric selections refer to the last displayed compatible catalog.
# Check the actual indices in your listing; these numbers are illustrative.
uv run homestack setup hermes f=1 e=1 a=1,2

# Use the catalog identifier printed by setup list for concurrent sessions.
uv run homestack setup hermes env=1 app=1 --catalog CATALOG_ID --dry-run

# Stable IDs are preferable for scripts; repository identity is explicit.
uv run homestack setup hermes env=bash app=codex,opencode --yes
uv run homestack setup hermes repo=hipotures/homestack --yes
uv run homestack setup 200 env=bash app=codex --dry-run
uv run homestack setup hermes env=bash app=codex --yes --non-interactive --json
```

The groups are **Files** (`files`, `f`), **Environment** (`env`, `e`), **Applications** (`app`, `a`), and **Repositories** (`repo`, `r`). Select comma-separated one-based indices or stable IDs. `0` or `all` selects the entire explicitly named category and cannot be mixed with other values. Omitted groups select nothing. Repeating an equivalent assignment deduplicates it; conflicting repeated assignments, unknown IDs and invalid indices are errors. `--yes` accepts only the HomeStack plan; it neither selects actions nor answers installer questions.

An explicit VMID or exact workspace name is always required for execution and for `setup status`. With selectors, HomeStack displays a plain plan and confirmation. Without selectors, an interactive terminal opens the Textual interface; non-TTY execution reports an actionable error. `setup status WORKSPACE` opens one read-only guest SSH session, verifies the workspace identity and persistent home, reports detected setup state, then closes the session without writes. Without `--yes`, JSON/non-TTY/`--non-interactive` invocations return a plan with `confirmation_required: true` and exit 3. `--dry-run` performs local validation and read-only target resolution, but no guest SSH, transfers, installer commands or GitHub deploy-key writes.

`--non-interactive` and JSON execution disallow interactive installers unless a configured, verified `non_interactive` recipe exists. They do not bypass hardware authentication: the configured security key may still require touch. HomeStack never switches identities to make an unattended run work. JSON stdout contains only the result; progress goes to stderr. Captured installer commands/output and onboarding transcripts are not put in results or logs. Without a configured installation check, a successful command is reported with installation state still unknown.

### Catalog identity and numbering

Listings include category index, stable ID, label, file path or repository identity, desktop availability, and available repository timestamps. They never claim to know guest installation state. Repository discovery uses authenticated desktop `gh api` with all pages, including accessible private repositories belonging to `[repo] owner`. Discovery failures remain visible and do not disable unrelated groups. GitHub host is explicitly `github.com`, matching the existing repository provisioning service.

```toml
[repo]
owner = "example-owner"
checkout_root = "~/DEV"
sort = "recent" # recent (default), created, or name
```

`recent` sorts descending by the later of creation and push time; missing push dates use creation time. Full repository name breaks ties deterministically. Explicit multi-repository selection is supported and is unrelated to the workspace name.

Each displayed catalog saves a version-1 immutable metadata snapshot below `$XDG_STATE_HOME/homestack/catalogs/` (default `~/.local/state/homestack/catalogs/`). It is scoped to the resolved configuration path and catalog-definition fingerprint. Repository mappings additionally bind the GitHub host/account/account ID, configured owner, checkout root and sort. Snapshots store IDs and repository identities, not commands, tokens or file contents. Files/Environment/Applications-only numeric execution does not require GitHub access. Missing/incompatible snapshots instruct you to list again. An explicit `--catalog ID` keeps using that immutable mapping after later listings; a new push never silently changes an old numeric selection. Selected repository identity and administration permission are rechecked before application.

### Terminal controls

Interactive setup authenticates one workspace SSH ControlMaster **before** the TUI is shown. The connection remains owned by that `setup` process and is reused for state inspection, review preflight, Files, Environment, Applications, Repositories, and `F5` state refreshes. It closes when the TUI exits. Child SSH/rsync sessions cannot authenticate independently, so loss of the master fails closed instead of prompting for another key touch. `setup list` remains targetless and never opens guest SSH.

The checkbox tree is selection state for the current TUI session, not installation state. The tree stays on the left and the full, independently scrollable read-only details pane stays on the right. The target/status header and filter sit above them, with one single-line shortcut footer below. Repository branches start collapsed. Installed/configured/in-sync entries detected during the current state inspection are green. Selecting an already-ready application, or selecting an entry that will overwrite existing configuration, makes its label red as an explicit update/reapply warning. An Environment profile whose configuration and recorded file hashes match shows `NO CHANGES` and stays green when selected. Files modified since the last apply are listed in details; a profile whose HomeStack blocks still match shows yellow `MODIFIED`, turns red when selected for review, and preserves those user edits; review describes verification with writes only if differences are found. Successful actions are deselected after execution and the guest state is inspected again through the same SSH master.

Up/Down moves through the tree, Left/Right collapses/expands, Space toggles an item or branch, and `0` selects the current branch. Clicking a checkbox toggles it; clicking a name only highlights it; clicking a disclosure arrow only expands/collapses. `/` focuses the filter and bulk selection with a filter affects **visible matches only**. The header reports hidden selections only when the filter hides selected items; those items also appear in review. Tab/Shift+Tab navigate focus; PageUp/PageDown and the mouse wheel scroll the details pane. `F5` refreshes live guest state without applying changes. `Ctrl+R` reloads the catalog and then refreshes state. The details pane always shows complete item information, with long text and commands wrapped to its width; highlighting another item scrolls it back to the top. Custom command payloads remain redacted. Enter on the main setup screen opens **Review & apply** for the current selections; it does nothing beyond a notification when nothing is selected.

The single-line footer uses plain colored keys without boxes. Its actions support mouse clicks and Tab/Shift+Tab focus; on narrow terminals, focusing an action scrolls it into view. Enter activates the focused control. In review, **Apply this plan** starts focused and requires a separate Enter or click to confirm before any writes; Esc or **Back** returns without applying. Only the main footer’s `Enter` key blinks yellow/dim once per second while any session selections are pending, including hidden selections. It stops when selections are empty or setup is running; Review is inactive while running. Review has one centered action row: `Esc Back   Enter Apply this plan`, with each key on a gray background and its dim description alongside. Execution returns to the main setup screen with refreshed state and a result notification.


### Catalog configuration and migration

Older configurations receive built-in Bash (`bash`), Zsh (`zsh`), Fish (`fish`), Nushell (`nu`), Codex (`codex`), OpenCode (`opencode`), and Hermes Agent (`hermes`) definitions in memory. Presence never selects or executes them. `config.example.toml` contains the complete neutral catalog.

Use `[[setup.items]]` to customize by stable ID. Existing definitions merge field by field, once per ID; duplicate definitions in one document are rejected. Changing an application's command clears inherited installation checks and unattended safety declarations. Explicitly supply the new command's `interaction`, `check`, and optional `non_interactive` recipe. Supported handler parameters are validated; unknown fields are errors. Application interpreters are Bash or Zsh so pipeline failures propagate. Group labels/descriptions live separately in `[[setup.groups]]`; additional groups can reuse existing handler types without renderer changes.

```toml
[[setup.items]]
id = "notes"
group = "files"
handler = "file"
label = "Notes directory"
description = "Copy desktop notes into persistent home without deleting guest files."
path = "~/notes/"

[[setup.items]]
id = "codex"
label = "Codex CLI"
# Other built-in fields remain available; override command only deliberately.

# A custom application requires group, handler, label, description and command.
# Optional: interpreter, interaction, prerequisites, prerequisite_checks, check,
# non_interactive, bin_dirs, requires_absent, backup_paths, depends_on.
```

`depends_on = ["ID"]` requires those actions to be explicitly selected and displays their order; it never silently selects credentials or applications. Cycles and missing dependencies block the plan.

Legacy `[sync] paths` become Files with stable `legacy-file-<hash>` IDs. Legacy `[sync] commands` become Applications while retaining the exact payload, including whitespace. Exact matches to existing application commands (including the known Codex non-interactive recipe) deduplicate safely. Arbitrary legacy commands get stable `legacy-app-<hash>` IDs and conservative interactive classification; no unattended flags are inferred. They never execute through `sync`.

Migration is explicit: run `setup list`, identify each legacy entry, add a `[[setup.items]]` definition with the displayed ID and original path/command, supply a short label/description and verified application interaction/checks, then remove that entry from `[sync]`. Review the proposed TOML yourself; HomeStack does not rewrite live configuration on load/list/setup. The runtime serializer and install reconfiguration/draft-resume paths retain the new fields. Configuration publication retains existing atomic writes and backups. Declining optional sync reconfiguration preserves existing entries.

### Execution and safety

For interactive `setup`, one private, owned ControlMaster remains alive from the initial guest-state inspection until the TUI exits, including repeated review/apply cycles. Explicit selector execution and `setup status` own one master for that single command. Every child SSH/rsync/PTY session requires the existing master and disables fresh authentication; losing the master fails closed. Socket cleanup runs on normal completion, errors and cancellation. Before reads that are trusted as workspace state, and again before writes, HomeStack verifies hostname, account, UID/GID, expected home, and the dedicated ext4 mount labeled `HS_HOME_<VMID>`.

All selected actions are preflighted before any mutates state. Environment normally precedes Files, Applications and Repositories; explicit selected dependencies take precedence. Overlapping writes (including Files versus startup profiles and colliding checkout/key paths) are rejected. A blocked preflight leaves the plan unapplied. Execution stops on failure, retains successful outcomes and marks remaining items `not-run`; results use `succeeded`, `already-ready`, `failed`, `blocked`, and `not-run`. Overall failure exits 1. Third-party installers are not transactional and are never blindly retried.

Files retain home-relative rsync semantics and never delete unselected destinations. Only selected source types/trees are validated for execution. Source/destination symlinks, special files and unsafe ancestors are rejected. Transfers do not preserve desktop ownership; guest ownership is verified and permissions restrict access to the configured user. File content is never printed. No SSH identity, authentication configuration, application credential or key rotation is implicitly selected.

Environment profiles repair minimal homes with managed blocks and atomic writes. They preserve custom and installer-added lines, avoid repeated PATH entries, and do not change the login shell. Bash does not copy a stock dotfile set: it computes the desired HomeStack blocks, updates `.bashrc`, and updates the first existing login file in Bash precedence order (`.bash_profile`, `.bash_login`, `.profile`), defaulting to `.profile` when none exists. Zsh initializes its built-in completion; Fish uses its native startup syntax; Nushell updates `env.nu` and `config.nu`. Ambiguous startup source cycles, malformed blocks and symlinks block reconciliation. Shell syntax is checked before writing using the selected binary. Missing shell binaries are reported for separate Gold preparation/refresh. Per-file sidecar `.homestack-backup-*` files are not created.

Application execution has explicit interpreter, HOME, working directory and PATH regardless of Environment selection. User bin directories are declarative. Selecting an application whose configured live check already passes is an explicit update/reapply request; HomeStack runs the selected installer instead of silently skipping it. `check` is recipe-specific and is used for live detection/verification only; command output and application version strings are not stored in workspace state. Optional `backup_paths` declare user-home configuration that must be snapshotted before an application update. Empty `backup_paths` are allowed but the interactive review warns that arbitrary third-party installer changes outside HomeStack-managed files cannot be restored. The default recipes follow [Codex installation](https://developers.openai.com/codex/cli/), [OpenCode installation](https://opencode.ai/docs/), and [Hermes installation](https://hermes-agent.nousresearch.com/docs/getting-started/installation/) and its [official stage installer](https://github.com/NousResearch/hermes-agent/blob/main/scripts/install.sh). Codex preserves the known `CODEX_NON_INTERACTIVE=1` recipe. OpenCode is non-interactive and uses `~/.opencode/bin`. Hermes uses the official repository/venv/python-deps/node-deps/path/config/complete stages, omitting system-package preparation, browser/computer-use installation, desktop, gateway and provider onboarding. Its conservative recipe requires a working supported Node/npm and compiler toolchain from Gold, preventing the upstream Node bootstrap's system-library installation path. Existing incomplete Hermes source installations block automatic replacement. Version checks establish installed software only; provider readiness remains separate.

Guest `python3` and `findmnt` support identity/path/atomic-write verification. Files alone require desktop/guest rsync. Repository actions require guest git/ssh/ssh-keygen and authenticated desktop GitHub administration access; desktop tokens stay on the desktop. Missing prerequisites never trigger Gold edits, privileged fallbacks, package installation, lifecycle operations or automatic credential copies. Repositories reuse isolated checkout-local SSH configuration and existing keys; setup never pulls, resets, cleans, rotates keys, runs project installation scripts, or discards dirty worktrees. Incomplete/read-only deploy-key repair is left to explicit standalone repository maintenance.

### Workspace setup state and snapshots

Setup state lives with the persistent home, not on the trusted desktop, at `~/.local/state/homestack/setup.json`. This follows the XDG state-home convention while deliberately keeping the HomeStack state below the workspace persistent home. Deleting the workspace and its persistent home therefore deletes its setup state as well; recreating a VM with the same name does not inherit stale desktop-side state. Merely inspecting a workspace does not create the file.

The registry records stable setup IDs, handler type, first/last HomeStack application timestamps, and the current SHA-256/size/modification metadata for files that HomeStack directly manages. Application records contain management/install timestamps only; they do not persist `--version` output. The registry is not the sole source of truth: `setup status` and interactive startup also perform live checks. A manually installed application can therefore be detected as installed even when no HomeStack record exists, while a recorded item that no longer passes its live check is not reported as ready.

Before an approved plan overwrites an existing declared path, HomeStack creates one operation snapshot below `~/.local/state/homestack/YYYYMMDD_HHMMSS/` (with a collision suffix when needed). The snapshot preserves the complete pre-change files/directories under `home/`, stores their SHA-256 and modification metadata in `snapshot.json`, and copies the previous registry as `setup.before.json` when it exists. All snapshot candidates are prepared before the first selected write; snapshot failure blocks the plan before mutation. Files that did not exist are represented in snapshot metadata but need no payload copy. Reapplying an already-matching Environment profile does not write shell files or create a snapshot; its result explicitly reports that no files changed. Changes outside HomeStack-managed blocks are preserved and do not by themselves require reconciliation. The state registry retains the last snapshot reference when a later apply creates no new snapshot. Snapshots are configuration/data backups, not a promise of full rollback for arbitrary third-party installers.


## Repository provisioning

Repository setup is explicit and independent of create, refresh, migrate, and sync. Configure the default GitHub owner and checkout root:

```toml
[repo]
owner = "owner"
checkout_root = "~/DEV"
```

Run either form:

```bash
uv run homestack repo WORKSPACE
uv run homestack repo WORKSPACE OWNER/REPO
```

Without an explicit repository, HomeStack resolves the workspace first and uses `[repo] owner` plus the actual workspace name: `homestack repo WORKSPACE` means `OWNER/WORKSPACE`. This also applies to numeric VMID targets, so `homestack repo 200` uses the resolved workspace name rather than `OWNER/200`. An explicit `OWNER/REPO` overrides this default. If neither an explicit repository nor `[repo] owner` is available, the command fails without making changes. The command first inspects the workspace and GitHub state, then offers `Exit`, `Setup`, or `Rotate key`.

Setup creates one Ed25519 deploy-key pair below `~/.ssh/homestack/github/` inside the workspace persistent home. The private key never leaves the workspace. HomeStack reads only the public key over the existing hardware-authenticated workspace SSH connection and registers it through the trusted desktop's authenticated `gh` session as a read-write GitHub deploy key.

The checkout lives at `<checkout_root>/<repo-name>`. If it does not exist, HomeStack clones it over SSH. If the correct repository already exists, HomeStack preserves the working tree and history, changes only the remote/checkout SSH configuration when necessary, and never performs `git reset`, `git clean`, or a destructive reclone. Re-running Setup on a fully configured repository is a no-op. If the GitHub deploy key disappeared but the local key pair remains valid, Setup re-registers the existing public key.

Rotate key replaces only the repository deploy key. It removes the matching GitHub deploy key, generates a new workspace-local key pair at the same path, registers the new public key as read-write, and verifies Git access. The checkout and working tree are not modified.

## Status

Global status discovers tagged workspaces cluster-wide, checks configured storage capacity, and inventories only HomeStack-named detached volumes on storage IDs allowed by `storage_layouts`. It reads the Proxmox cluster-node state first: node-specific VM and storage queries are skipped for nodes reported offline, and their offline state is shown instead of producing proxy SSH connection warnings. Lifecycle preflight remains fail-closed when an operation requires another node. Detailed status includes VM, network, guest-agent, persistent-home, SSH readiness, and configured volume information. Presentation and human-readable formatting live in `ui.py`; `--json` returns the underlying result structures.

## Development

The project uses a standard `src/` package layout. Run the complete suite with:

```bash
uv run python -m unittest discover -s tests -v
```
