# OCI image design, local build and LXC preflight

This reference is part of the canonical `homestack-service` skill. Read it
**before** choosing a Containerfile and before recording resource sizes in a
schema-2 `.homestack/<id>.yaml` manifest. The app author is responsible for actually
building and executing the final image, not merely producing valid YAML.

## 1. Establish the real runtime and propose an image base

Inspect actual language/toolchain, linked libraries, runtime processes,
architecture, network listeners, TLS behavior, state paths, entrypoint,
system dependencies, and existing build/CI workflow. Determine which
packages are **build-only** and which must exist in the final runtime.
Do not infer requirements from a project name or generic ecosystem advice.

Prefer the smallest practical **compatible and maintainable** production
runtime. Consider, for example:

- Static Rust or Go: multi-stage compilation and a minimal Alpine,
  distroless-like, or scratch runtime **only if** certificates, timezone,
  users, libc, resolver, native library, and entrypoint requirements work.
- Rust: distinguish musl/static from glibc/dynamically linked binaries.
  Do not copy a glibc-linked binary into Alpine and assume it will run.
- Python: test wheel/native-extension availability with musl; Alpine may
  cause complex source builds while a slim glibc image can be a better
  overall choice.
- Node.js: check native modules and Node ABI, not only application JS.
- Multiple services in one LXC: identify needed process supervision and
  graceful shutdown; do not add a full OS distribution just by habit.

Compare material alternatives by final image contents, compatibility,
security updates, reproducibility, build time and runtime footprint.
If trade-offs materially affect the owner (for example Alpine vs Debian
slim, or static vs dynamic linking), propose the choice and ask before
switching an established approach. Otherwise choose conservatively,
document why and verify.

**The Containerfile is the source of truth for the base image, packages,
multi-stage build and entrypoint.** Do not duplicate its `FROM` lines or
build commands in the HomeStack manifest. A schema-2 manifest declares
deployment resources and how to reach the published OCI image. Image
creation is the agent's responsibility, not a hidden HomeStack build step.

## 2. Build the actual production artifact locally

- Preserve the repository's working Containerfile and CI arrangement.
  Use the same production build target, architecture (`linux/amd64`),
  feature flags and runtime inputs as release CI. Favor existing cached
  stages and pinned dependencies rather than gratuitous clean builds.
- Prefer a locally available OCI builder such as Podman/Buildah or Docker.
  Do not require Docker **inside** Proxmox LXC and do not install an
  unrequested daemon on the PVE host.
- Capture the resulting image identity and inspect its runtime:
  base distribution, files, entrypoint/command, effective user, exposed
  ports, working directory, dynamic libraries and dependencies.
- Validate that the image starts without a build toolchain, compiler,
  package manager or privileged service manager at runtime unless truly
  necessary. Never rely on interactive login, SSH or `sudo`.
- Test the **built artifact**, not a local development process with host
  dependencies. A successful compiler run is not a successful container
  boot. OCI image compression size does not equal unpacked rootfs size.

Do not start an expensive remote GitHub Actions build merely to discover
an entrypoint, missing library or resource-limit failure that can be
checked with the local image.

## 3. Exercise a bounded local container runtime

Recommended sequence, in order of closeness to production:

1. Run the built OCI image in an isolated local OCI runtime with the
   declared CPU, memory, swap and root/data size limits and no privileged
   workaround. Verify process exit status, stdout/stderr and HTTP health.
2. If Incus is available, run the same OCI application image as an Incus
   application container (LXC-based) with equivalent resource settings,
   data mounts and network access. Incus consumes OCI images through
   an OCI registry remote; it **does not** build Containerfiles. The
   local builder and Incus have distinct roles. Prefer a local/test
   registry or a temporary artifact approach supported by the actual
   tool version; do not publish a production release just to do this.
3. A separately authorized, isolated Proxmox HomeStack test pool is
   the higher-fidelity final integration check. Never use production
   containers, credentials, storage or network identities for preflight.

Do not claim that an OCI-runtime smoke test is a Proxmox LXC test, or
that Incus perfectly reproduces Proxmox container behavior. Kernel,
user-namespace mounts, UID/GID mapping, cgroup setup and rootfs limits
can differ. If tools or permissions are missing, report what was and
was not tested instead of fabricating a pass. Ask the owner before
installing a new container runtime or changing the local machine.

## 4. Functional and persistence gates

Run the **final image** at the proposed resource values. Exercise:

1. Cold start and a readiness check: the actual listener, declared
   port, `GET /health/live` and `GET /health/ready` if HTTP.
   Dependency readiness is distinct from process liveness.
