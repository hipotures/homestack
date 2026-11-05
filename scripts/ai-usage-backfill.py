#!/usr/bin/env python3
"""Backfill AI usage from local Claude Code or Codex logs into Prometheus.

Scans session logs, deduplicates responses, compares the result with
Prometheus and writes an OpenMetrics file for `promtool tsdb
create-blocks-from openmetrics`. Only usage strictly before the first live
telemetry sample of this server (or of any unlabeled writer) is imported, so
backfilled and live samples never overlap.

Every backfilled series carries `backfill="logs"` and a session label. Values
are aggregated into fixed five-minute UTC buckets, so rescanning the same logs
produces identical samples; series already present in Prometheus are reported
as imported and skipped, and differing ones are reported as conflicts.

Standard library only. Run as the account that owns the agent logs.
"""

import argparse
import datetime as dt
import json
import math
import os
import re
import socket
import subprocess
import sys
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path

PRICING_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
BUCKET = 300
RETENTION_DAYS = 600
MARK = ("backfill", "logs")

CLAUDE_TYPES = ("input", "cacheRead", "cacheCreation", "output")
CODEX_TYPES = ("total", "input", "cached_input", "cache_write_input", "output", "reasoning_output")
LIVE_METRICS = {
    "claude": ["claude_code_token_usage_tokens_total"],
    "codex": ["codex_turn_token_usage_sum", "codex_response_token_usage"],
}


def parse_time(value):
    """Return epoch seconds for an ISO timestamp with an explicit zone, else None."""
    if not isinstance(value, str):
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed.timestamp() if parsed.tzinfo else None


def utc(seconds):
    return dt.datetime.fromtimestamp(seconds, dt.UTC).strftime("%Y-%m-%d %H:%M")


def count(value):
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


class Projects:
    """Same attribution as the live relay: Git common directory name, else hostname."""

    def __init__(self, host):
        self.host = host
        self.cache = {}

    def __call__(self, cwd):
        if not cwd:
            return self.host, "no-cwd"
        if cwd not in self.cache:
            self.cache[cwd] = self.resolve(cwd)
        return self.cache[cwd]

    def resolve(self, cwd):
        path = Path(cwd)
        existing = next(p for p in (path, *path.parents) if p.is_dir())
        repository = self.repository(existing)
        if existing == path:
            return repository or self.host, "exact"
        if repository:
            # A removed directory inside a repository belonged to that repository.
            return repository, "exact"
        if Path("/tmp") in path.parents:
            # Scratch directories were never repositories: the live default applies.
            return self.host, "exact"
        missing = path.relative_to(existing).parts[0]
        return missing, "guessed"

    @staticmethod
    def repository(directory):
        result = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=directory, text=True, capture_output=True, timeout=5, check=False,
        )
        if result.returncode != 0:
            return None
        common = Path(result.stdout.strip())
        return common.parent.name if common.name == ".git" else common.name


class Scan:
    def __init__(self):
        self.events = []  # (ts, project, labels, {metric: value}, session)
        self.sessions = {}  # session -> (ts, project, labels)
        self.skipped = defaultdict(int)
        self.versions = defaultdict(int)
        self.guessed = set()
        self.seen = set()

    def skip(self, reason):
        self.skipped[reason] += 1


def jsonl(path, scan):
    try:
        handle = open(path, encoding="utf-8", errors="replace")
    except OSError:
        scan.skip("unreadable file")
        return
    with handle:
        for line in handle:
            try:
                record = json.loads(line)
            except ValueError:
                scan.skip("invalid JSON line")
                continue
            if isinstance(record, dict):
                yield record


def rate(prices, field, prompt):
    """Base rate, or the highest '<field>_above_<N>k_tokens' tier the prompt exceeds."""
    best, limit = prices.get(field), -1
    for key, value in prices.items():
        match = re.fullmatch(re.escape(field) + r"_above_(\d+)k_tokens", key)
        if match and prompt > int(match.group(1)) * 1000 > limit:
            best, limit = value, int(match.group(1)) * 1000
    return best if isinstance(best, (int, float)) and math.isfinite(best) and best >= 0 else None


