# Internal services: new managed instances

The repository declares two portable schema-1 services:
`.homestack/homestack-api.yaml` and `.homestack/pangolin-site.yaml`.
HomeStack API can now initialize through common service deployment. Pangolin
Site remains blocked. No existing internal container is adopted or modified.

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
through the existing PVE API `hs_deploy::recreate` path. That algorithm and its
volume invariants are unchanged. Ordinary update/migrate manifest probing is
still TCP connectivity, not the complete initialization checks described here.
PVE metadata and guest telemetry do not prove application records were read;
this task makes no general data-persistence claim from mount metadata alone.

## Pangolin Site remains blocked

`Source::initialization_blocker` still rejects the Site image, including under
another manifest id or repository. No Site identity, credential or journal is
created or changed by this work. Its manifest has no inbound listener or generic
healthcheck; generic health remains unverified.

Future Site integration must carry the verified Step 5 private tunnel and
permanent least-privilege Step 7 organization key into Step 8's existing private
per-node credential/journal logic. It must initialize each new CT through its
existing authenticated root-channel stdin receiver, explicitly select the
managed credential path, and verify CT running plus its own Pangolin Site
online. The temporary Root key is never a substitute. This initialization-only
prerequisite does not belong in generic image replacement.

The prepared Site image's managed selector uses
`/home/app/pangolin-site/site.json`, owned by site with private modes; default
legacy behavior remains `/var/lib/pangolin-site/site.json`. The CLI's HOME
follows the selected directory. `DISABLE_SSH=true` and the existing pinned CLI
invocation remain intact. An eventual Site initializer must explicitly pass the
selector to `homestack-site-credential` too: `pct exec` must not be assumed to
inherit PID 1's environment. Site image publication/integration remains separate.

## Verification and production adoption boundary

Focused temporary verification uses synthetic credentials/certificates,
in-memory PVE responses and a local HTTPS fixture. It checks planning and
apply-time prerequisite failures, managed selection before start, common
metadata, TLS-variable cleanup, secure configuration/token upload, post-reboot
checks, interrupted initialization/reviewed recovery without requiring a second
volume allocation, refusal to replace a
missing home, Site blocking and the shared legacy initializer. The real HTTPS
client performs certificate and hostname validation; the fixture does not
represent a real LXC, application disk, Proxmox restart or PVE token authority.
The three retained `hs-deploy` volume/recovery safety checks remain intact.
No permanent test suite or production operation is added.

Real empty/reused mounted-volume startup and rootfs replacement, durable guest
file verification, real PVE environment/pending semantics and authenticated
bootstrap against an actual new LXC still require explicitly authorized
isolated targets. No live E2E result is claimed.

Later production adoption requires separate owner authorization, an inventory
of each legacy container's ownership/state/mounts, certificates and token files,
and each Site's independent identity and desktop recovery journal. It needs an
approved volume preparation/data transfer plan, application-specific persistence
verification and separately approved metadata reassociation. None is performed
here. Legacy API installation/update remains on its existing ownership path;
common identity takes priority for newly managed instances.

API numeric Git tags and `scripts/tag-api.py`, Site prefixed Git tags and numeric
OCI versions are unchanged. This handoff changes only client/deployer code and
its dependencies, not the server image or runtime; no API image release is
required. Publishing any future image authorizes no production deployment.