2. Typical representative work, not only an idle process; include
   expected burst/concurrent operations and relevant error paths.
3. A clean stop and restart, then a fresh-root instance with the
   previously used persistent volume attached for stateful services.
   Check that durable files and app data are unchanged.
4. For `persistence.mode: none`, a fresh-root/empty instance must start
   with the configuration and secrets supplied again through their
   supported deployment mechanism. Do not assume transient root files
   magically persist after replacement.
5. With persistence enabled, test a missing, incorrect, or unmounted
   `/home/app` and require a safe startup failure, not silent fallback
   onto disposable root. SQLite data, WAL and SHM must remain together.
6. With an HTTP service, verify standard HomeStack status telemetry
   with native measurements; keep inaccessible data, missing mounts
   and disabled controllers distinct from zero. Use the existing
   listener, not a new management daemon.
7. Local plain HTTP behind network restrictions, with external TLS
   terminated by Pangolin where applicable. Do not silently bundle
   Caddy, certificates or a second TLS layer into a new application
   solely to enable browser access. An existing TLS-only application
   requires a real implementation change before its health port can
   be documented as HTTP.

Use fake or isolated secrets in tests. Never copy actual production
tokens, private keys, vault databases or certificate keys into a
container image, sample manifest, test log or third-party service.

## 5. Measure and choose exact resource declarations

Choose a concrete configuration for the **running** application; do not
base it on the compiler's peak memory or GitHub runner requirements.
Use measurable evidence:

| Resource | Evidence |
| --- | --- |
| CPU | Usage under typical and burst work, quota throttling, request latency, startup cost. |
| Memory | Cgroup `memory.current`, high-water evidence where available, anonymous/file usage, `memory.events` OOM/OOM-kill, memory PSI; compare with the applied container limit. |
| Rootfs | Size of unpacked runtime files plus temporary/cache/log requirements, writable root activity and actual root filesystem free space. |
| Persistent volume | Expected durable data and growth rate, configured app paths, usable free bytes on the exact mount. |
| Swap | Explicitly test/decide `0M` or a chosen size; do not conceal insufficient RAM with unexpected swap. |
| I/O | Read/write throughput and stalls from available cgroup/PSI metrics during the workload. |

Use a finite, clearly stated representative workload. Distinguish
peak samples from long-running steady state, disk filesystem free
space from underlying thin-provisioned capacity, and container cgroup
numbers from host-wide `/proc/loadavg`. If a counter is unavailable,
say so; do not substitute host telemetry or invent values.

Adjust and rerun until the application completes the preflight under
**the precise proposed values** without OOM, unexplained crashes or
persistent-volume violations. Include reasonable headroom based on
observed workload and the owner's preference, but emit exactly one
chosen size per resource. There is no schema-level minimum/recommended
pair. Future operational measurements can justify resizing later.

Do not use example values from the schema-2 contract as evidence of a
real application's requirements.

## 6. Publish and hand off

- Confirm the local built image's runtime contract and the declared
  manifest refer to the same image/repository/ports/state paths.
- Document the chosen base image and measured deployment resources
  in the application repository, along with workload, measurement
  limits and any unverified behavior. The report is supporting
  evidence; the YAML carries the exact proposed values.
- Follow the service release standard: ordinary pushes run checks;
  a manual `dev-<commit>` image can be tested before promotion;
  an immutable numeric OCI release is created only for changes to
  the application image/runtime. A **manifest-only resource/topology
  change** does not by itself need a new image.
- Verify the newly published OCI image digest and make sure it matches
  the tested revision. If CI rebuilt rather than promoted the same
  tested artifact, treat that as a new artifact requiring validation;
  do not claim byte-identical promotion.
- Do not perform a production deploy from an application-side agent.
  HomeStack's reviewed Deployment operation owns the actual PVE
  lifecycle and is implemented in the later phase.

## Preflight reporting template

Record concrete, reproducible observations. Do not mark a test passed
without executing it.

```text
Service / commit / OCI image:
Base image and rationale:
Builder and architecture:
Runtime used: OCI / Incus LXC / isolated PVE LXC
Persistence mode and test volume:
Workload and test duration:
CPU configured / observed:
Memory configured / observed peak / OOM / PSI:
Swap configured / observed:
Root configured / used / free:
Home configured / used / available:
Health/readiness/telemetry:
Cold start / restart / fresh-root replacement:
Unverified scenarios or runtime differences:
Final proposed schema-2 resources:
```
