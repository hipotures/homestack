# AI usage telemetry attribution

The dashboard filters usage by the `host_name` and `project` metric labels.
Keep the global OTEL settings, including `host.name`, in the shell configuration
and the project-specific attributes in the repository's trusted `.envrc`.

## Terminal Codex sessions

Codex 0.165.0 normally connects terminal clients to a shared background daemon.
That daemon retains its startup environment. Loading `.envrc` in another
terminal does not change the daemon's resource attributes, even when the new
terminal client has the correct environment.

Source `scripts/codex-telemetry.bash` from interactive Bash after the global
OTEL exports. It runs terminal Codex with `--no-daemon`, preserves the resource
attributes supplied by Bash and direnv, and gives each invocation a fresh
`service.instance.id`. The separate instance prevents simultaneous processes
and restarts from overwriting each other's cumulative counters.

For a stable local installation, copy the script to
`~/.local/share/homestack/codex-telemetry.bash` and source that path from
`~/.bashrc`. Open a new terminal in the project, let direnv load its trusted
configuration, and run `codex`. Existing running conversations keep their
original backend and environment; finish the active turn before reopening or
resuming them. This launcher applies to terminal Codex, not to independently
started IDE, desktop, remote, or managed app servers.

## Response token export

Codex's native `codex.turn.token_usage` histogram is emitted when a turn
finishes. It does not provide live token accounting for active turns, and
unfinished turns can be absent. The dashboard uses completed-response
usage events for live tokens and estimated cost. Native metrics remain enabled for
turn duration, tool calls and other measurements.

Keep both exporters in the user-level `~/.codex/config.toml`, using the same
Collector host as the existing metrics endpoint:

```toml
[otel]
log_user_prompt = false
exporter = { otlp-http = { endpoint = "http://COLLECTOR:4318/v1/logs", protocol = "binary" } }
metrics_exporter = { otlp-http = { endpoint = "http://COLLECTOR:4318/v1/metrics", protocol = "binary" } }
```

Deploy the updated Collector before enabling log export, then restart the
Codex backend after finishing its active work. A shared daemon or independently
started IDE/app backend must also restart to load the configuration. Deploy
the updated AI Usage image after the new metric appears in Prometheus.

The Collector sums only `codex.sse_event` records with
`event.kind=response.completed` and token attributes. All other log records
are discarded rather than forwarded or stored as logs. It groups each batch
by resource and model, then emits `codex.response.token_usage` gauges with a
`token_type` label: `total`, `input`, `cached_input`, `cache_write_input`,
`output` and `reasoning_output`. Input includes cached and cache-write tokens;
reasoning is already included in output. The event's `tool_token_count` field
contains total response tokens, despite its name.

Each gauge sample is a batch's token increment, not a cumulative counter.
Prometheus receives these samples directly through OTLP, not through a scrape.
The dashboard uses `sum_over_time` for these gauges, so Collector or client
restarts do not require retaining an in-memory cumulative baseline. Never use
`rate` or `increase` on these gauges, scrape them as if they were counters, or
add the old turn histogram to their totals. Other dashboard instruments still
use cumulative counters. Resource compaction prevents duplicate resource groups
within a batch from overwriting samples at the same millisecond. Each Collector
also adds its container hostname as the `collector_instance` datapoint label,
read through the file provider from `/etc/hostname`. The same identity is
stored in `homestack.collector.instance` on the resource to isolate generated
Prometheus `target_info` series as well. All three deployment
replicas must use the updated image and have distinct hostnames. Separate
series prevent a delayed queue on one replica from delivering samples older
than another replica's latest sample in the same series. The dashboard sums
across Collector labels without counting a response more than once.

Usage becomes available after each model response completes and both the
Codex exporter and Collector flush their batches (the Collector timeout is
five seconds). This is not a token-by-token stream. Later interruption of a
turn does not remove already delivered response increments. The persistent
exporter queue protects converted metrics during downstream outages and ordinary
Collector restarts; log batches and in-flight source events are not a durable
journal, so abrupt process loss before enqueueing can still lose data.

