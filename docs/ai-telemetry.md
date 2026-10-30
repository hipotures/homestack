# AI usage telemetry attribution

The dashboard filters usage by the `host_name` and `project` metric labels.

## Client configuration

Exactly three files configure telemetry. No shell exports, direnv files or
launch wrappers are involved, so interactive SSH sessions, systemd user
services, herdr-restored agents and Remote Control behave identically.

`/etc/environment` (part of the Gold image, no VM identity) carries the only
setting Codex cannot read from its configuration file. Codex exports delta
metrics unless the environment requests cumulative ones, and Prometheus
rejects delta samples:

```
OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative
```

`pam_env` applies it to SSH logins and the systemd user manager reads the same
file through `/usr/lib/environment.d/99-environment.conf`.

`~/.claude/settings.json` holds Claude Code's complete configuration. Claude Code
applies `env` inside each process at startup, independent of the parent environment:

```json
"env": {
  "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
  "OTEL_METRICS_EXPORTER": "otlp",
  "OTEL_LOGS_EXPORTER": "otlp",
  "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
  "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT": "http://otel.homestack.app:4318/v1/metrics",
  "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://otel.homestack.app:4318/v1/logs",
  "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE": "cumulative",
  "OTEL_METRICS_INCLUDE_REPOSITORY": "1",
  "OTEL_RESOURCE_ATTRIBUTES": "host.name=VM-HOSTNAME"
}
```

Claude Code does not detect `host.name` itself; each VM writes its own name
there. `OTEL_METRICS_INCLUDE_REPOSITORY` (Claude Code 2.1.314, undocumented,
off by default) stamps every metric and event with `vcs.repository.name` and
related attributes, derived from the `origin` remote of the session's
repository. Each session, including Remote Control workers, is a separate
process, so several repositories on one VM are attributed correctly.
Repositories without `origin` remain unattributed. The `session.id` attribute
keeps concurrent sessions in separate series.

`~/.codex/config.toml` holds the Codex exporters (see
[Response token export](#response-token-export)).

The Collector endpoint appears in both agent files because Codex enables
exporters only from its configuration and Claude Code only from its
environment. Move the Collector by changing the `otel.homestack.app` DNS
record, not the files.

Agents read these settings at process start. A running Codex app-server
daemon, herdr server or Claude daemon keeps its original environment until it restarts.

## Codex attribution limits

Codex 0.166.0 normally serves every terminal and Remote Control client from
one shared app-server daemon. Its OpenTelemetry resource is process-wide, and
it records no repository or working-directory attribute on metrics or events.
Codex usage therefore carries no `project`. Codex sets `host.name` only on its
log resource, so response tokens (derived from log events) carry `host_name`,
while native metrics such as turn counts and durations do not. Codex also sets
no `service.instance.id`, so simultaneous Codex processes on one VM can share
native counter series. Response tokens are unaffected because the Collector
exports them as per-batch increments.

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
by resource, model and reasoning effort (the event's `model_reasoning_effort`,
exported as `effort` like Claude Code's label), then emits
`codex.response.token_usage` gauges with a `token_type` label: `total`, `input`, `cached_input`, `cache_write_input`,
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

## Claude Code request timing

Claude Code reports request duration and time to first token only in its
`api_request` log event, so its settings must also export logs
(`OTEL_LOGS_EXPORTER` and `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` above).

The Collector sums `duration_ms` and `ttft_ms` of those events and counts the
requests, by model and reasoning effort, into delta gauges:
`claude_code.api_request.duration_ms`, `claude_code.api_request.count`,
`claude_code.api_request.ttft_ms` and `claude_code.api_request.ttft_count`.
Every other Claude Code event is discarded, including prompt and response
metadata. The dashboard shows the per-bucket average (sum / count) as *API
request duration* and *Request to first token* on the Claude Code panel. These
are single API calls, unlike Codex's per-turn histograms, and carry no p95.

## Prometheus resource labels

Prometheus must promote the resource attributes onto the usage metrics:

```yaml
otlp:
  promote_resource_attributes:
    - host.name
    - project
```

The Collector sets the `project` resource attribute from Claude Code's
`vcs.repository.name`, on native metrics and on `api_request` events, and
drops the other `vcs.*` datapoint attributes.

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
`prometheus_config_last_reload_successful` is 1. After a new Claude Code
session completes a model response, query:

```promql
group by (host_name, project, session_id) (
  claude_code_token_usage_tokens_total
)
```

The selected server and project must appear on the usage metric itself.
For Codex, `codex_response_token_usage` carries `host_name` but no project
(see [Codex attribution limits](#codex-attribution-limits)). `target_info` alone does not establish that the
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
A token dataset includes model/category series and shared category, model,
cache-hit and per-model cache-hit projections. Cost, counters and durations have shared timelines.
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

Two overview cards analyze individual models. *Model tokens over time* stacks
one model's mutually exclusive categories per bucket: uncached input, cached
input, cache write and output. Its selector lists every active model of the
selected agents as agent plus model name. The choice is remembered in the
browser only and never enters the server selection or adds a query. Codex
reasoning is shown in the tooltip as a subset of output and is never stacked.
Buckets without any reported sample are shaded and say "no data reported";
reported idle buckets show zero. *Cache hit by model* draws one line per model,
on a 0–100% scale, as `cached / (uncached + cached + cache write)`. Output and
reasoning are excluded. Rust emits only buckets with input, so buckets without
input are gaps, not 0%. Each model's range rate in the legend is the ratio of
summed tokens, never a mean of bucket rates. The tooltip lists the exact
cached, uncached and cache-write counts. Both cards follow the global agent,
host, project and range filters. They redraw only when a token dataset changes
and offer the same chart/table switch, copy and Copy all export, with the model
identity, counts and formula included.

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
