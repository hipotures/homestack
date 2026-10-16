# Disposable Proxmox bootstrap

`hs-e2e` implements the first executable part of the
[isolated E2E laboratory design](isolated-e2e-lab.md): download a Debian qcow2,
initialize a VM, install Proxmox, prepare the HomeStack install inputs, verify
the lab, power it off and delete it. The HomeStack container installation is
confirmed manually in the real TUI; Gold/workspace fixtures remain later work.

The runner prepares `pve.e2e.homestack.app` through the existing Spaceship and
ACME implementation in `hs-deploy`. It installs a public Let's Encrypt
certificate in PVE and prepares `api1.e2e.homestack.app` at `10.77.0.101`.
The HomeStack phase obtains or reuses `*.e2e.homestack.app`, installs it in
PVE for both its browser alias and internal node name, and prepares a separate
client profile. DNS and certificate fixtures persist across runs.
Normal cleanup removes the VM; `cleanup --all` also resets owned fixtures.

## Run

Use a Linux x86_64 host with accessible `/dev/kvm`, nested virtualization enabled,
and these commands on `PATH`:

- `qemu-system-x86_64` and `qemu-img`;
- `ssh` and `ssh-keygen`;
- `mkfs.vfat` from dosfstools and `mcopy` from mtools;
- `openssl`; also `dig` and `nsupdate` when using local split DNS.

QEMU also needs its normal firmware and user-networking dependencies. For
example, on Debian/Ubuntu the packages are `qemu-system-x86`, `qemu-utils`,
`dosfstools`, `mtools` and `openssh-client`. No root privileges are required to
run the controller if the invoking user can open `/dev/kvm`.

From the repository root:

```sh
cargo run --locked -p hs-e2e -- run
```

The existing Spaceship credentials are read from
`$XDG_CONFIG_HOME/homestack/spaceship-dns` (default
`~/.config/homestack/spaceship-dns`). Override with `--spaceship-file FILE`.
Credentials remain on the controller and are never copied into the guest.

For the locally extracted QEMU cache used to validate this workspace:

```sh
PATH="$PWD/target/e2e-tools/bin:$PATH" cargo run --locked -p hs-e2e -- run \
  --local-dns 192.168.100.1 --pause-after verify
```

This network has a private `homestack.app` zone on the router. The
`--local-dns` option creates the exact PVE and API E2E A records there using
`~/.config/homestack/dns-tsig.key` (override with `--tsig-file FILE`), while
Spaceship serves the public A record and ACME TXT challenge. It does not edit
other names in either zone. Networks without a private zone omit this option.

That ignored cache is a local tool installation, not part of a fresh checkout.
The controller caches the guest image; it does not install host packages.
On every run it fetches the current Debian `SHA512SUMS` manifest over HTTPS
and hashes the complete cached file. A matching image is reused without
downloading it again. A missing, outdated or corrupt image is downloaded to a
temporary file and replaces the cache atomically only after successful hash
verification. Failed or interrupted downloads preserve the previous cache.
The cache keeps one current image in
`$XDG_CACHE_HOME/homestack-e2e/images` (default
`~/.cache/homestack-e2e/images`). A file lock serializes access. Every VM gets
its own base-image copy, so cache replacement or removal cannot affect a
running VM. Normal cleanup and the DNS/TLS `cleanup --all` reset retain this
download cache; remove its qcow2 file explicitly to force another download.
The report records `image.cache_hit`, `image.image_bytes` and
`image.downloaded_bytes` (zero when reused).
During image download, the terminal shows a progress bar, percentage, MiB
downloaded/total and average MiB/s. Without a reported size it shows bytes and
speed; redirected output uses periodic lines instead of terminal redraws.

The runner may execute on a separate Linux server or in a development workspace
managed by a production HomeStack installation. That workspace must expose
usable `/dev/kvm` and enough nested virtualization for this topology. The
production Proxmox and its guests are never test targets. Keep the HomeStack
client and browser on the desktop if desired; inspection does not require
running the controller there.

