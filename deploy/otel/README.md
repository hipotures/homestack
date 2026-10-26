# HomeStack OpenTelemetry Collector image

`ghcr.io/hipotures/homestack-otel` is a minimal derivative of the official
`otel/opentelemetry-collector-contrib:0.163.2`, pinned by its OCI index digest
in the Containerfile. The actual upstream OCI user is `10001:10001` and its
ENTRYPOINT is `/otelcol-contrib`. The build copies an empty queue directory
owned by that user, mode 0700, into the scratch filesystem, and adds one
read-only configuration. It inherits the binary, CA certificates, libraries
and ENTRYPOINT; only CMD is changed to select that configuration. No shell,
package manager, root runtime, helper, hook or `/home/app` volume is added.

The distribution's builder manifest at tag `v0.163.2` pins `otlphttpexporter`,
`healthcheckextension` and `filestorage` to component version `v0.163.0`.
The runtime configuration uses their actual `otlp_http`, `health_check` and
`file_storage` names. Release CI validates it with the built binary before
publishing. Upstream sources:

- https://github.com/open-telemetry/opentelemetry-collector-releases/blob/v0.163.2/distributions/otelcol-contrib/manifest.yaml
- https://github.com/open-telemetry/opentelemetry-collector/blob/v0.163.0/exporter/otlphttpexporter/README.md
- https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/v0.163.0/extension/storage/filestorage

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

`python3 deploy/otel/verify-image.py IMAGE` inspects the built image's command,
user and exported directory ownership/mode, validates Collector configuration,
runs at 1 CPU/512 MiB/no swap, sends an OTLP metric to an unreachable export
destination, checks health and backing files, restarts the same container, then restores a
local receiver and verifies delivery of the queued metric.
It prints image size, runtime resource observations and results to the CI log.
This is a short Docker OCI validation, not Proxmox LXC or capacity evidence.
The 2 GiB rootfs and 100..10000 queue bounds require workload-specific sizing;
queue capacity counts requests, not a fixed number of bytes. Operators must
size rootfs for their payloads and outage window.

The implementation environment has no local Docker/Podman runtime. Local
verification uses the checksum-verified upstream release binary with a
throwaway queue path; actual built-image validation runs on the GitHub runner.
The owner performs deployment and integration verification, including the
isolated scenario in `docs/e2e-schema3.md`.
