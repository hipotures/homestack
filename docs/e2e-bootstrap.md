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
The run deadline defaults to 1800 seconds; final forced process cleanup runs
after a timeout or interruption. There is no automatic software-emulation
fallback and no option to target an existing VM or Proxmox installation.

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
   with an ephemeral guest-local API token. TLS verification stays enabled.
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
network has `restrict=on` and IPv6 disabled. SSH and PVE forwarding bind only
to `127.0.0.1` on automatically selected ports.

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
the final disk-backed working directory. The five local tests, Clippy and
workspace compilation also passed.
