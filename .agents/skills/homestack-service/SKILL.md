---
name: homestack-service
description: Author and validate an application OCI image and portable HomeStack service manifest for deployment as an unprivileged Proxmox LXC. Use when preparing a new HomeStack service, choosing a minimal compatible base image, implementing HTTP health and telemetry, measuring runtime resources, planning persistent data and instance topology, or reviewing .homestack/*.yaml and image-release workflows.
---

# HomeStack service authoring

**HomeStack is the deployment system; this skill is the application author's
contract with it.** Build a real working OCI application image, validate
it locally, and declare its operating requirements. HomeStack consumes a
published image and a manifest, then creates and manages unprivileged
Proxmox LXC application containers. The application agent does not need
to understand HomeStack internals or operate the Proxmox cluster.

## Authoring target and current reader compatibility

- **Phase 1: MIGRATE THE REAL MANIFEST NOW.** Use
  [manifest-schema-v2.md](manifest-schema-v2.md) as the authoritative
  schema-2 authoring contract. When adapting an application to this skill,
  update the actual `.homestack/<id>.yaml` **in place to `schema: 2`**,
  including exact proposed CPU, RAM, swap, disposable root and persistent
  volume (where needed), production instance count and placement.
  Do not leave the production file at schema 1, create a detached example
  or postpone this YAML migration to phase 2.
- **Temporary reader incompatibility:** [manifest-schema.md](manifest-schema.md)
  documents legacy schema 1. The current HomeStack CLI parser accepts only
  schema 1 and will reject the newly migrated YAML until phase 2 updates
  the consumer. This is a known phase boundary, not a reason to suppress
  the migration. Report it explicitly; never claim that current HomeStack
  successfully parsed or deployed schema 2.
- **Local build and measurements:** [image-build-preflight.md](image-build-preflight.md)
  covers choosing the runtime base image, producing a real OCI image,
  running tests (preferably including LXC/Incus) and measuring resources.
- **Telemetry:** [service-status.md](service-status.md) defines the
  existing runtime HTTP protocol independently of manifest version.

Phase 1 modifies application repositories and their YAML manifests.
Phase 2 modifies the HomeStack CLI and combines Deploy/Manage under one
Deployment reconciliation flow. Do not mutate production guests in phase 1.

## Division of responsibility

**Application agent owns:**

- Application architecture, framework and actual runtime dependencies.
- Selection and explanation of the base OCI image (Alpine, minimal static
  runtime, slim glibc distribution, etc.), production Containerfile,
  entrypoint, health/telemetry handlers and persistence layout.
- A reproducible release workflow, the portable service manifest and
  representative local runtime/resource verification.
- Concrete measured proposed resource values and production topology in
  the actual schema-2 `.homestack/<id>.yaml`.
- Documentation of any deployment-time configuration variable names
  and bootstrap needs, **never** their secret values.

**HomeStack and the operator own:**

- Selection of release digest, PVE nodes/storage, CTIDs, IPs, networking,
  Pangolin integration, scope/credentials and deployment authorization.
- The final CPU, memory, swap, rootfs and data volume sizes. The manifest
  provides initial proposed values; the operator may override any of them.
- A single **Deployment** flow: read manifest plus saved operator
  overrides; discover actual state; show differences; let the operator
  select/skip individual corrections; review; apply; verify. The first
  installation is Deployment against an empty actual state.
- Preserving existing instance identity and data during safe updates and
  proposing missing production instances without altering unrelated
  production or test guests.

These Deployment reconciliation features are **future HomeStack work**.
Do not claim they already exist or try to implement them while only
adapting an application repository.

## Authoring workflow

1. **Inspect reality.** Read the application's repository instructions,
   running code, dependencies, image/CI workflows, listen sockets,
   configuration, state paths, health/readiness and secrets-injection
   interfaces. Never invent an endpoint, mount or dependency.
2. **Define a service boundary.** One service manifest means one
   independently deployable LXC. Multiple processes or ports in that
   LXC remain one service. Do not split it into pretend independent
   manifests.
3. **Make the image-base decision.** Consult
   [image-build-preflight.md](image-build-preflight.md). Prefer the
   lightest compatible maintainable production runtime, not Debian or
   Alpine by rote. Determine musl/glibc, native libs, certificates for
   outbound requests, runtime users and process supervision before
   writing the Containerfile. Present meaningful alternatives to the
   owner when necessary; preserve an already working build unless a
   change has a concrete benefit.
4. **Build the OCI image.** Use the application's real production
   Containerfile, multi-stage build and available cache. The
   Containerfile and CI specify `FROM`/packages/build steps; the
   HomeStack YAML does not repeat them. Target a single
   `linux/amd64` image with a working `ENTRYPOINT`/`CMD`.
5. **Choose the state model and local paths.** Decide whether any
   information must survive a disposable-root replacement. Use a
   dedicated `/home/app` volume only when durable application data
   actually requires one. Bootstrap configuration/secrets that can be
   reliably re-injected do not automatically justify a persistent
   volume. Follow the persistence rules below.
6. **Define real HTTP/TCP interfaces.** When the application serves
   HTTP, implement health and HomeStack telemetry on its existing
   listener. Use local HTTP for newly built services behind Pangolin;
   do not invent additional internal TLS. Existing TLS-only software
   must be changed at runtime before documenting an HTTP port.
7. **Assess instance topology.** Determine the initial number of
   production instances and whether they can run independently.
   Multiple local SQLite stores are separate data sets, not database
   replicas. Do not confuse PVE HA with application replica count.
   An additional *isolated test* instance is permitted even when the
   normal production deployment creates one instance.
8. **Build and execute preflight.** Test the **built** image at the
   intended exact CPU/RAM/swap/root and persistent-volume capacities,
   not just compilation. Exercise cold boot, realistic work, readiness,
   telemetry, restart and fresh-root replacement. Prefer a local LXC
   application runtime through Incus when available, then isolated
   PVE integration only when separately authorized. Record evidence,
   limitations and resource values. Do not call Docker an LXC test.
9. **Migrate the active manifest in place.** Read
   [manifest-schema-v2.md](manifest-schema-v2.md). Keep the service ID and
   image repository, but replace the existing `.homestack/<id>.yaml` with
   complete `schema: 2` YAML. Populate all resource/persistence/topology
   values using real application facts and preflight evidence, not the
   example numbers in the contract. Validate YAML syntax and the
   schema-2 contract independently; the old HomeStack CLI is not
   a schema-2 validator. Do not create only a draft outside `.homestack/`.
   Document the temporary parser incompatibility; do not revert the
   manifest to schema 1 merely to pass the obsolete consumer.
10. **Document and release.** Document entrypoint, image base,
    architecture, runtime paths, port, persistence decision, test
    observations, non-secret deployment settings, and image-publishing
    workflow. Follow the image release standard below. A change only
    to proposed resource sizes or instance count does not require a
    new image release.

## Persistence: disposable root vs durable state

The root filesystem is disposable and recreated from the OCI image.

**Stateful service:** Keep all durable application data on the separate
`/home/app` volume; prefer the application's native data-path option.
Keep SQLite database, `-wal` and `-shm` together; use the SQLite backup
API for live copies. Use subdirectories such as `/home/app/data`.
Never bake state into the image or initialize an expected existing
volume from scratch.

The image keeps `/home/app` empty. The entrypoint must create
required subdirectories after the mount exists, without resetting
data, and refuse to run when a required persistent mount is missing,
a symlink or unexpectedly the root filesystem. Do not declare an OCI
`VOLUME`: HomeStack owns attaching `mp0`. For a fixed non-configurable
state path, a carefully checked link into `/home/app` is possible,
but prefer direct native configuration. Handle UID/GID ownership safely
for unprivileged LXC; never use world-writable modes to hide errors.
Send routine logs to stdout/stderr unless retention is required.

**Stateless or reconstructible service:** Schema 2 supports
`persistence.mode: none`. HomeStack would then create no `mp0`.
The app must start safely on a new root with its deployment settings,
tokens and other transient configuration re-injected through the
authorized mechanism. Do not claim a service is stateless if its
irreplaceable data or credentials are available only on the discarded
root. Do not infer a persistent mount merely because the service
temporarily stores its injected configuration on root.

**Legacy schema-1 limitation:** The current HomeStack CLI requires
`persistence.mount: /home/app` and always creates `mp0`. The phase-1
manifest migration nevertheless MUST use the new schema-2 shape, including
`persistence.mode: none` where appropriate. Do not force new semantics
into schema 1. The later reader/deployer/initializer implementation
must make these declarations operational.

Replacement must retain a stateful service's *original* home volume.
Never substitute an empty home when trying to recover a missing
existing instance. Deletion or shrinking durable storage requires
a distinct explicit owner-approved operation; a metadata change
is not permission for destructive volume cleanup.

## Instance topology and deployment defaults

Legacy schema 1 has `deployment.multi_node` and
`deployment.state_model`, but these fields are not the desired-state
engine. During phase-1 migration, replace the legacy shape with the
schema-2 fields.

In schema 2:

- `deployment.instances` is the **desired production count**, e.g.
  1 for one stateful SQLite instance or 3 for three independently
  functioning service API instances. It is **not** a global maximum.
- `deployment.placement: distinct_nodes` requests one production
  instance on each of the required distinct eligible nodes. One
  reviewed Deployment operation plans the complete group. Node and
  storage choice remain operator decisions.
- `deployment.hostname_prefix` may yield `service-1`,
  `service-2`, `service-3`. Stable identity is more important than
  matching a name string.
- `deployment.state_model` documents whether production state is
  `local`, `stateless`, `independent_local` or `external`. Multiple
  independently configured APIs are not automatically disk replicas.
- User-created independent test instances do not count as missing
  or extra *production* instances and use separate identities,
  IPs and persistent data where appropriate.
- A missing instance is a proposed per-instance correction. An
  intentional operator resource override must not be undone
  inadvertently on the next Deployment review.

This is service topology, **not Proxmox HA configuration**. Do not
enable HA, automatic recovery, shared storage or background
reconciliation as a side effect of authoring a manifest.

## Network, health and TLS boundary

For newly developed HTTP services, the local runtime serves plain HTTP
on the private, firewall-controlled LAN. Pangolin handles external
HTTPS termination, certificates and remote authentication. Do not
require a local TLS certificate or embed a Caddy/NGINX TLS hop merely
because an external browser needs HTTPS; use the external Pangolin
address for secure-context browser functionality. Do not add separate
local login to an app solely for this infrastructure boundary.
Application-level account/security requirements remain independent.

The standard HTTP health endpoints use the "Health Check Response Format
for HTTP APIs" (draft-inadarei-api-health-check):

- `GET /health/live`: process liveness only, `200`
  `{"status":"pass"}`.
- `GET /health/ready`: dependencies necessary for work are ready;
  `200` with `"status":"pass"`, or `503` with `"status":"fail"`
  and optional safe `checks` details.
- `Content-Type: application/health+json`; do not leak internal
  errors, credentials or application data.

The manifest's HTTP healthcheck declares its actual port and
`/health/ready` (expected `200`). Only use HTTP if the application
really serves that path; use TCP when it is only possible to verify
port acceptance, or omit health when neither is justified. A TCP
connection is not proof of application readiness. Do not break
existing legacy health paths relied upon elsewhere.

## HomeStack service telemetry

HTTP services implement `GET /.well-known/homestack/status` with
`Content-Type: application/json`, according to
[service-status.md](service-status.md). The path is fixed, on the
same HTTP listener used for the application's healthcheck, and
does not need a separate manifest field or listener.

Report process uptime and cgroup-v2 CPU, memory, pressure (PSI),
I/O plus actual mounted filesystems including `/` and `/home/app`.
Keep `load_average: null`: host `/proc/loadavg` is not valid LXC
container usage. If there is no `/home/app` mount, report it as
missing/unmounted, not as a disguised rootfs measurement or fake
zero free space. Distinguish missing controllers, stale samples and
measurement failures from measured zero. No arbitrary shell/SSH
execution, no secret values, and no public exposure of this
management endpoint through Pangolin.

Prefer native APIs of the service's language and a bounded,
cached sampler. The existing
`crates/hs-service-status` crate in HomeStack predates the
`container` section and still reports host load; do not reuse it
as if current. Do not require Rust or a new HTTP daemon for an
application in another language. Verify values against the
actual guest cgroup and mounted filesystems.

## Image release standard

Every service has an independent SemVer release line.

1. Use an annotated Git tag `<id>-MAJOR.MINOR.PATCH`. Several
   services in one repository, and upstream fork tags, must
   not collide.
2. Provide an idempotent safe tagging script (for example
   `scripts/tag-image.py`) that fetches remote tags, chooses the
   next patch from this service's numeric versions, requires
   clean already-pushed HEAD and has `--dry-run`. Do not pick
   patch numbers by hand or move released tags.
3. On a release tag, CI publishes a **single**
   `linux/amd64` OCI image `<registry>/<image>:MAJOR.MINOR.PATCH`
   with no prefix or `v` in its image tag, anonymously pullable
   by the Proxmox node.
4. A manual development build publishes `dev-<commit>` for
   testing without consuming a release number. When supported
   by the repository's CI, promote the **same tested image** to
   the immutable release tag without rebuilding it.
5. Ordinary pushes run checks; they do **not** publish an image.
   CI must refuse to overwrite a released tag. A bad image
   gets a new patch release, never a retag of published bytes.
6. Preserve an established working build/publish workflow,
   only adapting what is necessary for this contract.

After committing and pushing, release a new image when actual
application/runtime contents changed: application code, runtime
dependencies, Containerfile, entrypoint or files baked into the
image. A change only to **deployment resource sizes, intended
instance count or placement in the YAML** does not change the
OCI artifact and requires no new image tag. An image change
that affects health behavior, state layout or startup must be
tested and released. Write this rule into the application
repository's `AGENTS.md` to survive later agents that do not
load this skill.

## Canonical skill and distribution

This directory in `hipotures/homestack` is the **canonical**
`homestack-service` skill. Other application repositories (such
as Tutor) carry a **vendored copy**, not a separate authoritative
fork. Edit the canonical skill first, then distribute a
versioned snapshot of the **whole** directory, including all
linked references. Agent-specific paths such as `.agents/skills/`
and `.claude/skills/` may be aliases/symlinks within a repo.

Until a synchronizer exists, a consumer repository's copy can
be stale; agents must compare its source commit with the
canonical HomeStack main before making policy-based changes.
Recommended follow-up is a checked-in synchronization script
and GitHub Actions job that fetches a *pinned* HomeStack commit
or release, checks the entire skill directory, and opens a
reviewable PR for each consumer repository. Never silently
overwrite consumer code or pull live mutable content at
runtime. A dedicated skills-only repository/submodule is
possible later, but is **not** required for this phase and
should not be introduced without an explicit decision.

## Refuse / do not assume

- Secrets, cert/key material, real tokens, host paths, host
  addresses, Proxmox node/storage IDs, DNS or Pangolin config
  in a portable service manifest or image.
- Silent changes to production guests, PVE API, storage, image
  tags, database files or persistent volumes while writing a
  manifest or skill.
- Docker-in-LXC, weak SSH credential fallbacks, privileged
  containers or unsafe mount permissions as build shortcuts.
- A fabricated local test, cgroup measurement or successful
  healthcheck; unmeasured sample values asserted as facts.
- Treating a second empty SQLite service as a synchronized
  replica, or prohibiting separately isolated manual tests
  based on the declared production instance count.
- Failing to migrate the actual `.homestack/<id>.yaml` to schema 2
  when adopting this skill in phase 1, or claiming the current
  HomeStack CLI already supports that file.
