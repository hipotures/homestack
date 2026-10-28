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

## Comparing agents

The dashboard's `Compare agents` switch replaces the charts with a table for
all registered agents, using the current server, project and time filters.
Cost and total tokens come first. Output includes reasoning; its separate
reasoning row is a subset, not additional usage. The comparison table and
`All agents` charts show only capabilities shared by every registered agent,
except token categories: those use the union of the agents' categories, so
reasoning or cache writes stay visible even when only one agent reports them.
Missing measurements are marked explicitly instead of being displayed as zero.
Selecting a single agent shows all its available charts, including metrics
that cannot be compared with another agent.

The `AGENTS` registry in `crates/hs-usage/src/index.html` defines each agent's
metric selectors, token label names, token categories and optional capabilities.
Add an adapter there to include another agent in the comparison columns,
agent selector, scope discovery and shared charts. Declare whether input
already includes cached and cache-write tokens. Unreported capabilities remain
absent. Shared charts keep agent colors consistent and show adjacent bars;
model costs and agent-specific measurements retain their own breakdowns.
Optional `extras` describe agent-specific counters and duration histograms.
Codex discovers turn counts and durations, first-token and first-output
latency, tool calls, failures and tool durations from the available metric
names. Histogram means divide the duration sum by the observation count and
convert milliseconds to seconds. Full-turn duration includes tools and waits;
it is not mapped to Claude Code's active time.

In the chart view, `All agents` also shows total cost and total tokens in a
separate column to the left of the per-agent tiles. These totals use the same
scoped snapshot, include estimated costs where applicable, and explicitly name
unavailable contributors when a sum is partial. The summary moves above the
agent tiles on narrow screens and is hidden in comparison and single-agent views.

The container downloads prices once at startup and embeds only the four token
rates needed by the dashboard in the HTML document. There is no separate
pricing request on page load, refresh or filter changes. Agent-specific
snapshot queries are skipped when their capabilities are hidden in the current
view. Metric-name discovery requests only the relevant Codex instruments;
server and project filters share a single query over usage metrics, without
scanning every tool metric and histogram bucket.

## Dashboard version and table export

The footer beneath the charts shows the AI Usage image version. Release CI
embeds its image tag into the binary through `HOMESTACK_USAGE_VERSION`; local
builds without that compile-time value show `development`.

In each chart's table mode, the copy icon copies JSON from the currently
loaded snapshot without another request. It contains the selected filters and
time bounds, the displayed series with unrounded numeric values, and every
source Prometheus query with its matrix data (`metric` labels and original
`[timestamp, value]` pairs). Source values remain strings exactly as returned
by Prometheus. For estimated costs, source queries contain token measurements
while displayed series contain the calculated USD values. Clipboard access
uses the browser Clipboard API and requires a secure context (HTTPS or
localhost); the button reports copy failures.
