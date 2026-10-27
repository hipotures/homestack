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
finishes a turn and exports metrics, query:

```promql
group by (host_name, project, instance) (
  codex_turn_token_usage_sum{token_type="total"}
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
reasoning row is a subset, not additional usage. Missing measurements and
unsupported metrics are marked explicitly instead of being displayed as zero.

The `AGENTS` registry in `crates/hs-usage/src/index.html` defines each agent's
metric selectors, token label names, token categories and optional capabilities.
Add an adapter there to include another agent in the comparison columns,
agent selector, scope discovery and shared charts. Declare whether input
already includes cached and cache-write tokens. Unreported capabilities remain
absent. Shared charts keep agent colors consistent and show adjacent bars;
model costs and agent-specific measurements retain their own breakdowns.