def claude_cost(pricing, model, usage, write_1h, scan):
    prices = pricing.get(model) if pricing else None
    if prices is None:
        scan.skip(f"cost: no price for {model}")
        return None
    if usage.get("speed") not in (None, "standard"):
        scan.skip(f"cost: unpriced speed {usage.get('speed')}")
        return None
    inp, read, write, out = (usage[k] for k in ("input", "cacheRead", "cacheCreation", "output"))
    prompt = inp + read + write
    parts = [
        (inp, rate(prices, "input_cost_per_token", prompt)),
        (read, rate(prices, "cache_read_input_token_cost", prompt)),
        (write - write_1h, rate(prices, "cache_creation_input_token_cost", prompt)),
        (write_1h, rate(prices, "cache_creation_input_token_cost_above_1hr", prompt)
         or rate(prices, "cache_creation_input_token_cost", prompt)),
        (out, rate(prices, "output_cost_per_token", prompt)),
    ]
    if any(tokens and price is None for tokens, price in parts):
        scan.skip(f"cost: incomplete price for {model}")
        return None
    return sum(tokens * price for tokens, price in parts if tokens)


def scan_claude(files, projects, pricing, scan):
    best = {}
    for path in files:
        for record in jsonl(path, scan):
            if record.get("type") != "assistant":
                continue
            message = record.get("message") or {}
            usage = message.get("usage")
            if not isinstance(usage, dict):
                continue
            scan.versions[record.get("version") or "unknown"] += 1
            model = message.get("model")
            if not model or model == "<synthetic>":
                scan.skip("synthetic or missing model")
                continue
            ts = parse_time(record.get("timestamp"))
            if ts is None:
                scan.skip("timestamp without zone")
                continue
            values = {
                "input": count(usage.get("input_tokens")),
                "output": count(usage.get("output_tokens")),
                "cacheRead": count(usage.get("cache_read_input_tokens", 0)),
                "cacheCreation": count(usage.get("cache_creation_input_tokens", 0)),
            }
            if any(v is None for v in values.values()):
                scan.skip("missing or invalid token counts")
                continue
            if not any(values.values()):
                scan.skip("zero usage")
                continue
            # Streamed content blocks repeat one response; resumed sessions copy it.
            key = (message.get("id"), record.get("requestId"))
            if None in key:
                key = (record.get("uuid"), None)
            split = (usage.get("cache_creation") or {}).get("ephemeral_1h_input_tokens")
            entry = (ts, record, values, count(split) or 0, usage.get("speed"))
            previous = best.get(key)
            if previous is None:
                best[key] = entry
            else:
                scan.skip("duplicate response")
                # Older versions stream growing output counts; keep the final one.
                if values["output"] > previous[2]["output"]:
                    best[key] = (min(ts, previous[0]),) + entry[1:]
    for ts, record, values, write_1h, speed in best.values():
        project, quality = projects(record.get("cwd"))
        if quality == "guessed":
            scan.guessed.add((record.get("cwd"), project))
        model = record["message"]["model"]
        session = record.get("sessionId") or "unknown"
        metrics = {("claude_code_token_usage_tokens_total", ("type", t)): values[t] for t in CLAUDE_TYPES}
        cost = claude_cost(pricing, model, dict(values, speed=speed), min(write_1h, values["cacheCreation"]), scan)
        if cost is not None:
            metrics[("claude_code_cost_usage_USD_total", None)] = cost
        labels = (("model", model), ("project", project), ("session_id", session))
        scan.events.append((ts, project, labels, metrics, session))
        if session not in scan.sessions or ts < scan.sessions[session][0]:
            scan.sessions[session] = (ts, project, (("project", project), ("session_id", session)))


def codex_usage(usage):
    values = {
        "input": count(usage.get("input_tokens")),
        "cached_input": count(usage.get("cached_input_tokens", 0)),
        "output": count(usage.get("output_tokens")),
        "reasoning_output": count(usage.get("reasoning_output_tokens", 0)),
        "total": count(usage.get("total_tokens")),
    }
    if "cache_write_input_tokens" in usage:
        values["cache_write_input"] = count(usage["cache_write_input_tokens"])
    return None if any(v is None for v in values.values()) else values


