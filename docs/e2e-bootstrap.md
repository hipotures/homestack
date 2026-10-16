# Disposable Proxmox bootstrap

`hs-e2e` implements the first executable part of the
[isolated E2E laboratory design](isolated-e2e-lab.md): download a Debian qcow2,
initialize a VM, install Proxmox, verify it, power it off and delete it.
It does not install HomeStack or create Gold/workspace guests yet.

The [planned DNS/TLS lifecycle](isolated-e2e-lab.md#persistent-dns-and-certificate-fixture)
uses `e2e.homestack.app`, retains owned DNS records and cached public
certificates across runs, and adds an explicit `cleanup --all` fixture reset.
That lifecycle is not implemented here: this bootstrap makes no public DNS or
ACME calls, uses the VM's own PVE CA, and removes all of its private VM data.

## Run

Use a Linux x86_64 host with accessible `/dev/kvm`, nested virtualization enabled,
and these commands on `PATH`:

- `qemu-system-x86_64` and `qemu-img`;
- `ssh` and `ssh-keygen`;
- `mkfs.vfat` from dosfstools and `mcopy` from mtools.

QEMU also needs its normal firmware and user-networking dependencies. For
example, on Debian/Ubuntu the packages are `qemu-system-x86`, `qemu-utils`,
`dosfstools`, `mtools` and `openssh-client`. No root privileges are required to
run the controller if the invoking user can open `/dev/kvm`.

From the repository root:

```sh
cargo run --locked -p hs-e2e -- run
```

For the locally extracted QEMU cache used to validate this workspace:

```sh
PATH="$PWD/target/e2e-tools/bin:$PATH" cargo run --locked -p hs-e2e -- run
```

That ignored cache is a local tool installation, not part of a fresh checkout.
The controller downloads the guest image; it does not install host packages.

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
| `image` | Download verified and sparse disk prepared; no VM yet |
| `boot` | Debian booted, Cloud-Init and initialization checked; SSH and restricted APT networking work |
| `kernel` | Proxmox kernel installed and the VM rebooted |
| `proxmox` | PVE services installed and rebooted; temporary web credentials and public CA available |
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
contains the panel URL, login `root@pam`, generated password and CA location.
In the web form, use username `root` and realm **Linux PAM**.
The password is not printed to logs or stored in artifacts. Its file is mode
`0600` inside the private run directory and is removed during cleanup. SSH
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

The panel URL defaults to `https://127.0.0.1:<reported-port>` **on the runner
host**, forwarding to PVE's TCP 8006. The public PVE CA is saved as
`pve-ca.pem` in artifacts and the private run directory. Trust that CA in a
dedicated browser profile to verify the default localhost/127.0.0.1 name;
the runner does not change browser or system trust. Browser CA trust and web
password authentication are separate from the pinned SSH host key.
This increment still uses PVE's own CA; the persistent public domain and
Let's Encrypt integration remain the separate design linked above.

For a remote runner, open a tunnel from the desktop using the host's ordinary
SSH account and the port printed by the runner, for example:

```sh
ssh -N -L 18006:127.0.0.1:REPORTED_PORT USER@RUNNER_HOST
```

Then open `https://127.0.0.1:18006` in the desktop browser. Read the private
access file on the runner for the temporary web password and transfer only the
public CA for browser trust. With a nested HomeStack workspace, the tunnel
terminates at the workspace running this controller. Its host address and
authentication belong to the existing desktop SSH configuration.

For direct access over a reachable host interface, explicitly set
`--web-bind 192.168.1.20 --web-port 18006`, replacing the address with one
assigned to the runner. Bind defaults stay on loopback; no host firewall rules,
physical bridges or routes are changed. The VM certificate does not
automatically cover the host's LAN address: access by the certificate's FQDN
with matching DNS/hosts resolution, or use the loopback SSH tunnel. The
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
3. Generate per-run client and server SSH keys, a hostname, an instance ID and
   JSON initialization data. Supply them through a NoCloud `CIDATA` FAT disk.
   Password authentication is disabled. Pin the guest SSH host key before
   connecting, rather than accepting a network-discovered key. Set the guest's
   short hostname, FQDN and resolver search domain explicitly so PVE generates
   a certificate for the intended `e2e.test` name.
4. Start QEMU with KVM, host CPU features and local-only SSH/PVE port forwards.
   Wait for SSH and successful Cloud-Init, then read the initialization data
   from the guest and compare it with the requested values.
5. Install the Proxmox kernel from its signed no-subscription APT repository,
   reboot and confirm a new boot ID. Install Proxmox VE services and reboot
   once more. Package installation runs only inside the disposable VM.
6. Verify the running `-pve` kernel, exposed nested KVM device, active PVE
   services and preserved initialization data. Obtain the PVE CA through the
   pinned SSH connection and verify the real HTTPS `/api2/json/version` API
   with an ephemeral guest-local API token. Check that the real web page is
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
handled by this first increment. The independent watchdog and durable ownership
journal in the larger design remain required before unattended destructive
testing. A successful check of `/dev/kvm` inside PVE also does not yet prove
that a nested guest boots; that is a later scenario.

## Local checks

```sh
cargo test --locked -p hs-e2e
cargo clippy --locked -p hs-e2e --all-targets -- -D warnings
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

All ten local tests passed, including failure preservation and inspection time
being excluded from the active deadline. Clippy and formatting checks passed.
