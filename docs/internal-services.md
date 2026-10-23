# Internal services: new managed instances

The repository declares two portable schema-1 services:
`.homestack/homestack-api.yaml` and `.homestack/pangolin-site.yaml`.
HomeStack API and new Pangolin Site connectors initialize through common service
deployment. No existing internal container is adopted or modified.

## API deployment

The existing service browser and review/apply worker use
`services/deploy.rs::{survey,plan,apply}` and the scoped PVE deploy token.
They select the numeric OCI release, node/storage, CTID/address and resources,
create an unprivileged LXC with disposable rootfs and a separate persistent
`mp0` at `/home/app`, and retain the common `HomeStack service homestack-api`
identity, repository, manifest revision, image and digest. No desktop registry,
SSH connection or second replacement engine is involved.

The API initializer accepts only the canonical HomeStack API manifest. One
independently configured instance is deployed per review. The operator must
choose image `0.2.10` or later: earlier images do not implement managed state.
The manifest's TCP 443 check still means connectivity only; API deployment
uses authenticated HTTPS checks in addition to that portable declaration.

### Prerequisites checked before creation

`hs_deploy::deploy::ApiBootstrap::load` reads the existing trusted desktop
inputs from `deploy.toml`. Review checks certificate validity dates, wildcard
coverage, a matching TLS private key, a nonempty bearer token, a parseable PVE
token and HTTPS endpoint settings, valid pool settings and `server.toml` within
its upload limit, and the chosen hostname/chain against the HTTPS client's
system and configured CA roots. It neither requests a certificate nor changes DNS. Apply reads
and validates these inputs again before any image pull, CT or volume creation,
and revalidates the reviewed image digest/platform.

The generated hostname is used as `<hostname>.<configured domain>` for TLS
hostname validation; the HTTPS client connects to the chosen instance address
using the existing `instances_client` mechanism. The system roots and optional
`[acme] ca_file` remain authoritative. Certificate checking is never bypassed.
The desktop needs its configuration and scoped PVE access, but does not need an
already-running HomeStack API. The existing installer retains its own first-API
path through `servers.rs::RealServers::deploy`.

The API's Environment field must remain empty: the initializer supplies its
settings. Arbitrary file/token/state overrides are not accepted. A configured
custom `[pve] ca_file` blocks this new path: the existing server bootstrap has
no custom-PVE-CA upload operation, so new instances currently require
system-trusted PVE endpoint certificates. The optional API HTTPS CA is a
separate desktop trust setting and remains supported. No alternative trust or
secret transport is invented.

The installer grants the desktop deploy token `HomeStackDeploy` on both
`homestack-service` and `homestack-test`, including the matching user ACL
required by privilege separation. An isolated API uses its separate `!test`
runtime token; the deploy token is never uploaded to it. Uninstalling the test
part removes the deploy token's test-pool ACL while retaining its service-pool
access. Existing installations can add the two test-pool entries on a PVE node:

```sh
pveum acl modify /pool/homestack-test --users homestack@pve --roles HomeStackDeploy --propagate 1
pveum acl modify /pool/homestack-test --tokens 'homestack@pve!deploy' --roles HomeStackDeploy --propagate 1
```

### Shared authenticated bootstrap

Legacy `hs_deploy::deploy::apply` and new common deployment call the same
`ApiBootstrap::initialize` operation. This operation never creates, replaces,
destroys or mounts storage, and never rewrites ownership descriptions.

For the new common instance:

1. Common creation attaches `mp0` before first start. Autostart is initially
   disabled so a host reboot cannot start an uninitialized API in legacy mode.
2. The common description temporarily records `api-initialization-home` with
   the exact original `mp0` option. This contains no application secret and
   keeps normal common-service ownership recognizable.
3. Bootstrap sets `HOMESTACK_MANAGED_HOME=1`, the unique instance name, bearer
   token SHA-256, PVE endpoints and pool settings, and the existing temporary
   TLS bootstrap values using the scoped PVE API. It starts the container and
   waits for HTTPS `GET /health/live` using the existing bearer client, hostname
   validation and CA roots.
4. It removes both temporary TLS bootstrap variables, reboots when PVE reports
   pending environment changes, checks HTTPS again, and confirms the variables
   are absent. It securely uploads the configured PVE token and `server.toml`
   through the existing protected HTTPS routes and verifies the returned
   configuration version.
5. Managed initialization reboots once more and verifies HTTPS, the expected
   stored configuration version, and read-only `/v1/pve/version` using the
   stored token. The existing HTTPS service telemetry must confirm a fresh,
   successful measurement of the mounted `/home/app`. Health requests carry
   the bearer header; protected configuration/PVE routes independently verify
   bearer authentication.
6. Common deployment rechecks the running CT, unchanged root/home/network and
   ownership, managed selector and TLS-variable removal. Only then does it
   remove the pending marker, enable autostart and report success. The common
   service metadata remains intact.

