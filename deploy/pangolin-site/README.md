# Pangolin Site runtime

This image runs the pinned Pangolin CLI in Site mode (`pangolin-cli up site`)
as the fixed `site` user under runit. Alpine provides the existing supervision,
CA certificates and musl runtime without adding an interpreter to the final
image. The Go build stage compiles a static `linux/amd64` executable from a
checksum-verified upstream archive. `prepare-source.py` adds the management
endpoints to that same process and a callback to Newt's successful tunnel
probes; it refuses unexpected upstream source. There is no telemetry daemon.

The upstream CLI and Newt source remain available at
<https://github.com/fosrl/cli/tree/0.18.1> and
<https://github.com/fosrl/newt/tree/v1.18.1>. The exact HomeStack additions and
build recipe are in this directory. The compiled modifications use AGPL-3.0;
the upstream license is included in the image at
`/usr/share/licenses/pangolin-cli/LICENSE`.

## State and deployment settings

`/home/app` must be an actual mount and must not be a symlink. The image leaves
it empty. Startup creates `/home/app/pangolin-site` with mode 0700, owned by
`site`. The credential receiver accepts the Site configuration on stdin,
installs `site.json` atomically with mode 0600 and restarts the connector.
It never generates or replaces a credential on its own. A symlink at the
state directory is refused. Durable state is never written to disposable root.
Logs go to stdout/stderr. The image contains no Site credentials.

HomeStack supplies `HOMESTACK_MANAGED_HOME=1`, the mount, the Site configuration
through its authenticated credential receiver and
`HOMESTACK_HEARTBEAT_SECONDS` (10–3600, default 60). Self-update and SSH stay
disabled. Each instance owns an independent Site and home; the manifest
spreads three instances across available nodes. The management port is private
LAN HTTP, with no Pangolin exposure or bundled TLS proxy.

## HTTP health and telemetry

Port 8080 serves only these management routes:

- `GET /health/live`: `application/health+json`; process liveness and heartbeat
  age/count. Startup has a grace period of three heartbeat intervals.
- `GET /health/ready`: the same format, requiring at least one successful
  tunnel probe and a fresh heartbeat. Both health routes return 503 after
  three missed intervals, otherwise 200. Readiness does not claim that every
  downstream resource is available.
- `GET /.well-known/homestack/status`: schema 2 JSON, `Cache-Control: no-store`;
  process uptime, mount-aware filesystem space, cgroup v2 CPU, memory, PSI and
  cumulative I/O. Telemetry `status` matches liveness, independent of errors
  in individual measurements.

The heartbeat counts actual successful ICMP probes through Newt's userspace
WireGuard tunnel, at most once per configured interval. It adds no periodic
timer, disk writes or synthetic progress. A stuck or disconnected tunnel
cannot advance it. HomeStack also verifies the Site is online through the
Pangolin API during initial deployment. Image replacement preserves the existing
home and credential and verifies HTTP readiness directly, without connecting
to the Pangolin API or opening an SSH tunnel.

Telemetry wakes a single idle measurement worker only when requested and
reuses completed samples for two seconds. HTTP waits at most 700 ms, within
HomeStack's 800 ms request timeout. A stalled filesystem retains the previous
sample with its true age, or reports `pending`; it cannot accumulate workers.
Missing mounts, missing controllers/PSI and measurement failures stay distinct
from zero. No shell commands or subprocesses collect telemetry, and host load
averages are never read.

New deployments and reviewed updates record `status-port 8080` through the
HTTP healthcheck in `.homestack/pangolin-site.yaml`. Existing containers need
an explicit image update to obtain the new executable and metadata; status
discovery does not change their configurations. Use Site image 0.18.2 or later.

## Local verification, 2026-10-24

Production Containerfile, image
`89aa533fa0dd55a4a866a8698d067cdf38e5df3f47427c55d0fc32aeb9a9f18f`:

| Item | Evidence |
| --- | --- |
| Runtime | Rootless Podman OCI container; not an LXC test |
| Workload | Cold start with disposable local credentials, unreachable loopback server, 120 management requests across 12 concurrent clients, 31-second heartbeat expiry |
| CPU | 1 CPU enforced (`cpu.max=100000 100000`); first sample 76,715 µs cumulative CPU, no throttled periods |
| Memory | 128 MiB enforced (`memory.max=134217728`); peak 21,082,112 bytes during management workload; zero OOM/max events |
| Swap | Disabled (`--memory-swap` equals the memory limit) |
| Root | Image size 40,344,098 bytes, unpacked root 40,064 KiB (`du -skx /`); default 128 MiB retained; local overlay root quota was not enforced |
| Home | Exact 10 MiB separate tmpfs mount for resource verification; 4,096 bytes used, values compared with `df -B1` |
| Cgroup | CPU/memory/PSI/I/O present; counters compared with the container's cgroup files |
| Health | Ready 503 without a tunnel; live initially 200, then both 503 after three missed 10-second intervals; telemetry status then `fail` |
| Fresh root | Two containers using the same separate durable volume preserved the exact credential digest; home quota was not enforced in this separate persistence check |
| Missing home | Startup and credential receiver refuse an unmounted home |

Temporary Go diagnostics also verified successful-probe counting and expiry,
no collection while idle, bounded stale responses, unavailable PSI, failed
filesystem measurements, empty I/O counters and visible CPU/memory limits.
They are not permanent repository tests.

Not tested: a live Pangolin server, real resource forwarding, full tunnel
throughput or Proxmox LXC. The resource defaults retain the existing connector
allocation; this run verifies the added management workload, not a new tunnel
capacity claim. No production container, data or credential was used.

## Releases

Run `python3 scripts/tag-image.py` after committing and pushing image changes.
It fetches tags, automatically selects the next patch in the independent
`pangolin-X.Y.Z` line, refuses dirty/unpushed/already-released HEADs and pushes
only its annotated tag. `--dry-run` previews the result. CI checks first, then
publishes `ghcr.io/hipotures/homestack-pangolin-site:X.Y.Z` as a single
`linux/amd64` image. Existing image versions cannot be overwritten. A manual
workflow run publishes `dev-<commit>`; ordinary branch pushes only check.
Manifest-only changes, documentation and tooling need no image release.
