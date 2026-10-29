# Metrics

Metrics is a standalone, stateless, read-only Prometheus MCP service. Any MCP
client can discover metrics, labels and series, then construct arbitrary PromQL.
There is no UI, persistent storage, response cache or application authentication.
It is independent of AI Usage and has no agent-specific metric interpretation.

The official Rust MCP SDK (`rmcp` 3.8) serves **stateless Streamable HTTP** at
`/mcp`, with JSON responses for ordinary request/response calls. MCP clients
initialize the connection and discover descriptions and JSON input schemas using
`tools/list`. Legacy HTTP+SSE transport is not used. `GET /` returns 404.

| Tool | Fixed upstream endpoint | Inputs |
| --- | --- | --- |
| `prometheus_query` | `/api/v1/query` | arbitrary `query`, optional `time`, `limit` |
| `prometheus_query_range` | `/api/v1/query_range` | arbitrary `query`, `start`, `end`, `step`, optional `limit` |
| `prometheus_series` | `/api/v1/series` | nonempty `matchers`, optional `start`, `end`, `limit` |
| `prometheus_label_names` | `/api/v1/labels` | optional `matchers`, `start`, `end`, `limit` |
| `prometheus_label_values` | `/api/v1/label/{label}/values` | `label`, optional `matchers`, `start`, `end`, `limit` |
| `prometheus_metadata` | `/api/v1/metadata` | optional `metric`, `limit`, `limit_per_metric` |

Use `prometheus_label_values` with `label="__name__"` to discover metric names.
Matchers become repeated `match[]` parameters. Timestamps accept finite Unix
seconds or RFC3339 strings. Steps accept positive finite seconds or Prometheus
durations such as `30s`, `5m` and `1h30m`.

Results preserve the Prometheus JSON envelope in MCP `structuredContent` and
JSON text content, including `status`, `data`, `resultType`, `result`, `warnings`,
`infos`, labels, sample strings and metadata fields. Upstream API errors retain
the original error envelope and set MCP `isError`; local validation/transport
failures return a structured error. Nothing rounds, normalizes or interprets data.

The security boundary is explicit: **arbitrary PromQL: YES; arbitrary Prometheus
HTTP access: NO; Prometheus writes/admin operations: NO**. Each tool uses a fixed
GET endpoint. Callers cannot supply methods, paths, upstream URLs or extra request
parameters. Label path segments are validated before constructing the URL.

## Deployment contract

The schema-3 service lives in `.homestack/services/metrics/`. HomeStack supplies
required `HOMESTACK_PROMETHEUS_URL`, an HTTP URL with a host and no credentials.
Its path, query and fragment are ignored. Redirects and environment proxies are
disabled. The service listens on `0.0.0.0:80` by default; `HOMESTACK_LISTEN` can
override this for local diagnostics. It logs to stderr and writes no state.

There is no TLS inside the container. Pangolin is expected to terminate external
TLS and provide external authentication separately, forwarding `/mcp` from the
`metrics` subdomain to port 80. Do not route `/.well-known/homestack/status`
publicly. No credentials or private domains belong in these manifests. The server
accepts operator-selected Host names and rejects browser Origin headers; native
MCP clients and server-side MCP Inspector connections need no browser session.

`/health/live` and `/health/ready` return `application/health+json`. Both check a
self-HTTP listener heartbeat, independent of Prometheus availability, so an
upstream outage cannot trigger a restart loop. `HOMESTACK_HEARTBEAT_SECONDS`
defaults to 60 (allowed 1..3600). Above three missed intervals health fails.
`/.well-known/homestack/status` uses the shared schema-3 service-status contract
and reads current process RSS. The heartbeat has no persistent state.

| Operational bound | Value |
| --- | --- |
| Upstream timeout | 20 seconds, including body reads |
| Concurrent upstream requests | 16; excess calls fail promptly and may retry |
| Query/selector text | 32 KiB; at most 64 selectors with 32 KiB combined text |
| MCP HTTP request body | 128 KiB |
| Upstream response body | 8 MiB, checked while streaming |
| Range evaluation points per series | 11,000, including the initial point |
| Result limit | default 100, caller range 1..1000; zero/unbounded rejected |
| Metadata entries per metric | default 100, caller range 1..1000 |

There is no maximum history duration: increasing `step` permits hundreds of days
of history. Discovery and query result limits are sent upstream; responses that
ignore them are rejected rather than silently truncated.

## Image and release

The image is `ghcr.io/hipotures/homestack-metrics:X.Y.Z`. A static amd64/musl
`homestack-metrics` binary runs directly on pinned Alpine 3.24.2. Alpine supplies
Proxmox-compatible hosts/resolver layout; no frontend, extra daemon, runtime
package installation, certificates or persistent volume is needed.

Build through the repository wrapper:

```sh
RUSTFLAGS='-C target-feature=+crt-static' python3 scripts/cargo.py build --release --locked --target x86_64-unknown-linux-musl -p hs-metrics --bin homestack-metrics
```

Copy the binary into a local image context and build using
`deploy/metrics/Containerfile`. For focused image admission, use the official MCP
Python SDK rather than a handwritten protocol client:

```sh
uv run --no-project --with mcp==2.3.0 python deploy/metrics/verify-image.py --runtime podman IMAGE
```

The fixture is disposable and local. Admission prints initialization and complete
`tools/list` schemas, exercises all fixed endpoints, verifies original query JSON,
checks invalid ranges and injection attempts before upstream access, tests
concurrent calls, and records container memory/OOM evidence. CI uses Docker and
runs this admission before publication. This is an image smoke check, not live
Prometheus or Proxmox E2E.

After committing and pushing an image-affecting change, run:

```sh
python3 scripts/tag-image.py --service metrics
```

The script selects the next independent `metrics-X.Y.Z` tag (initially
`metrics-0.1.0`). Tags publish immutable numeric image versions after checks;
ordinary branch pushes only check. Manual workflow runs publish `dev-<commit>`.
Manifest-only or documentation-only changes need no image release.

## Initial resource evidence

Local verification used rootless Podman, not Proxmox LXC. The declared profile is
1 core, 256 MiB memory, zero swap, 64 MiB rootfs, one `metrics` instance, single
placement and stateless operation, with no home volume. The memory budget leaves
headroom for simultaneous raw Prometheus responses (up to 16 × 8 MiB before JSON
processing), unlike a small precomputed dashboard response.

The initial static image occupied 17,481,144 bytes unpacked. A fresh container
completed the local fixture workload in a few seconds: initialization, discovery,
instant/range queries, metadata, errors and 24 concurrent calls. Upstream
concurrency reached 16, with excess calls rejected. Observed cgroup memory peak
was 3,170,304 bytes; process RSS reported 4,796,416 bytes; OOM events were zero.
The sample consumed about 19 ms CPU. The workload uses small synthetic result
sets, not 16 simultaneous maximum-size matrices; operators can raise memory for
large result workloads. Rootfs capacity is measured against the unpacked image;
OCI smoke checks do not enforce a Proxmox root-volume quota. No live Prometheus,
Pangolin or LXC deployment was exercised.
