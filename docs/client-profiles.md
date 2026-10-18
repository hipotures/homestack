# Client profiles

Select a profile before opening the TUI or running a command:

```sh
homestack                         # default
homestack --profil default        # the same desktop configuration
homestack --profil e2e            # the active disposable lab
homestack --profil p2p            # a separate persistent environment
homestack --profil p2p status --json
homestack profiles                # names and storage locations
```

`--profile` and `--profil` select the same setting. The selector also works
after a subcommand, for example `homestack status --profil p2p`. No selector
means `default`; the last selected profile never changes this default. The TUI
shows the active profile in its main screen and installer.

## Storage

Profile `default` is the existing desktop environment. Its client connections
are in `$XDG_CONFIG_HOME/homestack/client.toml`, or
`~/.config/homestack/client.toml` when XDG is unset. `[server].urls` lists the
HomeStack API endpoints; `token_file` and optional `ca_file` name credential and
trust files. Install inputs are in `config.toml`, deployment settings in
`deploy.toml`, and preferences in `tui.toml`, alongside `client.toml`.
Generated workspace SSH configs and per-VM host keys live in
`~/.ssh/config.d/homestack/`. Selecting `default` uses these paths directly.

Other persistent profiles have a private root under
`$XDG_CONFIG_HOME/homestack/profiles/NAME/`, or
`~/.config/homestack/profiles/NAME/`:

| Path inside the profile | Contents |
| --- | --- |
| `config/homestack/` | Client endpoints, tokens, install and deployment settings, certificates, preferences, TUI log |
| `home/.ssh/` | Profile SSH keys and configuration, workspace configs and host keys |
| `state/` | Setup catalog snapshots and other XDG state |
| `cache/` | XDG cache |
| `bin/` | Profile SSH wrapper and desktop Herdr entry point |

Opening a new name such as `p2p` creates empty private directories and starts
the installer. It does not copy another profile's endpoints, tokens or keys.
The first installation asks for the workspace network, user name and numeric
UID/GID, root and home disk slots, default home size and snippet directory,
alongside the existing cluster resource choices. No `config.toml` is required
before starting. A supplied `config.toml` only supplies initial answers.
The review validates the answers before any cluster change; confirmation
saves the installation settings, `server.toml`, `deploy.toml` and `client.toml`
inside the selected profile. Typed answers are retained in the installation
draft and survive a cluster rescan.

After a successful production uninstall, the active `client.toml` is archived
as `client.toml.uninstalled`. Reopening that profile starts the installer;
installation settings and deployment files remain available. A failed or
cancelled uninstall leaves the active client configuration intact. An API
connection failure alone does not change installation state.
A retained tagged API container can be selected for adoption into the service
pool and reused by the same installation plan.

An installed profile opens its normal status view. Menu **Reinstall HomeStack**
opens the full installation form with its saved settings. Menu **Update API
containers** uses the instances, credentials and network in `deploy.toml`,
asks for an image and confirmation, and updates the containers without root
access or repeating Proxmox account, role and pool installation. This action
also works when the HomeStack API is unreachable, provided the saved Proxmox
deployment is reachable. Enter on a server row updates that container, or the
marked containers.

The directories have mode `0700`; generated secrets and configuration have
mode `0600`. Names use lowercase ASCII letters, digits, `_` and `-`, start
with a letter or digit, and contain at most 64 characters.

The process receives profile-specific `HOME` and XDG roots before any client
configuration is loaded. OpenSSH is forced to use the profile's SSH config;
it does not inherit the desktop's config, default private keys, agent or
global known-hosts file. Workspace entries contain absolute key and host-key
paths because OpenSSH expands `~` independently of the child's `HOME`.
Put explicitly chosen keys and host entries in the profile's `home/.ssh/`.
Herdr remains available in every profile. Its entry point uses the desktop's
existing Herdr configuration and running session while HomeStack keeps the
profile's own configuration, tokens and SSH files. The install and uninstall
screens let the owner choose SSH or a Herdr pane. A failed Herdr tab lookup
is shown directly on the access screen.

`--config` cannot override a named profile. Edit that profile's files instead.
A missing or invalid configuration produces an error or the first-install
screen; the client never searches another profile. These are application data
boundaries within one Unix account; explicitly configured external paths still
use that account's filesystem permissions.

## Profile e2e

The E2E controller registers `e2e` when it pauses after preparing HomeStack.
Open it without supplying a run ID or artifact directory:

```sh
cargo build --locked -p hs-cli -p hs-e2e
./target/debug/homestack --profil e2e
./target/debug/homestack --profil e2e status --json
```

The registration at `profiles/e2e/run.toml` contains only an absolute artifact
path and run ID. Actual configuration, tokens and keys remain in that run's
private `client/` directory and are removed with the VM. This profile also
uses the lab's pinned SSH identity, isolated SSH aliases and a client-owned
SOCKS tunnel. Closing the client leaves the paused VM running.

An already running controller can be registered once without restarting it:

```sh
./target/debug/homestack-e2e profile --artifacts target/e2e/RUN_ID
./target/debug/homestack --profil e2e
```

`e2e` requires a matching paused run and a reachable control socket. A missing,
stopped or replaced run produces an error before connecting. It never becomes
an empty generic profile and never opens `default`. Another live paused lab
cannot replace the binding. After the old lab stops, the next prepared paused
run can replace it. The existing stopped-run reference is harmless metadata
and remains available for identifying its artifacts.

The two executables must be installed beside each other on the runner. Remote
access and the VM lifecycle remain described in [E2E bootstrap](e2e-bootstrap.md).
