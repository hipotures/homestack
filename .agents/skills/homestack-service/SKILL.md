---
name: homestack-service
description: Prepare an application repository for deployment by HomeStack as unprivileged Proxmox LXC containers built from an OCI image. Use when asked to make a service deployable through HomeStack, to write or review .homestack/*.yaml manifests (resources, persistence, instances, Pangolin exposure), to choose and test a minimal base image, or to implement the HomeStack health and telemetry endpoints.
---

# HomeStack service authoring

HomeStack deploys a service as one or more unprivileged Proxmox LXC
containers created from the service's published OCI image. You prepare the
application side only: the image, its runtime data layout, its health and
telemetry endpoints and a portable manifest that tells HomeStack how to
deploy it. The exact manifest contract is in
[manifest-schema.md](manifest-schema.md); follow it literally.

Manifest schema 1 remains supported without migration. Schema 3 opts into
three files at `.homestack/services/<id>/{service,defaults,inputs}.yaml`:
invariant service requirements, operator-overridable deployment defaults,
and typed application inputs. Read the schema-3 section of the contract
before authoring these files. Manifest schema 3 and HTTP service-status
telemetry schema 3 are independent contracts.

This contract does not depend on the application's language or framework.
It describes how the application behaves when HomeStack deploys it; it does
not change what the application does.

## Boundary

- You own: the Containerfile and base image choice, the entrypoint, where the
  application writes state, health and telemetry endpoints, the release
  workflow that publishes the image, and `.homestack/*.yaml` with the
  service's default resources, persistence, instance count, placement and
  Pangolin exposure.
- HomeStack and its operator own: nodes, CTIDs, IPs, storage, the domain,
  Pangolin sites and credentials, and every secret. The operator may change
  any manifest default in the deployment form. Never put environment-specific
  values or secrets in the repository, the image or the manifest, and never
  run Proxmox operations yourself.

## Steps

1. Read the repository's own instructions and the real runtime: how the image
   is built and published, the start command, listening ports, a real health
   endpoint, every path the application writes and every setting it needs at
   start. Do not invent any of them.
2. Decide what is one service: one independently deployable LXC. Several
   processes or ports in one container are one service with several ports.
   Do not add a manifest for something that is not deployable on its own.
3. Choose the base image and build the image as described in
   [image-build-preflight.md](image-build-preflight.md): the lightest runtime
   the application actually works on without substantial code changes.
4. Decide whether the service has durable state (see Persistence below). If
   it has, put all of it under `/home/app/<directory>`; if not, leave `persistence:`
   empty and make sure the service starts on a fresh root.
5. Implement the health endpoints and the telemetry endpoint on the existing
   HTTP listener (see below).
6. Decide the instance topology: how many production instances a normal
   deployment creates and whether they go one per node.
7. Run the built image locally with the exact resources you intend to
   declare, measure it, and adjust until it works within them
   ([image-build-preflight.md](image-build-preflight.md)).
8. Write `.homestack/<id>.yaml` per the schema with the measured resources,
   persistence, topology and Pangolin exposure. Write every top-level section;
   leave `persistence:` or `exposure:` empty to switch that feature off.
9. Set up image releases to the standard below. Adapt a working build to it
   rather than replacing it wholesale.
10. Document the service's runtime contract next to its image: base image and
    why, ports, state paths, measured resources and the workload used, and the
    settings HomeStack must provide at deployment time (never their values).

## Persistence

Prepare fixed nonroot writable directories in the OCI image with their
actual runtime UID/GID and restricted modes. HomeStack does not create or
chown arbitrary paths on Proxmox root filesystems. For a scratch runtime,
use a build stage and `COPY --chown`. Rootfs-local queues survive an ordinary
CT restart and are lost on image replacement; disclose this in the image's
runtime documentation and Review. They do not imply a persistent home.

The root filesystem is disposable: HomeStack recreates it from the image on
every update. Durable data lives only on the optional home volume mounted at
`/home/app`.

**Stateless service** (writes nothing it needs later, e.g. an API whose data
lives elsewhere): leave `persistence:` empty in the manifest. HomeStack
creates no home volume. The service must start correctly on a fresh root with only its
image and the settings supplied at deployment. Temporary files on root are
fine; anything that must survive an update is not.

**Stateful service** (e.g. a local SQLite database): fill `persistence`
with the volume size and directories.

- Prefer the application's own setting (an environment variable or config key
  pointing at `/home/app/data`) over symlinks. If a path cannot be configured,
  have the entrypoint link it into the mount, and check the link never hides
  or replaces existing data.
- SQLite databases and their `-wal`/`-shm` files stay together inside the
  mount; never copy a live database with `cp`, use the SQLite backup API.
- Keep `/home/app` an empty directory in the image; nothing baked there
  survives the mount. Do not declare `VOLUME` and do not mount anything in the
  image.
- In the entrypoint, create the needed directories with `mkdir -p` after the
  mount is present; never reinitialise, overwrite or migrate away existing
  data.
- Refuse to start when `/home/app` is a symlink or not a mounted volume, unless
  an explicit opt-in for throwaway tests is set.
- The LXC is unprivileged: run as container root or a fixed service user that
  owns its directories; never fix permissions with world-writable modes.

Logs go to stdout/stderr unless there is a real reason to keep them.

## Instances and placement

`deployment.instances` is how many production instances one deployment
creates. `deployment.hostname` names them: `<hostname>` for one instance,
`<hostname>-1`, `<hostname>-2`, … for several; clients reach them by that name
in the local DNS, never by address. `deployment.placement` says how they
are spread over the Proxmox cluster:

- `spread`: one instance per node. On a cluster with fewer nodes than
  `instances`, HomeStack creates one per available node, never two on the
  same node. Use it for a stateless service that clients can reach on any
  node, e.g. an API deployed with `instances: 3`.
- `single`: all instances on one node.

`deployment.state_model` states what the instances share:

- `stateless`: nothing local; any instance can serve any request.
- `local`: each instance owns the data on its own home volume. A second
  instance is a separate, initially empty data set, never a replica. A
  service with one SQLite database is `local` with `instances: 1`.
- `external`: shared state lives in an external service designed for
  concurrent clients.

The count is the normal production deployment, not a limit. The operator can
always deploy an additional, separately named test instance (for example to
check a new image or UI); it starts with its own empty home.

This is not Proxmox HA: do not configure HA, replication or shared storage
as part of authoring a service.

## Network, health and TLS

The service serves plain HTTP inside the private LAN. Pangolin terminates
public HTTPS, handles certificates and remote authentication. Do not bundle
certificates, a TLS-only proxy or an extra login only for this purpose; the
application's own accounts and permissions are separate. An existing
TLS-only service must actually be changed to serve HTTP before its manifest
declares an HTTP port.

HTTP services expose health in the "Health Check Response Format for HTTP
APIs" (draft-inadarei-api-health-check), never as free text:

- `GET /health/live`: the process answers and its heartbeat (below) is
  fresh; checks no dependencies directly. `200` with `"status":"pass"`.
- `GET /health/ready`: the service can do its job (fresh heartbeat, database
  and other required dependencies reachable). `200` with `"status":"pass"`, or `503`
  with `"status":"fail"`; each dependency under `checks`, e.g.
  `{"status":"pass","checks":{"database:connection":[{"status":"pass"}]}}`.
- `Content-Type: application/health+json`, a JSON object, no secrets or
  internal error details in the body.

An open port or a page that still loads does not prove the service works:
the main worker can hang while the listener keeps answering. Every HTTP
service therefore runs a heartbeat:

- Every heartbeat interval the service performs one real, cheap unit of
  work through its main serving path, normally an HTTP request to its own listener on a
  lightweight route that goes through the same workers and required
  dependencies (e.g. one trivial database read). On success it records the
  time with a monotonic clock and increments a counter that starts at 0 at
  process start.
- A timer that only increments a value without passing through the main path
  is not a heartbeat. The heartbeat writes nothing to disk.
- The interval comes from the manifest's `runtime.healthcheck.heartbeat_seconds`
  (default 60). HomeStack passes it to the container as
  `HOMESTACK_HEARTBEAT_SECONDS`; the service reads it at start and uses 60
  when it is absent.
- The heartbeat age is the time since the last success, or since process
  start before the first one. Above three intervals (three missed beats) both
  `/health/live` and `/health/ready` return `503` with `"status":"fail"`.
- Both endpoints report the heartbeat as checks: `heartbeat:age` (integer
  seconds, `observedUnit` `s`) and `heartbeat:count`. Report the age, not a
  wall-clock timestamp, so time zones and clock offsets never matter. A count
  lower than before means the process restarted; an age that keeps growing
  means it is stuck.
- When health is served by a component next to an application you cannot
  modify, that component sends the heartbeat request to the application's
  own listener.

Consumers judge health only by the status code, so Pangolin and HomeStack
detect a stuck service without parsing the body; a process that does not
answer at all fails by timeout.

Example `/health/live` body:
`{"status":"pass","checks":{"heartbeat:age":[{"status":"pass","observedValue":12,"observedUnit":"s"}],"heartbeat:count":[{"status":"pass","observedValue":4211}]}}`

The manifest's `runtime.healthcheck` uses `/health/ready` with
`expected_status: 200`; HomeStack and the Pangolin resource both use it. Use
`tcp` only when the service has no HTTP endpoint, and no health check when
neither is real. Keep pre-existing health URLs working if something else
depends on them.

## Pangolin exposure

Declare `exposure.pangolin` when the service should be reachable through
Pangolin: the port to expose, the resource name and the subdomain. HomeStack
creates one Pangolin resource for the service with every instance as its
target, under the domain the operator selects. Leave `exposure:` empty for
services that are only used inside the LAN or have no inbound port.

## Service telemetry

For schema-3 third-party images, keep the application's actual declared
health endpoint. HomeStack does not infer a status telemetry endpoint or
inject a HomeStack heartbeat. Do not require an upstream application to
implement HomeStack telemetry schema 3. Container resource metrics come
from the PVE agent independently. The following native telemetry requirements apply
to HomeStack-integrated applications, including existing schema-1 services.

HTTP services implement `GET /.well-known/homestack/status`, returning
`application/json` according to [service-status.md](service-status.md). Read
that contract when implementing or reviewing the endpoint. The path is fixed
and served on the healthcheck's HTTP port; it needs no manifest field. It
reports only liveness and the process's RSS: CPU, memory, pressure, I/O,
filesystem space and uptime of the container come from `homestack-pve-agent`
and Proxmox through Prometheus, so the service does not measure them.

Read the RSS from `/proc/self/status` when a request arrives, on the existing
HTTP server; no additional daemon, timer, background sampler, SSH or shell
commands. The heartbeat is the only periodic work a service does for
HomeStack.

Verify with local HTTP requests that the RSS matches `VmRSS`, and that a
stalled main path (blocked dependency or paused worker) turns both health
endpoints into `503` and telemetry `status` into `fail` within three
heartbeat intervals.

## Release standard

In this repository, the derived Collector image is built from
`deploy/otel/Containerfile`, pins the official distribution by version and
digest, and uses independent `otel-X.Y.Z` Git tags to publish
`ghcr.io/hipotures/homestack-otel:X.Y.Z`. Run
`python3 scripts/tag-image.py --service otel` on clean, pushed HEAD for its
next release. Image validation runs before publication. Ordinary branch
pushes do not publish it. Use the image's baked configuration and OCI
startup command; supply only declared application environment values.

Every service has its own independent SemVer version line:

1. **Tags.** A release is the annotated Git tag `<id>-MAJOR.MINOR.PATCH`
   (`<id>` is the manifest `id`, e.g. `example-0.1.2`). The prefix keeps
   several services in one repository, and upstream tags in forks, apart.
2. **Automatic numbering.** The repository has a tagging script (create it if
   missing, e.g. `scripts/tag-image.py`) that fetches tags, takes the highest
   `<id>-X.Y.Z`, creates the next patch version on a clean HEAD that is
   already pushed to its upstream, and pushes only that tag. It refuses a dirty
   tree, an unpushed HEAD and a HEAD that is already released, and has a
   `--dry-run`. Never pick version numbers by hand; bump minor or major only
   when the owner asks.
3. **Release build.** CI builds the image when such a tag is pushed and
   publishes `<registry>/<image>:MAJOR.MINOR.PATCH` (no prefix, no `v`) as a
   single `linux/amd64` image, pullable anonymously by the Proxmox node.
4. **Development build.** A manual CI run publishes `dev-<commit>` for testing,
   without a new version.
5. **Ordinary pushes** run checks only and never publish an image.
6. **Immutability.** Never move, delete or force-push release tags, and never
   overwrite a published version: CI refuses to publish a version that already
   exists. A bad release is fixed by the next patch release.

Not every change is a release. After committing and pushing a completed
change, decide whether the image changes: application code or dependencies
it uses, the Containerfile, entrypoint or files baked into the image. If so,
run the tagging script. HomeStack reads the manifest from the repository, so
a change only to the manifest (resources, persistence size, instances,
placement, exposure) needs no image release; neither do documentation,
tests, CI checks or tooling. Record this rule in the repository's agent
instructions (`AGENTS.md`) so agents working without this skill follow it too.

## Refuse

Cluster-specific values or secrets in the manifest or image; shell commands
in manifests; Docker-in-LXC or privileged containers; resource values that
were not measured on the built image; claiming persistence, multi-instance
safety or health that the application does not actually have.
