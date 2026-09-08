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

The installer uses the same read-only discovery mechanism first, selects a verified root Herdr session, then asks only for configuration choices that cannot be discovered safely. An existing runtime config is never silently overwritten; reconfiguration is explicit and the previous file is backed up before atomic replacement.

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
workspace = "example-workspace"
tab = "example-node"
debug = true
```

Herdr is the only implemented backend. A small transport factory owns backend setup and verification, while the transport protocol exposes remote execution and generic execution metadata. Lifecycle, status, and Proxmox operations do not depend on Herdr pane internals, so another backend can be added later without rewriting their orchestration. Direct SSH is intentionally not implemented.

## Trust and transport model

HomeStack remains a trusted-desktop tool. It drives one already authenticated SSH root shell in the configured Herdr workspace, tab, and pane. Before remote work it verifies that:

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

Gold's source root name is discovered from its configured `scsi0`; Gold itself does not need the workspace root naming convention. Refresh clones that source into the existing VM and preserves its configuration, MAC, role tag, cloud-init disk, and persistent home.

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
