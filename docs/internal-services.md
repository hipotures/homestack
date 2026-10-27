# Internal services: API and Pangolin Site

The repository declares two portable schema-1 services:
`.homestack/homestack-api.yaml` and `.homestack/pangolin-site.yaml`.
HomeStack API and new Pangolin Site connectors initialize through common service
deployment. No existing internal container is adopted or modified.

## Attach an existing application to Pangolin

In the service's maintenance settings, select one running instance, choose
**Pangolin → Publish**, and review and apply. HomeStack opens its verified SSH
API tunnel, finds or creates the manifest's resource and routes it through an
online Site to the manifest's application port. F5 refreshes the lists and
preserves edits. Open **Pangolin domain** to connect and choose from the
organization's verified domains, for example `hs.example.com`. If publication
is requested before a domain is selected, HomeStack opens that selector before
Review. Publishing under a different base domain creates or reuses that name's
resource; the old resource is preserved.

This operation creates a missing resource and backend target or updates the resource's one
existing target. It enables monitoring from the service manifest's HTTP health
check: the declared port, path and expected status, with GET requests every
30 seconds and a five-second timeout. Reattaching also repairs a target whose
monitoring was disabled. The manifest must declare an HTTP health check.
It preserves resource policies and other routing settings,
and performs only reads against Proxmox. It never deploys, replaces, restarts
or removes the application or its volumes. Save disk/environment changes
separately; attachment cannot be combined with removal. Resources with multiple
targets require managing their routing in Pangolin.

When publishing, HomeStack first reads public A and AAAA answers from both public
resolvers. An existing name, including a wildcard or alias at any DNS provider,
can be reused without DNS credentials when the resolvers agree and its addresses
include a public address of the Pangolin dashboard. Review shows the full address
list, including IPv6; the operator must ensure all those addresses serve the
resource. Apply rechecks the addresses before saving Pangolin routing.

For a name without public addresses, HomeStack identifies the zone and provider
from the public NS delegation. OVH and Spaceship support creating a missing
CNAME to the Pangolin dashboard hostname through the corresponding configured
DNS integration. Other providers require setting up DNS manually. HomeStack
refuses a conflicting record instead of replacing it. Apply creates the missing
record and waits until both public resolvers see it before saving Pangolin routing.
If propagation times out, the record remains in place; review and apply again
later. Publishing an existing resource can repair a missing DNS record without
replacing its container.

The DNS check does not verify certificate issuance or public application access.
The Pangolin server needs the appropriate public ports and certificate setup.

Review and apply both verify organization membership, current Site identity
and target configuration. Apply rechecks the container's identity and address,
then verifies the saved target. An uncertain mutation response is never retried
automatically. Saving routing does not verify public access or backend health.
The Pangolin CLI connectors retain the API's `newt` Site type; no standalone
Newt process is installed or started.

## API deployment

After publishing a service's A record, deployment refreshes that name with
`resolvectl query --cache=no --type=ANY` and verifies that the desktop's
normal resolver returns the planned IPv4 address. The targeted refresh runs
when `resolvectl` is installed; otherwise the system resolver is checked
directly. No root access or system-wide cache flush is needed.
The DNS refresh and verification have a shared 15-second deadline. After
starting a new API container, bootstrap waits up to 15 seconds for its IP to
answer `ping`, then checks authenticated HTTPS with its own 15-second limit.
The desktop needs `ping`; ICMP reachability does not replace HTTPS readiness.

The service browser and review/apply worker use
`services/deploy.rs::{survey,plan,apply}` and the scoped PVE deploy token.
The canonical [API manifest](../.homestack/homestack-api.yaml) declares empty
`persistence` and `state_model: stateless`: new instances have a disposable
root only, with no `mp0` or persistent-home selector. It defaults to three
independent instances spread across nodes, a 256M root and 256M memory.
Review can select multiple nodes; each selected node gets one independent API.
The default hostname is `hs-api`, with numbered names for multiple instances.

