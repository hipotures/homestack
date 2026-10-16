# Disposable Proxmox bootstrap

`hs-e2e` implements the first executable part of the
[isolated E2E laboratory design](isolated-e2e-lab.md): download a Debian qcow2,
initialize a VM, install Proxmox, verify it, power it off and delete it.
It does not install HomeStack or create Gold/workspace guests yet.

The runner prepares `pve.e2e.homestack.app` through the existing Spaceship and
ACME implementation in `hs-deploy`. It installs a public Let's Encrypt
certificate in PVE and retains DNS and certificate fixtures across runs.
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
`--local-dns` option creates the exact E2E A record there using
`~/.config/homestack/dns-tsig.key` (override with `--tsig-file FILE`), while
Spaceship serves the public A record and ACME TXT challenge. It does not edit
other names in either zone. Networks without a private zone omit this option.

That ignored cache is a local tool installation, not part of a fresh checkout.
The controller downloads the guest image; it does not install host packages.
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
There is no automatic software-emulation
fallback and no option to target an existing VM or Proxmox installation.


## Installation phases and manual inspection

Normal runs remain automatic. Every phase writes its status, timings and error
to `report.json` before the next phase starts:

| Phase | Available state after success |
|---|---|
| `preflight` | KVM and host tools checked; no VM yet |
| `domain` | E2E DNS and publicly trusted certificate prepared or reused; no VM yet |
| `image` | Download verified and sparse disk prepared; no VM yet |
| `boot` | Debian booted, Cloud-Init and initialization checked; SSH and restricted APT networking work |
| `kernel` | Proxmox kernel installed and the VM rebooted |
| `proxmox` | PVE services installed and rebooted; public certificate installed and temporary web credentials printed |
| `verify` | PVE kernel, exposed nested KVM, HTTPS API, web page, password login and authenticated node inventory checked |
| `shutdown` | VM powered off; private files still exist until cleanup |
| `cleanup` | QEMU reaped and private disks, keys and web credentials removed |

Request pauses only where manual inspection is useful:

```sh
cargo run --locked -p hs-e2e -- run \
  --pause-after boot,kernel,proxmox,verify \
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

Each pause has its own deadline, 3600 seconds by default, configurable with
`--pause-timeout-seconds` (1–86400). Inspection time does not consume the active
installation budget set by `--timeout-seconds`. A pause deadline, Ctrl-C or
SIGTERM triggers cleanup. Keep the controller running during inspection.
Socket files are also removed on cancellation. Cleanup itself cannot be paused.

## Browser and SSH access

The panel URL is `https://pve.e2e.homestack.app:<reported-port>`, forwarding
to PVE's TCP 8006. With the default loopback binding, DNS points the hostname
to `127.0.0.1`. Open the printed domain URL: normal browser trust verifies the
public certificate without importing a private CA. PVE's internal cluster CA
remains separate. The runner prints username `root`, realm **Linux PAM** and
the temporary password. PVE requires its login form; passwords are not placed
in URLs.

For a remote runner, open a tunnel from the desktop using the host's ordinary
SSH account and the port printed by the runner, for example:

```sh
ssh -N -L 18006:127.0.0.1:REPORTED_PORT USER@RUNNER_HOST
```

Then open `https://pve.e2e.homestack.app:18006` in the desktop browser, where
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


## What a successful run proves

1. Download the current official
   [Debian 13 genericcloud amd64 qcow2](https://cloud.debian.org/images/cloud/trixie/latest/)
   and check its SHA-512 against the publisher's manifest fetched over HTTPS.
   Record the actual hash and byte count in the report. A checksum mismatch,
   including a publication change during download, fails the run.
2. Inspect the image format and reject external backing files. Create a private
   writable qcow2 overlay; the downloaded base stays read-only.
3. Generate per-run client and server SSH keys, an instance ID and
   JSON initialization data. Supply them through a NoCloud `CIDATA` FAT disk.
   Password authentication is disabled. Pin the guest SSH host key before
   connecting, rather than accepting a network-discovered key. Set the guest's
   short hostname, FQDN and resolver search domain explicitly so PVE generates
   the intended `pve.e2e.homestack.app` node identity.
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
   TLS verification stays enabled.
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
records are reused without claiming ownership. Only the explicit PVE SAN is
issued; production certificates and their cache are not reused or changed.

Each run checks DNS independently of certificate reuse. A certificate is reused
when its explicit SAN and expiry leave at least 30 days, its key matches and
its chain passes normal system trust validation. This creates no new ACME
order or challenge. A missing record is repaired without replacing a valid
certificate. Temporary DNS-01 values are removed after issuance or failure;
a failed cleanup leaves a durable journal for the next run to retry.

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

The runner verifies system DNS resolution before boot. Split DNS, router
rebinding protection or a cached negative reply can prevent browser access;
configure the E2E name on the client resolver as needed. If a name lookup fails,
the runner attempts a bounded refresh through `resolvectl` when available,
without changing resolver settings. On a separate client,
`resolvectl query --cache=no pve.e2e.homestack.app` refreshes this name through
systemd-resolved without changing DNS configuration.

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
The guest has no general Internet or host LAN
access. See [QEMU's restricted user networking](https://www.qemu.org/docs/master/system/invocation.html).

On an error, deadline, Ctrl-C or SIGTERM, the controller terminates and reaps
its QEMU child before removing its private files. Graceful shutdown and final
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
while the active controller held its lock. An independent inspection timeout
also removed the VM while retaining these fixtures.

All fifteen E2E unit tests and twenty-three deployment unit/integration tests
passed, including fixture ownership, exclusive locking, scoped cache reset,
failure preservation and inspection time being excluded from the active
deadline. Clippy and formatting checks passed. Tests requiring local sockets
run outside the restricted tool sandbox; live production-cluster tests remain
opt-in and were not run for this bootstrap change.
