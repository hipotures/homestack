# Managed HomeStack API: live verification, 2026-10-23

This run used the real existing Proxmox cluster, exclusively creating guests in
`homestack-test`. It exercised the common `services::deploy::{survey,plan,apply}`
and `services::update::{plan,apply}` operations through a temporary Rust driver,
not a simulated deployment. The driver restricted mutations to the selected
CTID, its temporary holder and test-created template files. No permanent test
suite was added. Source baseline: `413645f`, including API handoff `67a7f28`.

## Isolation and release

- Node: `pve2`, online, Proxmox 9.2.21.
- Pool: `homestack-test`; its pre-existing stopped VM 200 was retained.
- CTID: `103240`, verified unused cluster-wide. Temporary holder: `999999`, also
  verified unused before replacement.
- Address: `192.168.103.240/22`; bridge `vmbr0`; gateway and DNS `192.168.100.1`.
  Cluster guest configuration inventory showed no static-address collision.
  Three ARP probes received no reply from the candidate; the gateway replied
  and the selected DNS server resolved `ghcr.io`. These checks establish
  observed availability, not a permanent DHCP reservation.
- MAC: `BC:24:11:B3:C4:CF`, retained through replacement.
- Template storage: `local`; root and home: `local-lvm` (active, appropriate
  content types, approximately 1.25 TB available before creation).
- OCI image: `ghcr.io/hipotures/homestack-server:0.2.10`.
- Verified manifest/platform digest:
  `sha256:d51ee3e6b366e33eb3daa3f2c3bbbaf99400f2982c54bb6c251b005ac5323631`.
  Registry bytes matched the returned digest; common planning/apply verified
  the release again.

The owner granted the existing desktop `!deploy` token access to the test pool.
The existing `!test` token also had guest/clone authority on `homestack-gold`, so
it was not installed. The owner instead created `!api-test`, whose effective
management authority was verified as confined to `homestack-test`, with only
`Sys.Audit` outside it. The existing bearer token and valid wildcard certificate
were reused. Production configuration and credential files were not changed.
A private temporary deployment/configuration directory was used on the desktop.
Gold was deliberately unconfigured; its inert pool setting was
`homestack-test/gold`, with no pool or guest created there. Workspace creation,
Gold cloning and broad management capabilities were outside this API test.

## New installation and interrupted initialization

Common creation made one unprivileged LXC with a 2 GiB disposable root and a
separate 1 GiB home before its first start:

- root: `local-lvm:vm-103240-disk-0`;
- home: `local-lvm:vm-103240-disk-1,mp=/home/app,backup=1,size=1G`.

The first apply stopped before startup because PVE appends a newline to
container descriptions, while the initializer compared them literally. Actual
state was inspected: the CT was stopped, autostart disabled, and its original
home remained attached. The targeted fix normalizes trailing whitespace in
three description comparisons, matching the existing ownership convention.
The normal explicit same-CTID recovery path then initialized that existing CT
without another image pull, create request or volume allocation.

The real bootstrap set `HOMESTACK_MANAGED_HOME=1` before startup, transferred
existing TLS material through the established PVE environment bootstrap, and
passed certificate/hostname-validated HTTPS readiness. It removed temporary TLS
environment values, rebooted, uploaded configuration and the isolated PVE token
through protected HTTPS routes, verified the configuration version, and passed
another reboot plus stored-token and mounted-home checks. Final common
ownership metadata was retained and autostart enabled. Resumed initialization
including read-only planning took 10.96 s; bootstrap itself took approximately
7.53 s. No bootstrap secret was printed by the driver.

## Direct persistence evidence

The existing root inspection channel on PVE was used only to inspect the new
test guest with `pct exec`; it was not part of deployment or replacement.
Inspection asserted the exact CTID, pool, common identity and home first.

A unique non-secret file was written to `/home/app/e2e-persistence-marker`.
Another marker was written to disposable `/var/tmp/e2e-rootfs-marker`.
The guest process environment reported
`HOMESTACK_STATE_DIR=/home/app/homestack`; mount information showed a real ext4
home mount. State directory mode was 0700 and configuration, TLS certificate,
TLS key and PVE token were 0600, owned by `homestack:nogroup`.