Common ownership is `HomeStack service homestack-api`, plus repository,
manifest revision, image and digest in the PVE description. `homestack-api`
is the manifest ID; `homestack-server` is the container tag and OCI image name.
There is no desktop deployment registry. Only the canonical API manifest from
`hipotures/homestack` is accepted for this initializer. Choose image `0.2.31`
or later. Private HTTP port 80 serves only `/health/live`, `/health/ready`
and `/.well-known/homestack/status`; all other HTTP paths return 404.
The manifest uses HTTP readiness on this port, so common deployment and image
replacement record `status-port 80` in the container description. The schema-2
telemetry then supplies CPU, memory and I/O to the container view. Existing
instances need an image update with the current manifest to receive the HTTP
listener and telemetry metadata. API operations and secret management remain
on HTTPS port 443. Bootstrap and stateless API image updates also perform
authenticated HTTPS checks.

### Prerequisites checked before creation

`hs_deploy::deploy::ApiBootstrap::load` reads the trusted desktop inputs from
`deploy.toml`: certificate and private-key files, bearer and runtime PVE token
files, endpoints, pool settings and `server.toml`. Review checks certificate
validity, wildcard coverage and matching key, a nonempty bearer token, a
parseable PVE token, HTTPS endpoints, configuration upload size and hostname
trust against system/configured CA roots.
Apply reloads these inputs and verifies the reviewed image digest/platform
before creating anything. Keep these files available for later replacement:
the stateless API's local configuration is disposable.

The generated name is `<hostname>.<configured domain>`. The HTTPS client
connects to its reviewed address with that TLS hostname using
`instances_client`; certificate checking is never bypassed. The desktop needs
scoped PVE access, but no already-running HomeStack API. Leave Environment
empty: bootstrap supplies the API settings. A custom `[pve] ca_file` blocks
common deployment because bootstrap cannot upload that CA; PVE endpoints must
use system-trusted certificates. The optional desktop API HTTPS CA remains
supported and is a separate trust setting.

The installer grants `HomeStackDeploy` on `homestack-service` and
`homestack-test` to the desktop deploy token and its matching user ACL for
privilege separation. An isolated API uses its separate `!test` runtime token;
the deploy token is never uploaded. Test-part uninstall removes its test-pool
ACL while retaining service-pool access. See [Proxmox permissions](proxmox-permissions.md)
for exact role reconciliation and permission repair. Existing installations can
add the two test-pool entries on a PVE node:

```sh
pveum acl modify /pool/homestack-test --users homestack@pve --roles HomeStackDeploy --propagate 1
pveum acl modify /pool/homestack-test --tokens 'homestack@pve!deploy' --roles HomeStackDeploy --propagate 1
```

### Shared authenticated bootstrap

Common deployment and the installer's `servers.rs::RealServers::deploy` path
call `ApiBootstrap::initialize`. It initializes an already-created container;
it never creates, replaces, destroys or mounts storage or rewrites ownership.
For a current root-only common instance:

1. Common creation leaves autostart disabled and creates no home volume.
2. Bootstrap supplies the instance name, bearer token SHA-256, runtime PVE
   endpoints/pools and temporary TLS bootstrap values via the scoped PVE API.
   It starts the CT and checks authenticated HTTPS `GET /health/live` with
   hostname and CA validation.
3. It removes both TLS bootstrap variables, reboots if PVE reports pending
   environment changes, checks HTTPS again and confirms the variables are gone.
4. It uploads the runtime PVE token and `server.toml` through the protected
   HTTPS routes and verifies the returned configuration version. Common
   deployment then enables autostart and reports success.

Without `HOMESTACK_MANAGED_HOME=1`, the API and Caddy use
`/var/lib/homestack` in the disposable root. Each new or replaced stateless API
receives its configuration, certificate and credentials again from the desktop.
A failure leaves the created CT available for inspection; there is no
home-initialization marker or occupied-CTID deployment resume for this mode.
Review an image update of the existing common instance to replace its root and
bootstrap it again. A fresh deployment always requires a free CTID.

Raw credentials remain in private desktop files and bootstrap memory. Temporary
TLS values use PVE environment transport and are removed before success; a
failure before cleanup can leave them there. Plans, descriptions, progress and
logs do not contain raw tokens, certificates or private keys.