The new series starts when response export is enabled. Historical turn metrics
are read strictly before `HOMESTACK_CODEX_HISTORY_END`, the first response-token
sample's Unix timestamp. The AI Usage image defaults to `1793191130.125`
(2026-10-28 12:38:50.125 UTC), the original deployment's first response sample.
Other installations must set this environment variable to their own first
response sample, or set it to `0` to disable historical reads. Bare binaries
without the variable default to `0`.

This fixed boundary is independent of selected dates, servers and projects and
does not move as Prometheus retention expires. For a window spanning the
boundary, the dashboard freezes each historical counter immediately before the
first response sample and adds live response increments. Turn summaries at or
after the boundary are never counted, even in quiet response intervals.
Historical models, token categories, scopes and estimated costs use the same
queries as the charts, comparison, summaries and JSON exports. No Prometheus
data is rewritten or backfilled. Missing response metrics do not suppress
available historical costs; the page reports that only historical data is
available. Historical measurements retain the original turn accounting's gaps
and attribution limits.

## Prometheus resource labels

Prometheus must promote the resource attributes onto the usage metrics:

```yaml
otlp:
  promote_resource_attributes:
    - host.name
    - project
```

Prometheus already maps `service.instance.id` to its `instance` label. Without
a unique instance, several writers can share one series, causing apparent
counter decreases and unreliable totals. Promoting attributes only affects
newly ingested samples; it cannot repair or attribute old samples.

For a manually installed container with configuration at
`/etc/prometheus/prometheus.yml`, run `scripts/prometheus-ai-telemetry.sh`
inside the container. The script preserves existing settings, creates a
backup, validates the candidate with `/bin/promtool`, writes the configuration,
and sends SIGHUP to the Prometheus process at PID 1. It refuses an existing
promotion list or backup so it cannot silently replace an earlier change.
For an existing promotion list, retain its entries and add `host.name` and
`project` manually instead.

Verify `/api/v1/status/config` contains the promotion list and
`prometheus_config_last_reload_successful` is 1. After a new Codex session
completes a model response and exports events, query:

```promql
group by (host_name, project, instance) (
  codex_response_token_usage{token_type="total"}
)
```

The selected server, project and a distinct instance must appear on the
usage metric itself. `target_info` alone does not establish that the
dashboard can filter the counters.