API runit scripts in `deploy/lxc/service/` select
`/home/app/homestack` only with the managed selector. They require an exact
mount and reject symlinks instead of creating disposable replacement state.
The directory is owned by homestack, mode 0700. `server.toml`,
`secrets/pve-token` and `tls/{cert,key}.pem` persist there, with private file
modes enforced by existing server writes. Caddy uses the same selected TLS
paths and serves port 443 in front of loopback 8080. No new listener or daemon
is added. Without the selector, legacy `/var/lib/homestack` behavior remains.

Raw bearer/PVE tokens stay in their configured desktop files and bootstrap
memory. The existing initial TLS transfer temporarily uses PVE environment
values; these are removed before success. No raw token, certificate or key is
put in plans, descriptions, progress or logs. Failure diagnostics identify the
stage and suppress secret-bearing response bodies.

## Interrupted initialization and recovery

A failure retains the created CT and its actual home. No failure branch deletes,
formats or replaces either. Progress reports the failing step; temporary TLS
values can remain in PVE after a failure before cleanup, and the resumed
initializer removes them. Autostart remains disabled until successful completion.

To resume, reopen deployment for the same manifest and explicitly choose the
same CTID, version, node/storage and sizes. Automatic CTID allocation always
chooses a new free CTID; it is not a recovery selector. Review establishes
ownership from actual PVE configuration and verifies the original home marker,
resources and address. Recovery does not reserve space for another root/home
pair because no new volume is allocated. Apply compares the observed home, root, network,
hostname and description again. It skips both image pull and LXC creation for
a resumed CT, then reruns the idempotent bootstrap/configuration checks.
A freshly created, still stopped CT interrupted before writing its pending
marker can also resume only when its exact common description matches and it
has no HomeStack bootstrap environment yet.

Missing/changed homes, disappeared CTs after recovery review, foreign or legacy
ownership, changed manifest/image metadata and already completed instances are
refused. Review must use the matching manifest revision; a changed revision is
not silently reassociated. A pending instance whose existing bootstrap settings conflict, or which has
unsupported HomeStack environment overrides, is refused rather than changing
its authentication identity or state paths. A running instance must already
have all required settings.
If a running instance has pending environment changes, the normal TLS cleanup
restart path is reused. Image updates reject the pending initialization marker;
finish initialization before using Update. No credential is regenerated.

Completed managed instances retain their selector, state and common ownership
through the existing PVE API `hs_deploy::recreate` path. Common image replacement
records a non-secret operation identity in its existing PVE description and
holder note, so reinstalling an identical release actually replaces the root;
interrupted replacement reuses the identity observed on its holder. The replacement
algorithm and its volume invariants are unchanged. Ordinary update/migrate manifest probing is
still TCP connectivity, not the complete initialization checks described here.
PVE metadata and guest telemetry do not prove application records were read;
this task makes no general data-persistence claim from mount metadata alone.

## Pangolin Site deployment

A new managed Site connector is deployed through the same service browser,
review/apply worker and `services/deploy.rs::{plan,apply}` as other services:
published numeric release and digest, reviewed node/storage/CTID/address and
sizes, scoped deploy token, unprivileged LXC, disposable rootfs and a separate
persistent `mp0` at `/home/app`, with the common `HomeStack service
pangolin-site` identity, repository, manifest revision, image and digest. The
legacy Step 8 description and tag are not used. Only the canonical
`.homestack/pangolin-site.yaml` of `hipotures/homestack` is initialized;
another manifest id, path or repository using the Site image is refused. One
instance is deployed per review. The operator must choose Site image `0.1.2`
or later: `0.1.1` predates the managed runtime selector. Environment must be
empty.

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

Root for the credential is the saved access host (`install_host`), opened with
the existing SSH root access (hardware key, no password or software-key
fallback). It is used only for credential installation and digest checks.

### Instance identity and journal