### Replacement and existing ownership paths

Common image updates use `hs_deploy::recreate`: keep CTID, address, MAC and
reviewed settings, replace the root, and bootstrap root-only APIs again from
the desktop inputs. Original power/autostart intent survives interrupted updates
in PVE metadata. Reopen Update image to continue; a deliberate reinstall of the
same release replaces the root too. See [service replacement](service-replacement.md).

Home-backed API instances created with earlier persistent manifests retain
support in the implementation. With `HOMESTACK_MANAGED_HOME=1`, runit and Caddy
select `/home/app/homestack`, require the exact mount and reject symlinks. The
0700 directory holds `server.toml`, `secrets/pve-token` and `tls/{cert,key}.pem`
with private file modes. Replacement reassigns the original home and keeps the
selector. That support does not make the current manifest stateful.

Only home-backed initialization uses `api-initialization-home` to record the
original `mp0`. Its recovery requires the same CTID, manifest revision, image,
node/storage, sizes, ownership, root, network and original home; it never
allocates another home. It reruns bootstrap, checks stored configuration and PVE
access after reboot and requires fresh home telemetry before removing the
marker and enabling autostart. Conflicting bootstrap settings are refused.
Finish pending initialization before image updates or migration.

Installer-owned API containers retain their `HomeStack server <name>` ownership
and `deploy.toml` selection. `homestack install`, the TUI API update action and
the standalone `homestack-deploy` commands remain supported; they do not adopt
unrelated containers into common service ownership. The standalone entry points
are `cert [--force] [--push]`, `apply [--tag VERSION] [--only NAME]`,
`status [--only NAME]` and `remove [--only NAME] --yes`, with optional
`--config FILE`. Removal is destructive; it is separate from
`homestack uninstall`, which leaves containers and volumes intact.

## Pangolin Site deployment

A new managed Site connector is deployed through the same service browser,
review/apply worker and `services/deploy.rs::{plan,apply}` as other services:
published numeric release and digest, reviewed node/storage/CTID/address and
sizes, scoped deploy token, unprivileged LXC, disposable rootfs and a separate
persistent `mp0` at `/home/app`, with the common `HomeStack service
pangolin` identity (manifest id `pangolin`), repository, manifest revision, image and digest. The
legacy Step 8 description and tag are not used. Only the canonical
`.homestack/pangolin-site.yaml` of `hipotures/homestack` is initialized;
another manifest id, path or repository using the Site image is refused. One
instance is deployed per review. The operator must choose Site image `0.18.2`
or later, with native HTTP health and telemetry. Environment must be
empty; HomeStack supplies the managed-home selector and heartbeat interval.
Defaults are a 128M root disk and a 10M home: sizes are typed as a number
with M or G for every service (deployment and maintenance), and Proxmox stores
a 10M home as `size=10M`. Names follow the reviewed hostname: the first single
instance is unnumbered;
multiple or additional instances use free numbered suffixes. A 128M root or
10M home is listed as such, not 0G.

Site images have an independent SemVer line. `python3 scripts/tag-image.py`
automatically publishes the next `pangolin-X.Y.Z` Git tag, producing
`homestack-pangolin-site:X.Y.Z`; `--dry-run` previews it. The pinned upstream
Pangolin CLI version is a separate build input. See
[the runtime contract](../deploy/pangolin-site/README.md) for HTTP health,
on-demand telemetry, persistence and local verification evidence.

### Authenticated context

The common screen does not use the Pangolin wizard's memory. For a Site, the
deploy worker (`tui/pangolin_sites/managed.rs`) reads the saved Pangolin
settings (`integrations/pangolin.toml`): the verified Step 1 server, Step 3
`app.dashboard_url`, Step 4 loopback Integration API, Step 6 organization and
the Step 7 key destination. It starts its own Step 5 tunnel with the existing
`pangolin_tunnel::Session` (SSH, hardware key and askpass bridge as in Step 5)
and accepts only a live endpoint that passed the existing Pangolin health
check. The Step 7 key is read through its recovery record, which must be
`Complete`, for this server and organization, named `homestack`, credential
saved and actions verified, and whose file holds exactly the recorded key ID.
`getOrg` must return the organization. These checks run at review, again
before anything is made, and before every Pangolin step. The temporary Root
key is never read: it exists only in the wizard's memory. No Pangolin
credential enters the manifest, plan, PVE metadata, progress or logs.