References: [Codex CLI](https://learn.chatgpt.com/docs/developer-commands),
[Prometheus OTLP resource attributes](https://prometheus.io/docs/guides/opentelemetry/#promoting-resource-attributes).

## Dashboard architecture

The Alpine `homestack-usage` binary embeds its HTML, vanilla JavaScript,
Datastar 1.0.4 and Apache ECharts 6.1.0. Vendor releases, licenses, download
provenance and SHA-256 checksums are in `crates/hs-usage/assets/`. There are no
runtime CDN requests. The startup-only LiteLLM download supplies OpenAI token
prices; a failed download leaves estimated costs explicitly unavailable until
restart. Claude Code cost comes from its reported USD counter. Pangolin remains
the HTTPS and authentication boundary. Health, heartbeat, service status and
stateless deployment contracts are unchanged.

The browser no longer has a Prometheus proxy. Only registered dashboard ranges,
agents and bounded host/project labels are accepted. Arbitrary PromQL and metric
selectors are not API inputs. The serving pipeline is:

1. `registry.rs`: agent adapters and instrument discovery. Claude counters and
   Codex response increments, frozen turn history, counters, duration
   sum/count pairs and duration `_bucket` histograms declare their measurement
   and unit here.
2. `planner.rs`: stable UTC bucket plans, grouped queries, process-preserving
   counter subtraction and exact source provenance. Closed history and current
   partial buckets are independent operations. Duration p50/p95 are the one
   per-selection read: quantiles cannot be combined after host/project
   filtering, so each Codex duration histogram gets one instant
   `histogram_quantile` query for the range (p50, p95 and the bucket bounds,
   to mark values in the open `+Inf` bucket) and one closed-bucket p50
   `query_range`, both with the host/project matchers. They are
   bucket-approximated; means (Σ sum / Σ count) remain alongside.
3. `prometheus.rs`: one reused HTTP client, global concurrency **four**, query
   deduplication, per-key singleflight and bounded response/cache sizes.
4. `dashboard.rs`: host/project filtering, token normalization, model pricing,
   comparison totals, weighted duration means, heatmap inputs and spike detection.
   Calculations are shared across widgets and viewers.
5. `datasets.rs` and `live.rs`: content versions, coherent snapshots, point
   changes or replacements, Datastar SSE and exact-snapshot exports.

### Datasets and time

A dataset has a stable logical ID such as `codex.tokens`, `claude.cost`,
`all.summary` or `all.activity`; agent, measurement and unit; validated scope;
UTC start/end/step and current partial-bucket start; typed dimensions and exact
numeric points; availability (`available`, `missing`, `unavailable`, `partial`);
notes; a source-freshness block reference; and a content version. Source query values remain original Prometheus
strings in export provenance. Presentation rounding never enters accounting.
A token dataset includes model/category series and shared category, model and
cache-hit projections. Cost, counters and durations have shared timelines.
Tables, tiles and charts consume these projections rather than issuing queries.

Range steps give 30-90 points per chart and match `AIUsage.RANGES` in the
dashboard theme: 1h/2m, 24h/30m, 7d/3h, 30d/12h, 90d/1d, 180d/3d and 1y/1w.
`all` starts on the day of the first retained sample (one daily count query over
the 600-day retention, cached for six hours) and uses the smallest of 1d, 1w,
2w, 30d and 91d that yields at most 80 points. Multi-day steps are aligned to
the Unix epoch, so weekly buckets start on Thursdays (UTC).
The start is the current UTC boundary minus the requested duration. The result
includes that duration of closed buckets plus the partial current bucket.
`end` is the next boundary, not a claim that future samples exist. Points are
identified by bucket start; underlying Prometheus windows are open on the left
and closed on the right. Axes remain fixed inside a bucket. Display timestamps
and the weekday/hour heatmap use browser-local time, including local DST rules.
Durations and accounting windows remain UTC. Heatmap inputs cover at least seven
and at most thirty days, and the heatmap states its window ("last 30 days") for longer ranges; spike inputs cover the selected days plus seven prior
days. Reasoning stays a subset of output. Codex inclusive input is split into
uncached input, cached input and cache writes before pricing.

### Retention, cache and scheduling

The old `LOOKBACK = "100d"` is removed. Counter baselines use the existing
**600-day** retention horizon, including inactive processes. A process with an
old baseline that resumes in the selected period is subtracted against that
baseline, rather than having its whole retained counter counted as new usage.
Codex response gauges always use `sum_over_time`; `rate` and `increase` are not
used. Historical turn usage is frozen strictly before the fixed rollout
boundary. Later turn summaries cannot fill quiet response intervals or overlap
live response usage.

Closed range matrices and per-process boundary baselines have six-hour TTLs;
current partial buckets and normalized global views have thirty-second TTLs.
The frozen Codex historical endpoint keeps its six-hour TTL.
Discovery refreshes hourly. Server/project options also include labels found
only in retained process baselines, so inactive scopes remain selectable without
an additional Prometheus query. Ordinary refreshes within the same UTC bucket reuse
history/baselines and execute only recent-window queries. Cold views and bucket
rollovers can require more expensive retention-baseline calculations. This is
not evidence of full-year performance. Late data in closed buckets becomes
visible when those cached reads refresh. No recording rules or Prometheus
configuration changes are required.

Query caches allow 96 entries and 12 MiB of conservatively estimated parsed
storage; an individual response is limited to 1 MiB. Global calculations retain
at most two views within an 8 MiB estimate; exact exports retain at most twelve
snapshots within another 8 MiB estimate. Shared `Arc` data avoids duplicating
provenance across viewers. Normalized wire snapshots are limited to 512 KiB.
Oversized results fail explicitly, preserving the browser's last successful
state instead of silently truncating totals. These are internal safety budgets,
not public configuration. A global build lock coalesces snapshot misses and
prevents overlapping expensive refreshes. Queries within a build share the
four-slot execution semaphore. Cache misses for identical query/time parameters
coalesce; errors are cached briefly to avoid retry storms. Arbitrary scope
combinations cannot create unbounded caches or distinct PromQL plans.

### Updates and resynchronization

A page makes five initial requests: HTML, three local scripts and one Datastar
SSE stream. Vendor scripts have immutable caching and precompressed gzip
representations. Eligible ordinary responses use negotiated gzip. SSE remains
unbuffered, uncompressed and sends ten-second keepalive comments; the server
refreshes selected snapshots every thirty seconds only while subscribed. There
are at most 32 concurrent streams; excess streams receive HTTP 429 and retry
with Datastar's bounded backoff. Hidden pages close their stream and reconnect
when visible. Query work is shared across viewers, independently of rendering.

`/api/dashboard/events` accepts a Datastar selection payload and returns an
atomic packet signal. Every connection begins with a full snapshot. Later
packets contain only changed datasets: tiny or structurally changed datasets
are replaced; compatible larger series use append/update/remove points when
that encoding is smaller. Unchanged datasets produce no dataset payload and
no `setOption` calls. Small exact changes are compared with published versions,
so presentation rounding cannot hide accumulated usage. Metadata heartbeats
still report checks and source freshness. `source_at`, `history_checked_at`
and `checked_at` distinguish successful source reads from publication/version
changes; a version changes only when its dataset representation changes.

The client validates batch base snapshot and dataset versions before applying
changes. Each selection gets a generation number; responses from old selections
are ignored. Any mismatch restarts the stream with a full snapshot. Every
reconnection also starts full; no per-browser replay history is retained.
Connection state holds only the last snapshot ID. Deltas resolve against the bounded
shared snapshot cache; eviction automatically sends a full snapshot. A browser
cannot pin dataset or provenance histories in server memory.
Prometheus failures keep useful charts visible with an explicit error/freshness
indicator. The page detects a lost stream even before a source response arrives.

ECharts owns axes, scales, legends, tooltips, zooming and heatmaps. Stable chart
hosts are excluded from Datastar morphing. Instances are created once and
updated via `setOption`; table switching hides the instance, resizing uses
`ResizeObserver`, and removed widgets dispose their instances. Background
refresh never applies a grey loading overlay or rebuilds the chart grid.

### Comparison, exports and extension

All-agent and single-agent views retain cost/tokens, category breakdowns, model
costs and tokens, sessions, active time, commits, lines, Codex turn and tool
metrics, means, heatmap, spikes, themes and responsive layouts. Comparison uses
shared capabilities, with the union of token categories. Missing and unsupported
measurements remain explicit rather than zero. Summary totals and comparison
values come from the same selected datasets as the charts. Cost estimates use
model names without guessing and report missing rates; partial model costs are
never presented as complete agent cost. The footer reports the image version.

Chart/table copy and Copy all retain unrounded displayed series, filters, time
boundaries, original PromQL and source matrices/labels. Copy all emits JSON
Lines, one chart per line. An explicit `/api/dashboard/export?snapshot=...`
request obtains provenance for the exact intended displayed snapshot, rather
than attaching raw matrices to routine updates. Shared query provenance includes
the global source rows used before server-side scope filtering. Exports carry
the selected scope and normalized scoped datasets as well. Retention eviction
returns HTTP 409 instead of quietly exporting newer values; reconnect to obtain
a current exportable snapshot. Clipboard uses the browser Clipboard API over
Pangolin HTTPS (or localhost). `/api/dashboard/snapshot` is the same aggregated
JSON snapshot for diagnostics, with no arbitrary query access.

To add a telemetry source, define its instrument adapter and dimensions in
`registry.rs`, declare counter versus increment semantics, then normalize into
existing measurements or a new dataset in `dashboard.rs`. Extend planner kinds
only when the source accounting actually differs. To add a chart/table, consume
a dataset projection in `assets/dashboard.js`; do not add PromQL, pricing,
network polling or custom axes there. New derived calculations belong in Rust.
The retained raw sources automatically become available to snapshot exports.

### Known limits

Prometheus samples cannot establish lifetime completeness. Missing exports,
process resets between samples, baselines older than retention, and historical
host/project labels cannot be reconstructed. Totals explicitly describe retained
selected UTC buckets, not an audited lifetime ledger. Bucket totals are shared
with summaries; a counter reset is clamped per bucket, and a reset within a
bucket can still lose usage. Sources whose responses exceed the safety budgets
need a separately reviewed cardinality strategy; they fail visibly today.

Cold 90-day plans can scan retained baselines and have a twenty-second
Prometheus timeout. A full year of dense source data has not been benchmarked.
Closed-bucket late ingestion may take six hours to appear. All cache/export
state is memory-only and disappears on restart. Startup pricing is not a
historical rate ledger, and estimated API charges are not subscription costs.
The deployment remains at one core and 64 MiB unless measured workloads justify
a specific increase. Production deployment remains a separate owner action.

### Focused modernization verification (2026-10-29)

Measurements used the native debug binary and headless Chromium against a
**disposable local Prometheus 3.15.0**, with deterministic Claude/Codex counters,
response increments, the fixed transition boundary, and a process baseline
150 days old. They are not a production or full-year capacity benchmark.

| Measurement | Observed |
| --- | --- |
| Original browser initial load, same fixture | 23 requests, including 22 Prometheus requests |
| New browser initial load | 5 requests: page, 3 scripts, 1 SSE stream |
| New cold all-agent 24h calculation | 18 Prometheus requests, maximum concurrency 4 |
| Normal unchanged refresh | 0 new browser requests, 4 recent Prometheus queries |
| Full SSE snapshot | 30,135 bytes, including Datastar envelope |
| Unchanged SSE update | 702 bytes of metadata, zero dataset changes and zero chart render calls |
| Changed / removed-point SSE batches | 8,676 / 8,697 bytes; replacements plus point patches |
| Snapshot JSON / negotiated gzip | 27,025 / 3,454 bytes |
| Warm refresh HTTP response | 23 ms |
| Snapshot cache-hit HTTP responses | 1.5–1.6 ms |
| Cold initial generation | About 35 ms in this small fixture |
| Twelve simultaneous requests for an uncached 90d view | 9 shared Prometheus requests, one shared snapshot |
| Native process RSS / high-water after cache/scope/stream pressure | About 33 / 38.2 MiB |
| Stream bound | 32 accepted, 33rd request returned 429 |

The original and new dashboards agreed on 640 Claude tokens, 1,600 Codex tokens,
Codex categories (960 uncached input, 320 cached input, 320 output, 80 reasoning),
and USD 0.00444 Codex cost. The old lookback counted 1,500 sessions for the sparse
process; the retained 1,000-session baseline makes the new increment correctly
500. A separate ordinary process contributed four sessions in both dashboards.

Observable checks covered stable chart instance identities, unchanged-render
suppression, changed values, eight removed points, source outage retention,
reconnect/full resync, deliberately wrong versions, stale filter generations,
comparison, JSON/JSON Lines clipboard exports with original source strings,
Codex-specific views, dark/light themes and a 390px mobile layout without page
overflow. Vendor hashes/gzip, ETag 304, local licenses, all health/status paths,
invalid filters and removed proxy routes were checked. Forty arbitrary scope
requests stayed bounded. The checks were temporary diagnostics, not a new
permanent test suite. The existing image-validation probe now uses the dataset
endpoint for its expected upstream-unavailable response.