Supply a message to verify custom initialization data:

```sh
cargo run --locked -p hs-e2e -- run \
  --message "Disposable PVE lifecycle check" \
  --artifacts target/e2e/my-bootstrap
```

The artifact directory must not exist. Default resources are **1 vCPU, 2048 MiB
RAM and a 12 GiB sparse virtual disk**. RAM is the
[Proxmox baseline for its OS and services](https://www.proxmox.com/en/products/proxmox-virtual-environment/requirements);
it does not include capacity for additional guests. The disk consumes host
space as data is written. Provision several GiB of actual free temporary disk
space for the image and installation. Private VM files live under
`target/e2e-work/`, separately from artifacts, and are removed after the run.
This avoids placing virtual disks in a potentially RAM-backed `/tmp`.

Optional limits are `--memory-mib`, `--cpus`, `--disk-gib` and
`--timeout-seconds`. The defaults are the smallest accepted resource allocation.
The active run deadline defaults to 1800 seconds, excluding selected inspection
pauses; final forced process cleanup runs after a timeout or interruption.
There is no software emulation. A run always creates its own VM; `prepare`
can only update the runner's own live, paused lab.

Kernel and PVE package installation show separate **Package download** and
**Package install** progress bars, with the actual percentage and current
action reported by [APT's status stream](https://sources.debian.org/src/apt/3.0.3/doc/progress-reporting.md).
The percentage covers that package operation; reboot, service readiness and
TLS installation have their own messages. Terminal output redraws the bar;
redirected output prints periodic lines. Complete package output, status
messages and errors remain in `install-kernel.log` and `install-pve.log`.


## Installation phases and manual inspection

Normal runs remain automatic. Every phase writes its status, timings and error
to `report.json` before the next phase starts:

| Phase | Available state after success |
|---|---|
| `preflight` | KVM and host tools checked; no VM yet |
| `domain` | E2E DNS and publicly trusted certificate prepared or reused; no VM yet |
| `image` | Latest manifest checked, cached/downloaded image verified and sparse disk prepared; no VM yet |
| `boot` | Debian booted, Cloud-Init and initialization checked; SSH and restricted APT networking work |
| `kernel` | Proxmox kernel installed and the VM rebooted |
| `proxmox` | PVE services installed and rebooted; public certificate installed and temporary web credentials printed |
| `network` | Internal `vmbr0` bridge configured at `10.77.0.1/24`, persisted in the guest network configuration and verified through PVE |
| `homestack` | API DNS, cached public wildcard TLS, isolated install configuration and workspace key, internal DNS, restricted OCI access and a published server image in the native PVE cache; no container created |
| `verify` | PVE kernel, exposed nested KVM, HTTPS API, web page, password login and authenticated node inventory checked |
| `shutdown` | VM powered off; private files still exist until cleanup |
| `cleanup` | QEMU reaped and private disks, keys and web credentials removed |

Request pauses only where manual inspection is useful:

```sh
cargo run --locked -p hs-e2e -- run \
  --pause-after boot,kernel,proxmox,network,homestack,verify \
  --pause-on-failure \
  --artifacts target/e2e/inspection
```

Each successful selected phase pauses **before the next phase**. A paused VM
continues running. The runner prints an SSH command and the private
`access.json` path once the VM has started; after PVE installation that file
contains the panel URL, login `root@pam`, generated password and SSH details.
The runner also prints the panel URL, username `root`, realm **Linux PAM** and
the generated password directly to the terminal after PVE installation. The
password is not added to `report.json` or the runner's artifact files; capturing
the terminal output also captures this temporary password. Its private file is
mode `0600` inside the run directory and is removed during cleanup. SSH
password and keyboard-interactive login stay disabled.

Resume from another terminal on the **runner host**:

```sh
cargo run --locked -p hs-e2e -- resume --artifacts target/e2e/inspection
```

Resume uses a private Unix socket; it does not adopt a VM, signal a stored PID
or restart the installation. A failed phase pauses when `--pause-on-failure`
is selected. Resuming that failure continues to cleanup and preserves the
original failed result. A later attempt starts a fresh VM.

Inspection pauses have **no time limit**. The VM stays available until you
explicitly resume or stop the controller. Inspection time does not consume the
active installation budget set by `--timeout-seconds`. Ctrl-C or SIGTERM on
the controller triggers cleanup. Keep the controller running during inspection.
Socket files are also removed on cancellation. Cleanup itself cannot be paused.
A controller that was already running before an executable rebuild continues
using its loaded code, including any inspection timer from that older build.

## Browser and SSH access

The panel URL is `https://pve.e2e.homestack.app:<reported-port>`, forwarding
to PVE's TCP 8006. With the default loopback binding, DNS points the hostname
to `127.0.0.1`. Open the printed domain URL: normal browser trust verifies the
public certificate without importing a private CA. PVE's internal cluster CA
remains separate. The runner prints username `root`, realm **Linux PAM** and
the temporary password. PVE requires its login form; passwords are not placed
in URLs.

When the browser runs on the same computer as the controller, open the printed
panel URL directly. No SSH tunnel is needed. A local run prints that direct
instruction and does not suggest a tunnel back to itself.

For a remote runner started over SSH, the controller derives the actual
username, server address and SSH port from the invoking SSH session. It prints
a complete tunnel command for the desktop, with `ExitOnForwardFailure=yes`.
For an unattended runner, or to use an existing client SSH alias, set the
destination explicitly. For this workspace's runner:

```sh
PATH="$PWD/target/e2e-tools/bin:$PATH" \
cargo run --locked -p hs-e2e -- run \
  --local-dns 192.168.100.1 --pause-after verify \
  --runner-ssh user@192.168.1.100
```

Use `--runner-ssh-port` for a nonstandard SSH port; an explicit SSH alias uses
the client's configured port when no override is given. The generated command
contains the real PVE forward port and is also saved in `access.json` as
`remote_tunnel_command`, together with `remote_web_url`.

Run the printed tunnel on the desktop. Then open
`https://pve.e2e.homestack.app:18006` in the desktop browser, where
that hostname must also resolve to loopback. With a nested HomeStack workspace, the tunnel
terminates at the workspace running this controller. Its host address and
authentication belong to the existing desktop SSH configuration.

For direct access over a reachable host interface, explicitly set
`--web-bind 192.168.1.20 --web-port 18006`, replacing the address with one
assigned to the runner. Bind defaults stay on loopback; no host firewall rules,
physical bridges or routes are changed. DNS then points the same hostname to
the selected interface; open the domain URL. Changing an existing fixture's
address requires an explicit `cleanup --all` reset first. The
temporary SSH forward always stays on `127.0.0.1`.

For a shell inside the disposable VM, run the printed command on the runner:

```sh
ssh -F /path/printed/by/the/runner/ssh_config pve-e2e
```

That configuration uses the run's private key and pinned host identity. It is
independent of the user's SSH agent, keys and existing Proxmox configuration.

## Manual HomeStack TUI in the lab

Plain `cargo run -p hs-cli` opens profile `default`, using the desktop's
existing configuration. Open the isolated lab with `homestack --profil e2e`.
See [Client profiles](client-profiles.md) for storage and SSH isolation.

Build both executables, start the runner with `--pause-after verify` and keep
the controller paused. The pause registers profile `e2e` automatically and
prints the client command. In another terminal on the runner host:

```sh
cargo build --locked -p hs-cli -p hs-e2e
./target/debug/homestack --profil e2e
```

An already running controller built before profile support can be registered once
without restarting its VM:

```sh
./target/debug/homestack-e2e profile --artifacts target/e2e/inspection
```

The launcher requires a paused run after HomeStack preparation, its SSH
credentials and a reachable inspection socket. It opens the real `homestack` executable beside
`homestack-e2e`, with a separate `HOME` and XDG configuration, state and cache
inside that run's private `client/` directory. No production client files,
tokens, DNS credentials or preferences are copied. The first launch opens the
install screen without connecting to a configured HomeStack server.

For an uninstalled lab, the launcher passes
`tui --ssh root@pve-e2e --server-domain e2e.homestack.app --server-image <published-image>`
to the client. After installation it opens the normal TUI with that run's
`client.toml`; the production desktop profile remains excluded. **SSH** and **Host `root@pve-e2e`** are prefilled on the access
form; **Server domain `e2e.homestack.app`** is prefilled when the cluster is read.
The values override earlier access defaults for this launch and remain editable.
The launcher's SSH wrapper uses only the run's generated configuration,
identity and pinned host key; the real desktop's SSH config and agent are excluded. Other SSH aliases
fail, and Herdr is disabled to avoid using an existing production root shell.
No `Include` in the desktop's `~/.ssh/config` is required for this client.

Press **F2** on any TUI screen to enter copy mode. The display freezes and
mouse capture is released, so the terminal can select text and copy it with
its normal Copy shortcut. Press **F2** or **Esc** to resume the display.
Background work continues; copy mode does not stop a deployment or the VM.

Reopening `homestack --profil e2e` for the same VM retains its test profile. VM cleanup
removes its private client data together with the VM keys and disks. The named
reference contains only an artifact path and run ID; opening a stopped run fails,
and a subsequent paused run replaces the reference. There is no fallback to
`default`. Historical logs and reports in the artifact directory remain. Closing the TUI leaves the paused
controller and VM running. Explicit resume continues the remaining lifecycle,
which ends in VM cleanup; Ctrl-C or SIGTERM on the controller also removes the VM.

The `homestack` phase prepares the real installation without applying it:

- A fresh, validated `config.toml` describes the actual numeric node `pve1`,
  storage `local`, bridge `vmbr0`, gateway/DNS `10.77.0.1` and workspace user
  `user` (UID/GID 1000). No production configuration is copied. A dedicated
  workspace SSH key is generated under the private client home.
- `api1.e2e.homestack.app` resolves to **`10.77.0.101`**, so the real installer
  derives **CT 101**. The public and optional split-DNS records have their own
  ownership journal in `<fixtures>/service/`.
- The public **`*.e2e.homestack.app`** certificate and its account are cached
  in `<fixtures>/service/acme/`. Only the certificate and key are copied into
  the private client profile and PVE. Spaceship credentials remain on the
  runner. A valid cached certificate makes the installer proceed without
  requesting a DNS API key or issuing another ACME order.
- An internal DNS service on `10.77.0.1` resolves the API and PVE names for
  containers. The server reaches PVE as `pve1.e2e.homestack.app:8006`. The
  installer accepts a wildcard SAN by deriving the numeric node's name and
  still verifies its TLS hostname and trust chain.
  The E2E zone is local to dnsmasq: IPv4-only names return successful empty
  AAAA answers. Returning REFUSED for AAAA makes Alpine's musl resolver reject
  the name despite a valid A record. Bootstrap checks both query types for
  all three service names before continuing.
- The newest published server image is pinned in `service.json` and pulled
  into PVE's native OCI archive cache through `skopeo`. Repeated preparation
  reuses that archive. No HomeStack container is created during preparation.

For an already paused lab, the same preparation can be run explicitly:

```sh
./target/debug/homestack-e2e prepare --artifacts target/e2e/inspection \
  --local-dns 192.168.100.1
```

The command verifies the live inspection socket and VM initialization UUID
before changing anything inside it. Its log is `service-preparation.log`;
non-secret inputs and the selected image are in `service-preparation.json`.
Reopening the TUI is required to load a newly prepared profile.

The client owns an SSH SOCKS tunnel to the VM. Its HTTP(S) proxy environment
routes PVE and HomeStack API calls through SSH, using the internal DNS names
and normal public-CA TLS verification. A second, loopback-only SSH forward
lets the PVE OCI worker reach a registry proxy on the runner. The registry
proxy accepts CONNECT only to `ghcr.io:443` and
`pkg-containers.githubusercontent.com:443`, resolves once and rejects private,
loopback and reserved IPs. It does not terminate or weaken registry TLS.
The image catalog is fetched directly by the runner. Both forwards end when
the client exits; losing the SSH tunnel also stops the client. The VM stays
running in its inspection pause. A second client for the same API fixture is
rejected while the first holds its lock.

The API's private address is reached by the client through this tunnel. A
normal browser has no route to `10.77.0.101`; the PVE browser panel keeps its
reported loopback forward and domain URL.
The Rust installer supports installation before Gold exists. Select the
server's bridge independently in the form; no Gold is fabricated for this
step. Gold is required later for workspace creation and refresh.
The `network` phase creates **`vmbr0`** inside the disposable Proxmox VM, with
address **`10.77.0.1/24`**. Choose it in **Server network bridge**; press **F5**
in the installer's settings after changing the lab network. Configuration is
written to the guest's `/etc/network/interfaces`, enabled with `ifup vmbr0`
and verified against the PVE network inventory. The management NIC retains
its existing SSH and HTTPS access. An existing, unowned `vmbr0` is refused.
The report stores the observed bridge configuration under `network`.

The bridge has no external ports, IPv4 forwarding is disabled and IPv6 is
disabled on the bridge. Container veths and VM taps may attach to it; a
hardware NIC attached to it fails verification. It allows guests to reach
each other and services on the test Proxmox. It provides no Internet or LAN
egress or DHCP. The HomeStack phase adds only the explicit API DNS record
and the tunnels described above; arbitrary guest DNS/egress and browser
forwarding to HomeStack remain later work. Its configuration disappears with the outer
VM; no bridge, route or firewall rule is created on the runner host.

Validated against the paused disposable PVE 9.2.21 VM: with production XDG
paths deliberately inherited, the real TUI opened the install screen,
connected to `root@pve-e2e` and read the single-node inventory. It made no
HomeStack status requests and did not apply an installation. The normal
desktop client configuration, deployment configuration, preferences and SSH
files remained unchanged, and the controller stayed paused after TUI exit.
Unit tests also verify rejected stale runs, SSH alias isolation and the
blocked Herdr route. This does not verify a HomeStack container deployment.

A live TUI check on 2026-10-16 verified that launch arguments prefill the SSH
host and the server domain on the paused lab. F2 released terminal mouse
capture, kept the display unchanged across timer ticks and ignored install
actions until copy mode ended. Returning to the form allowed a read-only
PVE connection, with `vmbr0` selected. Normal desktop client and SSH files
were unchanged, and the VM stayed paused. A private PTY regression test
also verifies that returning from copy mode needs no cursor-position query.


## What a successful run proves

1. Fetch the checksum manifest for the current official
   [Debian 13 genericcloud amd64 qcow2](https://cloud.debian.org/images/cloud/trixie/latest/)
   and verify the cached image's SHA-512 against it. Download the image only
   when the cache is missing or does not match. Record the actual hash, image
   size, downloaded bytes and cache reuse in the report. A downloaded checksum
   mismatch, including a publication change during download, fails the run
   without replacing the previous cached image.
2. Inspect the image format and reject external backing files. Create a private
   writable qcow2 overlay; the downloaded base stays read-only.
3. Generate per-run client and server SSH keys, an instance ID and
   JSON initialization data. Supply them through a NoCloud `CIDATA` FAT disk.
   Password authentication is disabled. Pin the guest SSH host key before
   connecting, rather than accepting a network-discovered key. Set the guest's
   short hostname `pve1`, browser FQDN `pve.e2e.homestack.app` and resolver
   search domain explicitly. The numeric node name matches HomeStack's
   storage-layout naming; the browser alias remains stable.
4. Start QEMU with KVM, host CPU features and local-only SSH/PVE port forwards.
   Wait for SSH and successful Cloud-Init, then read the initialization data
   from the guest and compare it with the requested values.
5. Install the Proxmox kernel from its signed no-subscription APT repository,
   reboot and confirm a new boot ID. Install Proxmox VE services and reboot
   once more. Package installation runs only inside the disposable VM.
6. Verify the running `-pve` kernel, exposed nested KVM device, active PVE
   services and preserved initialization data. Install the cached public
   certificate through pinned SSH using `pvenode cert set`, then verify the
   real HTTPS `/api2/json/version` API using normal public roots and an
   ephemeral guest-local API token. Check that the real web page is
   served, log in through `/api2/json/access/ticket` with the generated
   password, and read the single-node inventory using its session cookie.
   Passwords, API token secrets and session tickets are not artifacts.
   TLS verification stays enabled. The browser page is also fetched using
   normal system DNS, without a resolver override; `web_dns_verified` records
   that check. Administrative requests stay pinned to the disposable VM.
7. Request `systemctl poweroff`, wait for successful QEMU exit, reap the process
   and remove the entire private directory: base image, overlay, initialization
   disk, keys and other temporary files. Confirm removal in the report.

The Proxmox kernel and userspace packages are the current versions in the
configured repositories. This deliberately follows the user's request for
latest inputs; it is not yet the fully locked dependency model proposed for
the eventual regression laboratory.

## Network and cleanup boundary

Persistent fixtures live in `$XDG_STATE_HOME/homestack-e2e` (default
`~/.local/state/homestack-e2e`); `--fixtures DIRECTORY` selects another dedicated
directory. A file lock is held for the entire run, including inspection and
cleanup. The private manifest records exact owned DNS values before mutation.
Conflicting A, AAAA or CNAME records are rejected. Existing matching public
records are reused without claiming ownership. Certificates cover only the
explicit PVE name and the E2E wildcard described above; production certificates
and their cache are not reused or changed.

Each run checks DNS independently of certificate reuse. A certificate is reused
when its explicit SAN and expiry leave at least 30 days, its key matches and
its chain passes normal system trust validation. This creates no new ACME
order or challenge. A missing record is repaired without replacing a valid
certificate. Temporary DNS-01 values are removed after issuance or failure;
a failed cleanup leaves a durable journal for the next run to retry.

The domain phase prints explicit outcomes: DNS `[EXISTING, UNCHANGED]` means
the matching record was already configured and was not modified; `[CREATED]`
means a missing record was added. Certificate `[REUSED]` confirms the existing
cached certificate passed verification without a new ACME order or challenge.
`[REQUESTING NEW]` announces issuance; `[CREATED]` confirms the new certificate
was issued, verified and cached.

To deliberately reset the fixture after all controllers have stopped:

```sh
cargo run --locked -p hs-e2e -- cleanup --all
```

Supply the same `--fixtures` and credential path overrides, if used for `run`.
The manifest supplies the owned local DNS server; local cleanup also uses the
TSIG key. Reset deletes only exact owned public/local A and challenge TXT
values and known ACME cache files. It preserves unrelated records, the domain
registration, provider credentials and TSIG key. The next run provisions the
fixture again. A live controller's lock prevents reset during inspection.

The runner verifies system DNS resolution before boot and again before
printing panel access and checking the browser page. Split DNS, router
rebinding protection or a cached negative reply can prevent browser access;
configure the E2E name on the client resolver as needed. The runner attempts a
bounded refresh through `resolvectl` when available, without changing resolver
settings. It requests type `ANY` because
[systemd-resolved stores NXDOMAIN](https://github.com/systemd/systemd/blob/main/src/resolve/resolved-dns-cache.c)
under that key: refreshing only a positive A entry can leave the old negative
entry behind, causing failure again after the A record expires. On a separate
client, use `resolvectl query --cache=no --type=ANY pve.e2e.homestack.app`.

This initial runner uses QEMU directly. It creates no libvirt domain, physical
bridge, TAP device, host firewall rule or host package installation. Its user
network has `restrict=on` and IPv6 disabled. SSH forwarding binds only to
`127.0.0.1` on an automatically selected port. PVE forwarding defaults to the
same policy; only an explicit `--web-bind` or `--web-port` changes its endpoint.

The only guest-initiated forwarding rule goes to a local read-only APT proxy.
It permits GET/HEAD requests under the Debian and Proxmox repository paths on
`deb.debian.org`, `security.debian.org` and `download.proxmox.com`. The proxy
fetches Debian content over verified HTTPS and uses Proxmox's
[published HTTP APT repository](https://github.com/proxmox/pve-docs/blob/master/pve-package-repos.adoc).
APT verifies signed indexes and package hashes using the Proxmox key fetched
over HTTPS. TLS certificate checks are never disabled. The proxy refuses
redirects, credentials, query strings, other destinations and CONNECT tunnels.
A separate registry proxy is available during HomeStack preparation and
through the active client's loopback SSH reverse forward, with the two exact
GHCR destinations described above. The guest has no general Internet or host
LAN access. See [QEMU's restricted user networking](https://www.qemu.org/docs/master/system/invocation.html).

On an error, active installation deadline, Ctrl-C or SIGTERM, the controller
terminates and reaps its QEMU child before removing its private files. Graceful shutdown and final
cleanup have separate results; a forced kill is not reported as a successful
graceful poweroff. Logs and `report.json` remain in the artifact directory.
SSH keys, API tokens, the initialization disk and VM disks are not artifacts.

SIGKILL, host power loss and a killed controller's orphan recovery are not
handled by this first increment. The independent watchdog and durable VM ownership
journal in the larger design remain required before unattended destructive
testing. A successful check of `/dev/kvm` inside PVE also does not yet prove
that a nested guest boots; that is a later scenario.

## Local checks

```sh
cargo test --locked -p hs-e2e -p hs-deploy
cargo clippy --locked -p hs-e2e -p hs-deploy --all-targets -- -D warnings
cargo fmt --all --check
```

Normal Cargo tests do not download images, boot VMs or contact Proxmox. The
real lifecycle runs only through the explicit `run` command.
An opt-in image-cache check downloads/verifies the current Debian image twice
without starting a VM or accessing DNS credentials:

```sh
cargo test --locked -p hs-e2e live_debian_image_is_reused_after_checksum_verification -- --ignored --nocapture
```

The live cache check passed on 2026-10-16: the first fetch downloaded
341,180,416 bytes, the second fetched a fresh manifest, verified the same
SHA-512 and downloaded zero image bytes. Both private copies were removed;
the verified read-only cache remained.

Validation on 2026-10-16 passed with 1 vCPU, 2048 MiB RAM and a 12 GiB sparse
disk: Proxmox API version `9.2.21`, release `9.2`, kernel `7.0.14-24-pve`,
verified HTTPS, graceful shutdown and complete VM removal. A separate live
SIGTERM test verified boot, pinned SSH, initialization data and cleanup using
the disk-backed working directory.

The phased lifecycle also passed on 2026-10-16: pause/resume after `boot`,
`kernel`, `proxmox` and `verify`, then graceful shutdown and complete removal.
Independent checks verified guest APT networking, pinned SSH, the HTTPS panel
through its `127.0.0.1` forward, four static JS/CSS assets, real PAM password
login and node inventory using the returned session cookie. A separate fresh
boot verified key-only SSH and complete cleanup after SIGTERM during a pause,
including socket removal. A deliberate preflight failure paused for inspection
and retained its original failure after resume and cleanup.

Public TLS validation on 2026-10-16 created only the E2E A records at Spaceship
and the local TSIG-managed zone, obtained an exact PVE certificate through
DNS-01 and removed its challenge TXT value. The certificate's normal trust
chain and installed PVE fingerprint matched. A complete fresh VM run then
passed with `certificate_reused = true`, real HTTPS/PAM login and node inventory,
`graceful_shutdown = true` and complete VM removal; DNS, account and certificate
files remained available for the next run. Full fixture cleanup was refused
while the active controller held its lock. Inspection has no timer and does
not remove the VM because of elapsed time.

A fresh lifecycle on 2026-10-16 passed the new `network` phase and paused after
`verify`. The real isolated TUI discovered `vmbr0` and selected it as
**Server network bridge** without requiring Gold. A temporary guest network
namespace connected through the bridge to PVE HTTPS, verified the public
certificate and received HTTP 200. Forwarding from that namespace to the outer
APT proxy was blocked. Its namespace and veth ports were removed after the
probe; this check did not deploy a HomeStack container. Reapplying the bridge
configuration to an existing lab also succeeded.

The workspace test run passed **1005 tests**, with 30 opt-in tests ignored,
including fixture ownership, exclusive locking, scoped cache reset,
image-cache reuse/repair and failed-download preservation, APT status parsing
and installer output/error preservation, failure preservation
and inspection remaining paused after two simulated days until manual resume,
without consuming the active installation deadline. Clippy and formatting
checks passed. Tests requiring local sockets run outside the restricted tool
sandbox; live production-cluster tests remain opt-in and were not run for
this bootstrap change.

HomeStack preparation was verified on 2026-10-16 against the actual lab:
the native PVE OCI worker downloaded the published server image and a second
preparation reused its archive. Public and split DNS resolved the API name to
`10.77.0.101`; the dedicated wildcard certificate was publicly trusted and
subsequent preparation created no ACME order. The real installer reached
**Review with zero blocking problems**, with SSH host, domain and bridge
prefilled and Gold absent. It stopped before applying the installation.
A real Rust deployment-client status request also authenticated to PVE through
the SOCKS tunnel with normal hostname and public-CA verification. The temporary
lab token was removed, no container was created, desktop configuration stayed
unchanged and the user's VM remained paused.

A separate fresh VM then passed every phase from Debian boot through
`homestack`, verification, graceful shutdown and complete VM removal with the
default 1 vCPU, 2048 MiB RAM and 12 GiB disk. It reused existing DNS and valid
E2E certificates, generated a complete isolated profile for `pve1` and pulled
the published OCI image through the restricted registry tunnel. Its copied
certificate caches had no DNS ownership and were reset afterward; the original
fixtures and the user's paused VM remained available.

The first real manual container deployment on 2026-10-16 exposed an internal
DNS failure after successful HTTPS health checks and configuration upload:
dnsmasq answered A but refused AAAA, preventing the Alpine server from resolving
its PVE endpoint. Declaring `local=/e2e.homestack.app/` fixed both the running
lab and future bootstraps. The actual `homestack --json status` then succeeded
through the client, deployed CT 101 and PVE, with normal public TLS validation;
the existing container was retained. Reopening the real E2E TUI loaded its
installed profile and displayed node `pve1`; desktop configuration stayed
unchanged and the lab remained paused. Gold remained unset.

An opt-in regression test launches a separate dnsmasq on a random loopback
port and runs the bootstrap's actual DNS check against both configurations.
It must reject the old configuration and accept the corrected one:

```sh
cargo test --locked -p hs-e2e isolated_dns_covers_a_and_aaaa -- --ignored --nocapture
```

This check requires `dnsmasq` and Python 3 on the test host. It does not edit
the host resolver, use provider credentials or change any VM. It passed on
2026-10-16, alongside all 30 regular E2E tests, Clippy and formatting checks.