No root SSH to Proxmox is used. The credential reaches the new container once,
through its Proxmox console (`hs_deploy::console`: `termproxy` and its
websocket, deploy token with `VM.Console`, console mode `shell`), which is a
shell inside the unprivileged container, not on the node. The installer
grants `VM.Console` in `HomeStackDeploy`. For an existing installation, review
and apply the updated installer's permission plan; a manual role addition alone
can be removed by an older installer. See [Proxmox permissions](proxmox-permissions.md)
for exact role reconciliation and user/token ACL requirements. With this privilege
the deploy token can open a root shell in every container of its pools.

### Instance identity and journal

A numbered container such as `pangolin-1` gets Site name and nice ID
`homestack-1`. Without a numeric hostname suffix (for example the first
single `pangolin` instance), the Site name is `homestack-<CTID>`. The journal
keeps that name, so renaming the container later is refused rather than followed.
Its credential is `secrets/pangolin-managed-site-<identity>-<CTID>.key` (mode
0600), with `.recovery.json` and `.lock` beside it; `<identity>` is the Step 7
SHA-256 of server and organization. Two instances, also on one node, therefore
never share a name, credential, journal or lock. The journal (Step 8's
`SiteRecord`) records server, organization, node, Site, Newt ID, Site ID,
CTID, original home volume and stage: `Prepared` (credential saved, Pangolin
not asked), `CreationRequested`, `Created`, `Installed` (credential verified in
the container) and `Online`. It is written only after the container exists, so
its CTID and home record that creation. It is a private credential journal,
not a deployment registry: ownership remains in Proxmox.

Review refuses an existing Site of that name whose Newt ID is not the saved
credential's, a Site created earlier but now gone, a Site ID different from
the journal, and any journal or credential left for a CTID whose container is
free (an earlier instance): such leftovers are inspected and removed
explicitly, never adopted.

### Initialization

1. Common creation attaches `mp0` before the first start, autostart off.
2. The description temporarily records `site-initialization-home` with the
   exact original `mp0` option; then `HOMESTACK_MANAGED_HOME=1` is set while
   the container is still stopped, together with the manifest's
   `HOMESTACK_HEARTBEAT_SECONDS`. Other application overrides are refused.
3. The Step 8 journal logic saves a new credential on the desktop (or recovers
   the saved one) and the journal before Pangolin is asked; the existing
   `create_site` marks `CreationRequested`, sends the saved Newt ID and secret,
   and resolves a rejected or unknown outcome by reading Pangolin's Site of
   that nice ID. An uncertain result never creates another identity: the next
   run reconciles the Site ID and Newt ID with the journal and reuses the same
   credential.