Each instance's Site is `homestack-site-<CTID>` (name and nice ID), distinct
from legacy `pangolin-<node>` Sites, whose identities are unchanged. Its
credential is `secrets/pangolin-managed-site-<identity>-<CTID>.key` (mode
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
   the container is still stopped. Any other environment is refused.
3. The Step 8 journal logic saves a new credential on the desktop (or recovers
   the saved one) and the journal before Pangolin is asked; the existing
   `create_site` marks `CreationRequested`, sends the saved Newt ID and secret,
   and resolves a rejected or unknown outcome by reading Pangolin's Site of
   that nice ID. An uncertain result never creates another identity: the next
   run reconciles the Site ID and Newt ID with the journal and reuses the same
   credential.
4. The container starts. The configuration is sent over the root session's
   standard input to `pct exec <CTID> -- env HOMESTACK_MANAGED_HOME=1
   homestack-site-credential` (the selector is explicit; `pct exec` does not
   inherit PID 1's environment). It writes `/home/app/pangolin-site/site.json`
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
with the journal's IDs, online. No inbound or TCP health check is used.
`DISABLE_SSH=true` and the pinned CLI are unchanged; the connector never
updates itself, and no new daemon is added.

### Recovery

A failure removes nothing. To resume, review deployment again with the same
CTID, version, node/storage and sizes. Recovery accepts only a CT with the
exact common description and marker of the original `mp0` (or a pristine
stopped CT before the marker), its reviewed resources, address and only the
selector in its environment, and a journal for that CTID whose home is the
CT's home. It never allocates another volume and never recreates, formats or
replaces the home. The image is neither pulled nor the container created
again; the steps above are idempotent: an existing Site is reused, an
installed credential is detected by digest. Foreign, mismatched or ambiguous
ownership, journals or Sites are refused.

### Image updates and other operations

Updates use the existing `hs_deploy::recreate` replacement unchanged. It keeps
the CTID, address, MAC, original `mp0` and environment (`HOMESTACK_MANAGED_HOME=1`
included), so the credential on the persistent home stays. The Site
initializer is not called: no Site is created, no credential rotated or
reinstalled, no root session opened. Review opens the same verified Pangolin
context and requires a completed (`Online`) journal whose Site, IDs and saved
credential match Pangolin; after the replacement the connector's own Site must
be online. Migration and restarts after environment changes use the same Site
readiness instead of the manifest health check. Containers still initializing
cannot be updated or migrated, and the managed selector cannot be edited in
maintenance. Step 8 connectors have no image update at all: replacing their root
disk would need root SSH to restore the credential, so that path was removed.

### Known limitations

- Removing an instance in maintenance deletes its container and home but leaves
  its Pangolin Site and desktop journal; delete the Site in Pangolin and the
  journal files explicitly before reusing that CTID.
- The Site's dashboard URL comes from Step 3 discovery; a changed URL rewrites
  the same credential on resume, never a new identity.
- Pangolin online status depends on Pangolin's own reporting interval; the
  wait is 180 s.
- Live behavior is unverified: no real LXC, root `pct exec`, Pangolin Site
  creation or online transition has been exercised by this integration.

### Remaining isolated live verification

A separately authorized run in `homestack-test` should deploy one new Site
from image `0.1.2` or later, interrupt and resume initialization, check that
the credential is at `/home/app/pangolin-site/site.json` and the Site is online,
replace its image and check that the Site stays and comes back online without
a root session, and finally remove the test Site, journal, container and home.
## Verification and production adoption boundary

Focused temporary verification uses synthetic credentials/certificates,
in-memory PVE responses and a local HTTPS fixture. It checks planning and
apply-time prerequisite failures, managed selection before start, common
metadata, TLS-variable cleanup, secure configuration/token upload, post-reboot
checks, interrupted initialization/reviewed recovery without requiring a second
volume allocation, refusal to replace a
missing home, the then-blocked Site and the shared legacy initializer. The real HTTPS
client performs certificate and hostname validation; the fixture does not
represent a real LXC, application disk, Proxmox restart or PVE token authority.
The three retained `hs-deploy` volume/recovery safety checks remain intact.
No permanent test suite or production operation is added.

The Site integration was checked the same way, temporarily: in-memory PVE
responses, a fake root channel and private temporary journals. It checks the
missing-context review problem and apply refusal before any PVE call, the
Step 7 record requirements, per-CTID names and journals, credential and journal
before any Pangolin request and their reuse, Site-step reconciliation after an
uncertain creation, refusal of leftovers, changed homes and foreign journals,
selector before start and prepare, start, install, online order, stdin-only
credential transfer with the explicit selector, Site recovery predicates and
Site readiness replacing the manifest probe. Pangolin itself, the tunnel and
root SSH were not exercised.

The authorized [2026-10-23 live verification](managed-api-live-verification-2026-10-23.md)
subsequently exercised a new API in `homestack-test`, real bootstrap/reboots,
direct guest-file persistence and same-CTID rootfs replacement. It found and
fixed PVE trailing-newline ownership comparisons and identical-release reinstall
being skipped. The test instance, holder and test-created disks/templates were
removed; the six production containers remained unchanged. See the report for
measured downtime, exact scope and remaining limitations.

Neither the three existing Site connectors nor the three API containers are
adopted, stopped or changed, no Site credential is migrated or rotated, and
no Resource, domain or DNS entry is created.

Later production adoption requires separate owner authorization, an inventory
of each legacy container's ownership/state/mounts, certificates and token files,
and each Site's independent identity and desktop recovery journal. It needs an
approved volume preparation/data transfer plan, application-specific persistence
verification and separately approved metadata reassociation. None is performed
here. Legacy API installation/update remains on its existing ownership path;
common identity takes priority for newly managed instances.

API numeric Git tags and `scripts/tag-api.py`, Site prefixed Git tags and numeric
OCI versions are unchanged. The Site integration changes client/deployer code,
not the API server; no API image release is required. Its managed runtime is
first published as `ghcr.io/hipotures/homestack-pangolin-site:0.1.2` from Git
tag `pangolin-site-0.1.2`; `0.1.1` remains the legacy Step 8 default.
Publishing any image authorizes no production deployment.
