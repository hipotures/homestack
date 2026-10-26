# HomeStack OpenTelemetry Collector image

`ghcr.io/hipotures/homestack-otel` contains a minimal Collector distribution,
`/otelcol-homestack`, built with the OpenTelemetry Collector Builder (`ocb`)
from `builder-config.yaml`. It includes only the components used by
`collector.yaml`: the `otlp` receiver, `batch` processor, `otlp_http`
exporter, `health_check` and `file_storage` extensions, and the env/file
configuration providers. The static binary is about 28 MB instead of the
419 MB `otelcol-contrib` binary. The builder image and the Alpine runtime are
pinned by digest in the Containerfile.

The runtime is a digest-pinned minimal Alpine filesystem so Proxmox recognizes
the distribution and configures `/etc/hosts` and `/etc/resolv.conf` through
its existing Alpine setup plugin. The Collector is the ENTRYPOINT, remains
PID 1 and runs in the foreground as `10001:10001`; no init system,
daemonization, runtime helper or custom entrypoint script is needed. The
queue directory is owned by that user with mode 0700, and CMD selects the
read-only configuration `/etc/otelcol/homestack.yaml`. There is no
`/home/app` volume.

A scratch-only rootfs selects Proxmox's unmanaged setup plugin, which skips
hostname and DNS configuration. The published 0.1.1 image reproduces an immediate exit in local LXC
when Collector telemetry tries to resolve `localhost:8888`. Without resolver
configuration, deployment-specific export hostnames cannot resolve either. Docker supplies hosts and
resolver files itself, masking this incompatibility. Alpine supplies generic
localhost entries; Proxmox supplies the operator's resolver configuration.
No private DNS server, domain or destination is baked into the image.
See the upstream [unmanaged plugin](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/Unmanaged.pm)
and [Alpine plugin](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/Alpine.pm).

Component versions match the upstream `otelcol-contrib` 0.163.2 distribution
manifest. Release CI validates the configuration with the built binary before
publishing. Upstream sources:

- https://github.com/open-telemetry/opentelemetry-collector-releases/blob/v0.163.2/distributions/otelcol-contrib/manifest.yaml
- https://github.com/open-telemetry/opentelemetry-collector/tree/v0.163.0/cmd/builder

Proxmox VE can send node, VM, LXC and storage metrics to this Collector with
its built-in OpenTelemetry metric server (Datacenter → Metric Server) over
OTLP/HTTP on port 4318.

## Runtime contract

- OTLP gRPC: TCP 4317; OTLP HTTP: TCP 4318.
- Health: `GET /health/live`, TCP 13133, expected status 200. This is the
  Collector's health endpoint, not HomeStack telemetry schema 2. No extra
  `metrics.yaml` or HomeStack status endpoint is required.
- Metrics only: OTLP receivers, batch processor, OTLP/HTTP metrics exporter.
- `HOMESTACK_PROMETHEUS_METRICS_ENDPOINT`: required full destination URL,
  including the metrics path. No endpoint is baked into the image.
- `HOMESTACK_SENDING_QUEUE_SIZE`: integer export-request capacity, default
  1000; the deployment form permits 100..10000.
- `file_storage` backs the exporter queue under `/var/lib/otelcol/queue`.
  Directory creation at runtime is disabled. Retry elapsed time is uncapped.
  Queue files survive stop/start and reboot of the same CT. Replacing the
  disposable rootfs loses unsent data; review this explicitly before updating.
- No shared state, replication, automatic failover or HA is implied.

Manifest schema 3 is authored in three files at `.homestack/services/otel/`.
The inputs map directly to the two environment variables above. The operator
chooses their private destination and standard deployment settings; the
manifest and image contain no private hostname, domain or credential.

## Release and verification

After committing and pushing, use `python3 scripts/tag-image.py --service otel`
(`--dry-run` previews). The independent `otel-X.Y.Z` tag publishes OCI tag
`X.Y.Z`, starting at 0.1.0. The existing GitHub Actions checks must pass first.
Existing image tags are refused; validation precedes `docker push`. Manual
workflow runs publish `dev-<commit>` images. Ordinary branch pushes only check.

`python3 deploy/otel/verify-image.py IMAGE` inspects the built image's
user, Alpine distribution metadata and exported directory ownership/mode,
validates Collector configuration,
runs at 1 CPU/512 MiB/no swap, sends an OTLP metric to an unreachable export
destination, checks health and backing files, restarts the same container, then restores a
local receiver and verifies delivery of the queued metric.
It prints image size, runtime resource observations and results to the CI log.
This is a short Docker OCI validation, not Proxmox LXC or capacity evidence.
The 512 MiB rootfs and 100..10000 queue bounds require workload-specific sizing;
queue capacity counts requests, not a fixed number of bytes. Operators must
size rootfs for their payloads and outage window.

Local diagnosis uses unprivileged LXC 6.0.6 with a separate UID/GID mapping,
OCI images pulled by digest with Skopeo and unpacked with Umoci. The original
scratch image reproduces the startup failure; overlaying the pinned Alpine
filesystem makes Collector start and answer its health endpoint. The local
lab has only isolated loopback networking and does not contact production.
Docker/Podman is unavailable locally; GitHub Actions builds the Containerfile
and validates the exact resulting OCI image. Local LXC verification of the
published result is recorded separately. The owner performs Proxmox deployment
and integration verification, including `docs/e2e-schema3.md`.


### Local LXC diagnosis

The candidate rootfs combines release 0.1.1 with the exact pinned Alpine
filesystem used by the updated Containerfile (8,417,144 additional file
bytes before overlapping files). It runs with UID/GID 10001, one CPU,
512 MiB memory and no swap. Health and OTLP/HTTP ingestion return 200.
One metric is ingested with the export destination unavailable, then the
same LXC is stopped and started. An isolated DNS server and HTTP receiver
are started afterward; the queued metric reaches the receiver through the
configured DNS hostname. Health remains 200 after restart. Observed memory
peak on that restart is 40,652,800 bytes, with zero OOM events. This is a
short diagnostic workload, not a throughput or queue-capacity measurement.
Resolver settings in this standalone lab are a fixture representing the
configuration Proxmox's Alpine plugin supplies; no Proxmox deployment was
performed by the agent.