def scan_codex(files, projects, scan):
    for path in files:
        session, cwd, version = None, None, "unknown"
        turns, current = {}, (None, None, None)
        record_pending = False
        for record in jsonl(path, scan):
            kind, payload = record.get("type"), record.get("payload")
            if not isinstance(payload, dict):
                continue
            if kind == "session_meta":
                session = payload.get("id") or payload.get("session_id")
                cwd, version = payload.get("cwd"), payload.get("cli_version") or "unknown"
                continue
            if kind == "turn_context":
                settings = (payload.get("collaboration_mode") or {}).get("settings") or {}
                current = (payload.get("model"), payload.get("effort") or settings.get("reasoning_effort"),
                           payload.get("cwd") or cwd)
                turns[payload.get("turn_id")] = current
                continue
            if kind == "token_usage_record":
                usage = payload.get("usage")
                key = ("response", payload.get("response_id"))
                context = turns.get(payload.get("turn_id"), current)
                thread = payload.get("thread_id") or session
                record_pending = True
            elif kind == "event_msg" and payload.get("type") == "token_count":
                if record_pending:
                    record_pending = False
                    continue
                info = payload.get("info")
                if not isinstance(info, dict):
                    scan.skip("token_count without usage")
                    continue
                usage = info.get("last_token_usage")
                key = ("total", session, json.dumps(info.get("total_token_usage"), sort_keys=True))
                context, thread = current, session
            else:
                continue
            scan.versions[version] += 1
            if not isinstance(usage, dict) or (values := codex_usage(usage)) is None:
                scan.skip("missing or invalid token counts")
                continue
            if None in key:
                scan.skip("response without identity")
                continue
            if key in scan.seen:
                scan.skip("duplicate response")
                continue
            scan.seen.add(key)
            ts = parse_time(record.get("timestamp"))
            if ts is None:
                scan.skip("timestamp without zone")
                continue
            if not values["total"]:
                scan.skip("zero usage")
                continue
            model, effort, where = context
            if not model:
                scan.skip("no model context")
                continue
            project, quality = projects(where or cwd)
            if quality == "guessed":
                scan.guessed.add((where or cwd, project))
            labels = [("model", model), ("project", project), ("session_id", thread or "unknown")]
            if effort:
                labels.append(("effort", effort))
            metrics = {("codex_response_token_usage", ("token_type", t)): v for t, v in values.items()}
            scan.events.append((ts, project, tuple(sorted(labels)), metrics, thread))
            if thread and (thread not in scan.sessions or ts < scan.sessions[thread][0]):
                scan.sessions[thread] = (ts, project, (("model", model), ("project", project),
                                                       ("session_id", thread)))


class Prometheus:
    def __init__(self, url):
        self.url = url.rstrip("/")

    def get(self, path, **params):
        query = urllib.parse.urlencode(params)
        with urllib.request.urlopen(f"{self.url}{path}?{query}", timeout=120) as response:
            body = json.load(response)
        if body.get("status") != "success":
            raise RuntimeError(f"Prometheus error: {body}")
        return body["data"]["result"]

    def first_sample(self, selector, now):
        """Exact timestamp of the oldest retained sample matching the selector."""
        start = now - RETENTION_DAYS * 86400
        for step in (86400, 3600, 300):
            rows = self.get("/api/v1/query_range", query=f"count(count_over_time({selector}[{step}s]))",
                            start=start, end=now, step=step)
            points = [float(t) for row in rows for t, _ in row["values"]]
            if not points:
                return None
            end = min(points)
            start, now = end - step, end
        rows = self.get("/api/v1/query", query=f"{selector}[300s]", time=end)
        return min(float(t) for row in rows for t, _ in row["values"])


def cutoff(prom, agent, host):
    names = "|".join(LIVE_METRICS[agent])
    found = {}
    for scope, value in (("server", host), ("unlabeled", "")):
        selector = f'{{__name__=~"{names}",host_name="{value}",{MARK[0]}=""}}'
        found[scope] = prom.first_sample(selector, dt.datetime.now(dt.UTC).timestamp())
    times = [t for t in found.values() if t is not None]
    return (min(times) if times else None), found


