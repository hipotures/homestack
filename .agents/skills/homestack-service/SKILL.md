---
name: homestack-service
description: Prepare an application repository for deployment by HomeStack as a Proxmox LXC built from an OCI image, with persistent data in /home/app. Use when asked to make a service deployable through HomeStack, to write or review .homestack/*.yaml manifests, or to adapt a Containerfile/entrypoint for a persistent /home/app volume.
---

# HomeStack service authoring

HomeStack deploys a service as one unprivileged Proxmox LXC per selected node,
created from the service's published OCI image, with a dedicated persistent
volume mounted at `/home/app`. You prepare the application side only: the
image, its runtime data layout and a portable manifest. The exact manifest
contract is in [manifest-schema.md](manifest-schema.md); follow it literally.

## Boundary

- You own: the Containerfile, the entrypoint, where the application writes
  state, the release workflow that publishes the image, and `.homestack/*.yaml`.
- HomeStack owns: nodes, CTIDs, IPs, storage, volume sizes, CPU/RAM, domains,
  exposure and every secret. Never put any of these in the repository, the
  image or the manifest, and never run Proxmox operations yourself.

## Steps

1. Read the repository's own instructions and the real runtime: how the image
   is built and published, the start command, listening ports, a real health
   endpoint, and every path the application writes. Do not invent any of them.
2. Decide what is one service: one independently deployable LXC. Several
   processes or ports in one container are one service with several ports.
   Do not add a manifest for something that is not deployable on its own.
3. Put all persistent state under `/home/app/<directory>`:
   - prefer the application's own setting (an environment variable or config
     key pointing at `/home/app/data`) over symlinks;
   - if a path cannot be configured, have the entrypoint link it into the
     mount, and check the link never hides or replaces existing data;
   - SQLite databases and their `-wal`/`-shm` files stay together inside the
     mount; never copy a live database with `cp`;
   - logs go to stdout/stderr unless there is a real reason to keep them.
4. Make the image safe for an empty, then reused, volume:
   - keep `/home/app` an empty directory in the image; nothing baked there
     survives the mount;
   - in the entrypoint, create the needed directories with `mkdir -p` after
     the mount is present, never reinitialise or overwrite existing data;
   - refuse to start when `/home/app` is a symlink or not a mounted volume,
     unless an explicit opt-in for throwaway tests is set;
   - do not declare `VOLUME` and do not mount anything in the image;
   - the LXC is unprivileged: run as container root or a fixed service user
     that owns its directories; never fix permissions with world-writable modes.
5. Set up image releases to the standard below; it is required, not optional.
   Adapt a working build to it rather than replacing it wholesale.
6. Write `.homestack/<id>.yaml` per the schema. `multi_node: true` only when
   independent simultaneous instances are genuinely safe (`stateless`, or
   `external` shared state designed for it). Local SQLite means
   `multi_node: false` and `state_model: local`. A health check must be real:
   use HTTP only if the service serves that path; otherwise `tcp` or none.
   An HTTP service exposes the standard health endpoints below and the
   manifest points at its readiness endpoint. It also implements the
   [service telemetry contract](service-status.md) on that same HTTP port.
7. Verify without touching any cluster: the YAML parses and satisfies the
   schema; the service starts with an empty `/home/app`, writes its state
   there, and keeps it across a restart.
8. Document the service's runtime contract next to its image, including the
   settings HomeStack must provide at deployment time (never their values).

## Health endpoint standard

HTTP services expose health in the "Health Check Response Format for HTTP
APIs" (draft-inadarei-api-health-check), never as free text:

- `GET /health/live`: the process answers; checks no dependencies.
  `200` `{"status":"pass"}`.
- `GET /health/ready`: the service can do its job (database and other
  required dependencies reachable). `200` with `"status":"pass"`, or `503`
  with `"status":"fail"`; each dependency under `checks`, e.g.
  `{"status":"pass","checks":{"database:connection":[{"status":"pass"}]}}`.
- `Content-Type: application/health+json`, a JSON object, no secrets or
  internal error details in the body.

The manifest's `runtime.healthcheck` uses `/health/ready` with
`expected_status: 200`. Keep pre-existing health URLs working if something
else depends on them.

## Service telemetry

HTTP services implement `GET /.well-known/homestack/status`, returning
`application/json` according to [service-status.md](service-status.md). Read
that contract when implementing or reviewing the endpoint. It is independent
of the application's language and separate from health/readiness. Use the
existing HTTP server and native OS APIs; no additional daemon, SSH, shell
commands or framework replacement. Keep schema 1 YAML unchanged: the endpoint
uses the HTTP healthcheck's declared port, with a fixed path.

Report process uptime, explicitly scoped system load (1/5/15 minutes), and
an array of actual data mounts including `/` and `/home/app`, with filesystem
type and byte capacities. Detect mounts instead of assuming ext4: XFS, Btrfs,
NFS and other supported filesystems use the same contract. Missing mounts,
failed measurements and stale data must remain distinguishable from zero
free space. These observations never prove Proxmox volume ownership and
never authorize a lifecycle operation.

The HomeStack repository contains a small Linux Rust implementation in
`crates/hs-service-status`; reuse it when available. For other languages,
implement the same wire contract with native equivalents. Do not require
Rust, publish a new package, or add an HTTP listener to a non-HTTP service
merely because this reference exists. Verify with local HTTP requests and
compare filesystem values with `df -B1` or the platform equivalent; also
check an absent `/home/app` and a measurement failure without touching PVE.

## Release standard

Every service has its own independent SemVer version line:

1. **Tags.** A release is the annotated Git tag `<id>-MAJOR.MINOR.PATCH`
   (`<id>` is the manifest `id`, e.g. `tutor-0.1.2`). The prefix keeps several
   services in one repository, and upstream tags in forks, apart.
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
change, decide whether the running service changes: application code or
dependencies it uses, the Containerfile, entrypoint or runtime files, or
`image`/`runtime`/`persistence` in the manifest. If so, run the tagging script;
if the change only touches documentation, tests, CI checks or tooling, do not.
Record this rule in the repository's agent instructions (`AGENTS.md`) so
agents working without this skill follow it too.

## Refuse

Cluster-specific values or secrets in the manifest or image, shell commands in
manifests, Docker-in-LXC, and claiming multi-node safety or health that the
application does not actually have.
