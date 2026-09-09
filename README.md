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

The desktop also needs `herdr`, `ssh`, and `rsync`. GitHub repository provisioning additionally requires an authenticated `gh` CLI on the trusted desktop. Proxmox nodes need `qm`, `pvesh`, `pvesm`, `perl`, `base64`, `ssh`, and `scp`.

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

Lifecycle targets may be a numeric VMID or exact workspace name. `create`, `refresh`, `migrate`, `sync`, and `destroy` resolve and display a plan before confirmation. Use `--yes` for non-interactive execution and `--json` for machine-readable plans/results.

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

Synchronization is explicit and never runs as part of create, refresh, or migration. `[sync] paths` contains normalized `~/...` desktop paths; an ending slash denotes a directory. Optional commands run in order as the workspace user only after all configured paths synchronize successfully.

One SSH ControlMaster connection is established and reused for checks, directory creation, rsync, verification, and post-sync commands. Fresh authentication methods are disabled on child connections, so a broken control socket fails instead of requesting another hardware-key interaction. HomeStack does not use password authentication, a fallback key, `sudo`, or credentials stored on PVE hosts.

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
