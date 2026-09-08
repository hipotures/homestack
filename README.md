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

The desktop also needs `herdr`, `ssh`, and `rsync`. Proxmox nodes need `qm`, `pvesh`, `pvesm`, `perl`, `base64`, `ssh`, and `scp`.

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

The installer uses this dedicated `PVE` workspace for read-only discovery, selects a verified root Herdr session, then asks only for configuration choices that cannot be discovered safely. It discovers workspace network profiles from existing HomeStack VMs and read-only PVE network/DNS data on the Gold VM's inherited bridge, lets the user choose when more than one profile is available, and asks only for missing network fields. Hardware-backed SSH identities are selected by number instead of being repeated as a long comma-separated path default. An existing runtime config is never silently overwritten; reconfiguration is explicit and the previous file is backed up before atomic replacement.

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

Commands use unique begin/end envelopes and the transport parses results from the pane. `debug = true` retains command output in Herdr scrollback; `false` clears it after parsing. HomeStack stores no Proxmox token or private credential on a PVE host and never adds password, fallback-key, or `sudo` authentication paths.

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

Current HomeStack guest initialization targets a systemd Linux guest using Cloud-Init and NetworkManager. For Debian/Ubuntu-family Gold images, install the equivalent of:

```bash
apt-get update
apt-get install -y cloud-init qemu-guest-agent network-manager openssh-server e2fsprogs util-linux rsync
```

Run administrative preparation as `root`; HomeStack does not use `sudo`. `rsync` is required inside workspaces when `homestack sync` is used.

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

After Gold selection and workspace-account selection, `homestack install` performs a non-destructive readiness check. It validates the PVE role tag, root/data-disk layout, `net0`, Cloud-Init drive, QEMU Guest Agent option, and boot order. If Gold is running, it also checks QEMU Guest Agent access, required guest tools, the configured user/UID/GID, the no-`sudo` policy, regular-user policy, root SSH keys, and a workspace public-key source.

The installer never starts Gold just to inspect it. A stopped Gold can therefore pass the PVE-side contract when it has valid Proxmox `sshkeys`, but the installer reports guest checks as not inspected. Runtime `create` and `refresh` verification still fail closed if the resulting workspace violates the account, SSH, persistent-home, or guest requirements.

## Storage and lifecycle invariants

- Gold is the configured `gold_vmid` and must have the exact `homestack-gold` tag.
- Workspaces have the exact `homestack-ws` role tag.
- `scsi0` is the disposable root; `scsi1` is persistent home.
- Persistent home is ext4 with label `HS_HOME_<VMID>`.
- Workspace volumes are named `vm-<VMID>-hs-root-default` and `vm-<VMID>-hs-home-<USER>`.
- Create placement and migration targets must belong to the node's configured `storage_layouts` list.
- Refresh replaces only the disposable root, keeps home attached, rechecks state before destructive work, and restores the previous power state.
- Migration is offline, copies cloud-init snippets before shutdown, preserves home, and restores the previous power state.
- Cleanup removes only verified HomeStack volumes and exact stale references; unrelated `unusedN` entries and volumes are untouched.
- Destroy re-resolves the workspace role and persistent-home identity before deleting the VM and attached home.

Gold's source root name is discovered from its configured `scsi0`; Gold itself does not need the workspace root naming convention. Refresh clones that source into the existing VM and preserves its configuration, MAC, role tag, Cloud-Init disk, and persistent home. The full Gold preparation and readiness contract is described above.

## Workspace SSH and synchronization

After a successful create, HomeStack writes and fsyncs a mode-`0600` temporary workspace entry below `~/.ssh/config.d/homestack/`, atomically publishes it, and only then removes stale entries for the same VMID. A publication failure therefore preserves the previous working alias. If this local step fails after VM verification, HomeStack reports that the VM was created successfully and does not roll it back. Known-host removal is likewise narrow.

Synchronization is explicit and never runs as part of create, refresh, or migration. `[sync] paths` contains normalized `~/...` desktop paths; an ending slash denotes a directory. Optional commands run in order as the workspace user only after all configured paths synchronize successfully.

One SSH ControlMaster connection is established and reused for checks, directory creation, rsync, verification, and post-sync commands. Fresh authentication methods are disabled on child connections, so a broken control socket fails instead of requesting another hardware-key interaction. HomeStack does not use password authentication, a fallback key, `sudo`, or credentials stored on PVE hosts.

## Status

Global status discovers tagged workspaces cluster-wide, checks configured storage capacity, and inventories only HomeStack-named detached volumes on storage IDs allowed by `storage_layouts`. It reads the Proxmox cluster-node state first: node-specific VM and storage queries are skipped for nodes reported offline, and their offline state is shown instead of producing proxy SSH connection warnings. Lifecycle preflight remains fail-closed when an operation requires another node. Detailed status includes VM, network, guest-agent, persistent-home, SSH readiness, and configured volume information. Presentation and human-readable formatting live in `ui.py`; `--json` returns the underlying result structures.

## Development

The project uses a standard `src/` package layout. Run the complete suite with:

```bash
uv run python -m unittest discover -s tests -v
```