4. The container starts. Its console turns echo off, then runs
   `env HOMESTACK_MANAGED_HOME=1 homestack-site-credential` with the
   configuration as a here-document (the selector is explicit; a console
   shell does not inherit PID 1's environment), and prints the installed
   file's digest behind a marker written in two parts, so the terminal's echo
   of the command never matches. It writes `/home/app/pangolin-site/site.json`
   (directory 0700, file 0600, owner site), and its SHA-256 must match the
   local configuration.
5. The worker polls Pangolin until exactly that Site ID is online and checks
   that the Site still has the journal's organization, ID and Newt ID.
6. Common deployment rechecks the running CT, original root/home/network,
   ownership and selector, removes the marker, enables autostart and reports
   success.

Success therefore means: the CT exists in the reviewed pool and runs, the
original `mp0` is at `/home/app`, managed mode is on, the installed credential
matches the saved one, and Pangolin reports that Site of that organization,
with the journal's IDs, online. Initial readiness uses Pangolin's online status;
subsequent image updates use the manifest's HTTP readiness check.
`DISABLE_SSH=true` and the pinned CLI are unchanged; the connector never
updates itself, and no new daemon is added.

### Recovery

A failure removes nothing. To resume, review deployment again with the same
CTID, version, node/storage and sizes. Recovery accepts only a CT with the
exact common description and marker of the original `mp0` (or a pristine
stopped CT before the marker), its reviewed resources and address, and no
application environment beyond the managed selector and manifest heartbeat.
Its journal must identify that CTID and the CT's original home. It never
allocates another volume and never recreates, formats or replaces the home. The image is neither pulled nor the container created
again; the steps above are idempotent: an existing Site is reused, an
installed credential is detected by digest. Foreign, mismatched or ambiguous
ownership, journals or Sites are refused.

### Image updates and other operations

Updates use the existing `hs_deploy::recreate` replacement unchanged. It keeps
the CTID, address, MAC, original `mp0` and environment (`HOMESTACK_MANAGED_HOME=1`
included), so the credential on the persistent home stays. The Site
initializer is not called: no Site is created, no credential rotated or
reinstalled, no root session opened. Image review and replacement use only the
Proxmox API and the manifest's HTTP health check after start. They require no
SSH tunnel, Pangolin API connection or desktop Site journal: the existing Site
identity and credential remain on the same persistent home. Opening the service
screen also requires no Pangolin connection; operations that need its API open
the context when reviewed. Migration and restarts after environment changes
still verify the Site through Pangolin. Containers still initializing
cannot be updated or migrated, and the managed selector cannot be edited in
maintenance. Step 8 connectors have no image update at all: replacing their root
disk would need root SSH to restore the credential, so that path was removed.

### Known limitations

- Removing an instance in maintenance first deletes its Site from Pangolin
  (`DELETE /v1/site/{siteId}`, `deleteSite`, only when Pangolin's Site has the
  journal's IDs), then its credential, journal and lock files, then the
  container with its root and home. A failure before the container is deleted
  leaves it listed, so removal can be repeated. It needs the Pangolin tunnel,
  not root SSH.
- The Site's dashboard URL comes from Step 3 discovery; a changed URL rewrites
  the same credential on resume, never a new identity.
- Pangolin online status depends on Pangolin's own reporting interval; the
  wait is 180 s.
- The console transfer was checked live on a test container in
  `homestack-test` (a 10M home, a non-secret configuration, the digest
  matched in four runs, the container removed afterwards). A real Pangolin
  Site creation and online transition through common deployment are still
  unverified.

### Remaining isolated live verification

A separately authorized run in `homestack-test` should deploy one new Site
from image `0.18.2` or later, interrupt and resume initialization, check that
the credential is at `/home/app/pangolin-site/site.json` and the Site is online,
replace its image and check that the Site stays and comes back online without
a root session, and finally remove the test Site, journal, container and home.

## Verification and adoption boundary

The [2026-10-23 live verification](managed-api-live-verification-2026-10-23.md)
records a home-backed API test in `homestack-test`: bootstrap/reboots, direct
guest-file persistence and same-CTID root replacement. It is dated evidence
for that instance and manifest, not a claim that the current API manifest
allocates a home. It does not establish real common Pangolin deployment or
application-data persistence for arbitrary services. PVE metadata and health
alone do not prove existing application records were read.

Existing installer-owned APIs and Step 8 Site connectors are not automatically
adopted by common deployment. Changing their ownership or transferring state
requires a separate owner decision and an inventory of mounts, certificates,
tokens, Site identity and desktop recovery files. The Step 8 integration and
its saved state remain supported; its connectors have no root-image update
because that would require restoring credentials through root SSH.

API image releases use numeric Git tags and `scripts/tag-api.py`. Pangolin Site
releases use independent `pangolin-X.Y.Z` tags and `scripts/tag-image.py`, with
numeric OCI versions `X.Y.Z`. The upstream CLI version is a separate build
input, not the Site image version. Publishing an image does not deploy it.