After an additional API-driven container reboot, direct guest reads confirmed
both markers and unchanged SHA-256 values for the home marker, configuration,
certificate, key and PVE token. Authenticated HTTPS, expected configuration
version, `/v1/pve/version` and fresh schema-1 mounted-home telemetry passed.
The protected configuration endpoint without a bearer token returned 401.

After rootfs replacement, the disposable-root marker was absent, while the
home marker and all four initialized-state file hashes remained identical.
The original full `mp0` option, IP, MAC and CTID were unchanged. LVM showed the
home's original creation time (01:24:39 local time), versus the replacement
root's new creation time (01:30:04); the home remained device 252:45 across
inspection. PVE temporarily renamed its LV while moving it to the holder and
restored its original volume name on return. No replacement home was allocated.

## Real image replacement and timings

The initial same-release reinstall review promised a new root, but execution
skipped replacement because the requested ownership description matched the
old one. The root marker remained present and there was no outage. This was
recorded as a defect, not accepted as successful replacement.

The targeted common-updater fix adds a non-secret replacement operation UUID
to its existing PVE descriptions and holder note. A new request gets a distinct
identity; recovery reuses the holder's recorded identity and refuses another
CT's holder. The `hs_deploy::recreate` volume algorithm was not changed.

A second reviewed reinstall of the same immutable release performed the real
replacement. Timings below are elapsed wall-clock intervals from progress and
PVE request/task completion observations; small inventory gaps are excluded.

| Step | Duration |
|---|---:|
| Pull update image | 4.29 s |
| Pull holder image | 2.13 s |
| Create and prepare networkless, never-started holder | 1.30 s |
| Graceful shutdown | approximately 1.19 s |
| Move original home to holder | 0.89 s |
| Destroy old CT after home moved | 0.31 s |
| Create new root under original CTID and verify network | 1.58 s |
| Move original home back | 0.88 s |
| Restore retained environment/options | approximately 0.06 s |
| Delete holder after it no longer owned home | 0.33 s |
| Restore autostart | approximately 0.12 s |
| Start request to TCP connectivity | 1.44 s |
| Start request to full HTTPS/config/PVE readiness | approximately 1.90 s |
| Entire updater, including pulls and image cleanup | 16.56 s |

An independent monitor used normal certificate and hostname validation, the
existing bearer token, and checked health, exact configuration version and PVE
access every approximately 0.2 s plus request time. First failed observation to
first recovered full success was **7.55 s**; the conservative interval from the
last complete success to the first recovered success was **7.89 s**. The sample
gap at outage onset was 0.34 s. Full readiness timing relative to updater start
is approximate, correlated with the final timestamped log write. The generic
updater itself still reports TCP connectivity only; independent HTTPS and
file checks supplied the stronger evidence in this run.

## Verification and cleanup

The temporary operation-identity diagnostic checked fresh request identities,
reuse from the matching holder, rejection of another CT's holder and malformed
identities, and preparation of an unfinished holder. It was removed after
passing. The three retained `hs-deploy` volume/recovery safety checks also
passed. Final wrapped `hs-cli`/`homestack` compilation and Clippy with warnings
denied passed. These code-level checks are distinct from the real LXC observations.

Cleanup reviewed exact pool, common identity, network and home, and enumerated
storage to ensure only the two known test disks belonged to CT103240. The
existing explicit common maintenance removal then deleted that test CT and its
root/home. The holder had already been removed by `recreate`. Exactly the two
new template files pulled by this test were removed; the updater removed its
own temporary image file. Final inventory confirmed no CT103240, holder999999,
or associated disks remained. VM200 and the six production API/Site container
configuration hashes and power states were unchanged. No pre-existing guest,
disk, snapshot or backup was removed.

The owner-created runtime test token and its private desktop/PVE transfer files
remain for the owner to retain or revoke. No credential was rotated or revoked
as cleanup. Task-specific logs and temporary driver/configuration remain under
`/tmp/hs-managed-api-live`; they are not repository content.

Pangolin Site remains blocked, and no production adoption, DNS, Pangolin,
network-policy or production guest mutation occurred. Both fixes affect only
client-side deployment/update code, so no new API image release is required.
