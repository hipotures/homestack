# Base image, local build and resource measurement

Read this before writing the Containerfile and before putting resource values
into `.homestack/<id>.yaml`. The goal is the smallest image and the smallest
resources the application really works with, found by running the built image,
not by guessing.

## 1. Choose the base image from the application

Inspect the real runtime first: language and toolchain, linked libraries
(glibc or musl), native extensions, processes that must run together, init or
supervision needs, outbound TLS (CA certificates), timezone data, users and
the existing Containerfile and CI. Separate build-only dependencies from what
the running service needs.

Then pick the lightest base that runs the application without substantial
code changes:

- A static Rust or Go binary: multi-stage build onto Alpine (or smaller) when
  certificates, resolver, users and libc needs are covered. Do not copy a
  glibc-linked binary into Alpine and assume it runs.
- Python or Node.js: check native extensions against musl. When they need
  source builds or patches on Alpine, a slim glibc image (e.g. Debian slim) is
  the lighter overall choice.
- Several processes, an init system or service manager (OpenRC, systemd,
  s6, …) that the application actually depends on: choose a base that
  provides it, rather than reimplementing it.

Do not choose a base by habit in either direction: not Debian "to be safe",
and not Alpine when it forces rewriting the application. When the choice
changes an established, working build, or the trade-off is significant,
present the options and the reason to the owner before switching. Document the
chosen base and why.

The Containerfile is the source of truth for the base image, packages and
build steps; the manifest does not repeat them.

## 2. Build the production image locally

- Use the repository's production Containerfile, `linux/amd64`, the same build
  target and inputs as release CI, and the available build cache.
- Use a local builder (Podman/Buildah or Docker). Never install Docker inside
  an LXC or on a Proxmox node.
- Inspect the result: entrypoint/command, user, ports, unpacked size, linked
  libraries. The image must start without a compiler, package manager, SSH or
  interactive login.
- Test the built image, not a development process on the host. A successful
  compilation is not a successful container start, and the compressed image
  size is not the unpacked rootfs size.

Do not start a remote CI build just to find a problem the local image shows.

## 3. Run it as a container with the intended resources

Run the built image with exactly the cores, memory, swap, rootfs and home
volume size you intend to declare, unprivileged:

1. If Incus (or LXD) is available, run the OCI image as a system container
   with those limits and a separate volume at `/home/app` for stateful
   services. This is closest to the Proxmox LXC.
2. Otherwise use the local OCI runtime (`podman run` / `docker run`) with
   `--cpus`, `--memory`, `--memory-swap` and a volume for `/home/app`, and
   state that it was not an LXC test.

Ask the owner before installing a container runtime. If a tool or permission
is missing, report what was not tested; never report a test that did not run.
Never use production containers, data or credentials for these tests.

Check:

1. Cold start, `/health/live`, `/health/ready` and the telemetry endpoint on
   the declared port.
2. Representative work, not just an idle process: typical requests, a burst
   of concurrent ones and the expected error paths.
3. Stop and start again; for a stateful service, a fresh container with the
   same home volume keeps its data unchanged.
4. A stateful service refuses to start without a mounted `/home/app`.
5. A stateless service starts on a fresh root with only its deployment
   settings.

## 4. Measure and choose the values

Read the container's own cgroup while the workload runs:

| Resource | Evidence |
| :--- | :--- |
| CPU | `cpu.stat` usage and throttling under typical and burst work, request latency |
| Memory | `memory.peak` (or the highest `memory.current` seen), `memory.events` with no `oom_kill`, memory PSI |
| Swap | decide `0M` unless the application needs swap; do not hide too little RAM with swap |
| Rootfs | unpacked image size plus temporary files, caches and logs written to root |
| Home | expected durable data and its growth; free space on the mount |

Choose one value per resource with reasonable headroom above the observed
peak, rerun the workload at exactly those values, and repeat until it runs
without OOM, throttling that hurts latency, or full filesystems. Real clients
may need more later; the operator can raise the values per deployment.

Do not size from compiler memory, CI runner sizes or the examples in
[manifest-schema.md](manifest-schema.md).

## 5. Record the evidence

Document in the application repository, next to the runtime contract:

```text
Image / commit:
Base image and why:
Runtime used: Incus LXC / OCI runtime
Workload and duration:
CPU declared / observed:
Memory declared / observed peak / OOM events:
Swap declared:
Rootfs declared / used:
Home declared / used (or: no persistence):
Health, readiness and telemetry:
Restart / fresh-root result:
Not tested:
```

The manifest carries the chosen values; this record is the evidence for them.