def build(scan, agent, host, limit):
    """Aggregate events before the cutoff into deterministic series samples."""
    base = (("host_name", host), MARK)
    buckets = defaultdict(lambda: defaultdict(float))  # series -> bucket -> value
    projects = defaultdict(lambda: {"first": None, "last": None, "responses": 0, "tokens": 0,
                                    "sessions": set(), "series": set(), "late": 0})
    for ts, project, labels, metrics, session in scan.events:
        info = projects[project]
        if limit is not None and ts >= limit:
            info["late"] += 1
            continue
        info["first"] = ts if info["first"] is None else min(info["first"], ts)
        info["last"] = ts if info["last"] is None else max(info["last"], ts)
        info["responses"] += 1
        info["sessions"].add(session)
        for (name, extra), value in metrics.items():
            if extra == ("token_type", "total") or name == "claude_code_token_usage_tokens_total":
                info["tokens"] += value
            key = (name, tuple(sorted(base + labels + ((extra,) if extra else ()))))
            buckets[key][int(ts // BUCKET)] += value
            info["series"].add(key)
    starts = "claude_code_session_count_total" if agent == "claude" else "codex_thread_started_total"
    for session, (ts, project, labels) in scan.sessions.items():
        if limit is not None and ts >= limit:
            continue
        key = (starts, tuple(sorted(base + labels)))
        buckets[key][int(ts // BUCKET)] += 1
        projects[project]["series"].add(key)
    series = {}
    for key, values in buckets.items():
        gauge = key[0] == "codex_response_token_usage"
        total, points = 0.0, []
        for bucket in sorted(values):
            stamp = (bucket + 1) * BUCKET - 0.001
            if limit is not None:
                stamp = min(stamp, limit - 0.001)
            total += values[bucket]
            points.append((stamp, values[bucket] if gauge else total))
        # Counters are compared by final value, increments by their sum.
        series[key] = (points, total)
    return series, projects


def existing(prom, series, host):
    """Map each already backfilled series of this host to its comparable value."""
    found = {}
    for name in sorted({key[0] for key in series}):
        selector = f'{name}{{host_name="{host}",{MARK[0]}="{MARK[1]}"}}'
        func = "sum_over_time" if name == "codex_response_token_usage" else "last_over_time"
        for row in prom.get("/api/v1/query", query=f"{func}({selector}[{RETENTION_DAYS}d])"):
            labels = {k: v for k, v in row["metric"].items() if k != "__name__"}
            found[(name, tuple(sorted(labels.items())))] = float(row["value"][1])
    return found


def same(a, b):
    return math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-9)


def escape(value):
    return value.replace("\\", "\\\\").replace("\n", "\\n").replace('"', '\\"')


def write_openmetrics(path, series):
    by_name = defaultdict(list)
    for key in series:
        by_name[key[0]].append(key)
    with open(path, "w", encoding="utf-8") as out:
        for name in sorted(by_name):
            out.write(f"# TYPE {name} gauge\n")
            for key in sorted(by_name[name]):
                labels = ",".join(f'{k}="{escape(v)}"' for k, v in key[1])
                for stamp, value in series[key][0]:
                    out.write(f"{name}{{{labels}}} {value!r} {stamp:.3f}\n")
        out.write("# EOF\n")


def load_pricing(source):
    if source.startswith(("http://", "https://")):
        with urllib.request.urlopen(source, timeout=60) as response:
            catalog = json.load(response)
    else:
        catalog = json.loads(Path(source).read_text())
    prices = {}
    for name, value in catalog.items():
        if isinstance(value, dict) and value.get("litellm_provider") == "anthropic":
            prices[name.rsplit("/", 1)[-1]] = value
            prices[name] = value
    if not prices:
        raise RuntimeError("No Anthropic prices in the pricing catalog")
    return prices


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--agent", required=True, choices=("claude", "codex"))
    parser.add_argument("--source", action="append", type=Path,
                        help="log directory to scan, repeatable (default: the agent's own directory)")
    parser.add_argument("--host", default=socket.gethostname(),
                        help="server name the logs come from (default: this hostname)")
    parser.add_argument("--prometheus", default=os.environ.get("HOMESTACK_PROMETHEUS_URL"),
                        help="Prometheus base URL (default: $HOMESTACK_PROMETHEUS_URL)")
    parser.add_argument("--pricing", default=PRICING_URL, help="LiteLLM catalog URL or file (Claude cost)")
    parser.add_argument("--project", action="append",
                        help="import only this project, repeatable (default: all projects)")
    parser.add_argument("--prom-project",
                        help="Prometheus project name for all selected --project values (default: unchanged)")
    parser.add_argument("--output", type=Path, help="write new series as OpenMetrics for promtool")
    args = parser.parse_args()
    if not args.prometheus:
        parser.error("--prometheus or HOMESTACK_PROMETHEUS_URL is required")
    if args.prom_project and not args.project:
        parser.error("--prom-project requires at least one --project")

    home = Path.home()
    sources = args.source or [home / ".claude" / "projects" if args.agent == "claude" else home / ".codex" / "sessions"]
    pattern = "*.jsonl" if args.agent == "claude" else "rollout-*.jsonl"
    files = sorted({f for s in sources for f in s.expanduser().rglob(pattern) if f.is_file()})
    print(f"Agent {args.agent}, server {args.host}: {len(files)} log files in "
          + ", ".join(str(s) for s in sources))

    scan, projects = Scan(), Projects(args.host)
    if args.agent == "claude":
        scan_claude(files, projects, load_pricing(args.pricing), scan)
    else:
        scan_codex(files, projects, scan)

    if args.project:
        found_projects = {event[1] for event in scan.events}
        for name in sorted(set(args.project) - found_projects):
            print(f"  project {name!r} not found in these logs")
        scan.events = [e for e in scan.events if e[1] in args.project]
        scan.sessions = {k: v for k, v in scan.sessions.items() if v[1] in args.project}
    if args.prom_project:
        # Several log projects (e.g. a removed worktree) become one Prometheus project.
        name = args.prom_project

        def rename(labels):
            return tuple(sorted((k, name if k == "project" else v) for k, v in labels))

        scan.events = [(ts, name, rename(labels), metrics, session)
                       for ts, _, labels, metrics, session in scan.events]
        scan.sessions = {k: (ts, name, rename(labels)) for k, (ts, _, labels) in scan.sessions.items()}
        print(f"  importing {', '.join(args.project)} as project {name!r}")

    prom = Prometheus(args.prometheus)
    limit, found = cutoff(prom, args.agent, args.host)
    for scope, ts in found.items():
        print(f"  first live sample ({scope}): {utc(ts) + ' UTC' if ts else 'none'}")
    print(f"  importing usage strictly before: {utc(limit) + ' UTC' if limit else 'no live telemetry, everything'}")

    series, by_project = build(scan, args.agent, args.host, limit)
    present = existing(prom, series, args.host)
    def unnamed(key):
        return key[0], tuple(item for item in key[1] if item[0] != "project")

    elsewhere = {unnamed(key): dict(key[1]).get("project") for key in present}
    status, renamed = {}, set()
    for key, (_, value) in series.items():
        if key not in present and unnamed(key) in elsewhere:
            # The same session was already imported under another project name.
            status[key] = "conflict"
            renamed.add((elsewhere[unnamed(key)], dict(key[1]).get("project")))
        elif key not in present:
            status[key] = "new"
        elif same(present[key], value):
            status[key] = "imported"
        else:
            status[key] = "conflict"

    print(f"\n{'project':<28} {'first (UTC)':<17} {'last (UTC)':<17} {'resp':>7} {'tokens':>14} "
          f"{'sess':>5} {'after':>7}  status")
    for project in sorted(by_project):
        info = by_project[project]
        states = defaultdict(int)
        for key in info["series"]:
            states[status[key]] += 1
        state = ("nothing before cutoff" if not states else
                 "imported" if set(states) == {"imported"} else
                 ", ".join(f"{n} {s}" for s, n in sorted(states.items())) + " series")
        print(f"{project[:28]:<28} {utc(info['first']) if info['first'] else '-':<17} "
              f"{utc(info['last']) if info['last'] else '-':<17} {info['responses']:>7} "
              f"{info['tokens']:>14,} {len(info['sessions']):>5} {info['late']:>7}  {state}")
    print("  'after' = responses at or after the cutoff, left to live telemetry.")
    if scan.guessed:
        print(f"\nProject guessed from a removed directory outside Git: {len(scan.guessed)} paths")
        for path, project in sorted(scan.guessed)[:10]:
            print(f"  {path} -> {project}")
    print("\nSkipped records:")
    for reason, n in sorted(scan.skipped.items()) or [("none", 0)]:
        print(f"  {reason}: {n}")
    print("Records by agent version: " + ", ".join(f"{v}={n}" for v, n in sorted(scan.versions.items())))

    conflicts = [key for key, s in status.items() if s == "conflict"]
    for before, after in sorted(renamed):
        print(f"\nSessions already imported as project {before!r} are not exported again as {after!r}; "
              f"delete the {before!r} backfill first to rename them.")
    if conflicts:
        print(f"\n{len(conflicts)} series differ from already imported data and are NOT exported "
              "(scan all sources of these sessions together):")
        for name, labels in sorted(conflicts)[:10]:
            print(f"  {name} {dict(labels)}")
    new = {key: value for key, value in series.items() if status[key] == "new"}
    if not new:
        print("\nNothing new to import.")
        return
    if not args.output:
        print(f"\n{len(new)} series can be imported; rerun with --output FILE to write them.")
        return
    write_openmetrics(args.output, new)
    print(f"\nWrote {len(new)} series to {args.output}. Import inside the Prometheus container:\n"
          f"  promtool tsdb create-blocks-from openmetrics --max-block-duration=24h {args.output.name} /tmp/backfill-blocks\n"
          "  mv /tmp/backfill-blocks/* /prometheus/   # the TSDB data directory; Prometheus loads them on its own")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
