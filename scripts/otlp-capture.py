#!/usr/bin/env python3
"""Capture an agent's OTLP/HTTP telemetry locally and summarize it.

Run it in an empty directory; every request body is written there and a
summary of event names, metric names and attribute keys is printed and saved
as summary.txt when you stop it with Ctrl+C. Nothing leaves this machine.

    python3 otlp-capture.py              # listen on 127.0.0.1:4319
    python3 otlp-capture.py --summary    # summarize captures in this directory

Point an agent at it for one session, for example:

    codex -c 'otel.exporter={otlp-http={endpoint="http://127.0.0.1:4319/v1/logs",protocol="binary"}}' \\
          -c 'otel.metrics_exporter={otlp-http={endpoint="http://127.0.0.1:4319/v1/metrics",protocol="binary"}}'

    CLAUDE_CODE_ENABLE_TELEMETRY=1 OTEL_LOGS_EXPORTER=otlp OTEL_METRICS_EXPORTER=otlp \\
    OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4319 claude

Only the binary protobuf encoding is decoded. The summary also says what the
HomeStack Collector keeps: Codex response.completed token counts with model and
effort, and every metric; all other log records are discarded.
"""
import argparse
import collections
import gzip
import http.server
import itertools
import os
import struct
import sys

# ---- minimal protobuf reader ----


def fields(data):
    """Yield (field number, wire type, value) of one protobuf message."""
    i = 0
    while i < len(data):
        key, i = varint(data, i)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, i = varint(data, i)
        elif wire == 1:
            value, i = data[i:i + 8], i + 8
        elif wire == 2:
            size, i = varint(data, i)
            value, i = data[i:i + size], i + size
        elif wire == 5:
            value, i = data[i:i + 4], i + 4
        else:
            raise ValueError(f"unsupported wire type {wire}")
        yield number, wire, value


def varint(data, i):
    shift = result = 0
    while True:
        byte = data[i]
        i += 1
        result |= (byte & 0x7F) << shift
        if byte < 0x80:
            return result, i
        shift += 7


def sub(data, number):
    return [value for n, wire, value in fields(data) if n == number and wire == 2]


def any_value(data):
    for number, wire, value in fields(data):
        if number == 1:
            return value.decode(errors="replace")
        if number == 2:
            return bool(value)
        if number == 3:
            return value - (1 << 64) if value >= 1 << 63 else value
        if number == 4:
            return struct.unpack("<d", value)[0]
        if number in (5, 6):
            return "[list]" if number == 5 else "{map}"
        if number == 7:
            return f"<{len(value)} bytes>"
    return None


def attributes(messages):
    out = {}
    for kv in messages:
        key = next((v.decode() for n, _, v in fields(kv) if n == 1), "")
        value = next((any_value(v) for n, _, v in fields(kv) if n == 2), None)
        out[key] = value
    return out


# ---- OTLP structure ----

# Attribute field number of each metric data point type (gauge, sum, histogram,
# exponential histogram, summary).
POINT_ATTRIBUTES = {5: 7, 7: 7, 9: 9, 10: 1, 11: 7}


def logs(body):
    for resource_logs in sub(body, 1):
        resource = attributes(sum((sub(r, 1) for r in sub(resource_logs, 1)), []))
        for scope_logs in sub(resource_logs, 2):
            for record in sub(scope_logs, 2):
                attrs = attributes(sub(record, 6))
                event = next((v.decode() for n, w, v in fields(record) if n == 12 and w == 2), "")
                yield attrs.get("event.name") or event or "(no event.name)", attrs, resource


def metrics(body):
    for resource_metrics in sub(body, 1):
        resource = attributes(sum((sub(r, 1) for r in sub(resource_metrics, 1)), []))
        for scope_metrics in sub(resource_metrics, 2):
            for metric in sub(scope_metrics, 2):
                name = next((v.decode() for n, w, v in fields(metric) if n == 1 and w == 2), "?")
                for kind, field in POINT_ATTRIBUTES.items():
                    for data in sub(metric, kind):
                        for point in sub(data, 1):
                            yield name, attributes(sub(point, field)), resource


def kept_by_collector(event, attrs):
    return event == "codex.sse_event" and attrs.get("event.kind") == "response.completed"


# Attributes the Collector turns into Codex token metrics.
CODEX_KEPT = {
    "event.name", "event.kind", "model", "model_reasoning_effort", "tool_token_count",
    "input_token_count", "cached_token_count", "cache_write_token_count",
    "output_token_count", "reasoning_token_count",
}


# ---- summary ----


def short(value):
    text = repr(value)
    return text if len(text) <= 60 else text[:57] + "..."


def summarize(directory):
    log_events = collections.defaultdict(lambda: [0, collections.Counter(), {}])
    metric_points = collections.defaultdict(lambda: [0, collections.Counter(), {}])
    resources = collections.Counter()
    files = sorted(f for f in os.listdir(directory) if f.endswith(".bin"))
    for name in files:
        with open(os.path.join(directory, name), "rb") as file:
            body = file.read()
        try:
            rows = list(logs(body)) if "_logs" in name else list(metrics(body)) if "_metrics" in name else []
        except (ValueError, IndexError) as error:
            print(f"{name}: cannot decode ({error}); only binary protobuf is supported", file=sys.stderr)
            continue
        target = log_events if "_logs" in name else metric_points
        for key, attrs, resource in rows:
            if "_logs" in name and kept_by_collector(key, attrs):
                key += " [response.completed]"
            entry = target[key]
            entry[0] += 1
            entry[1].update(attrs.keys())
            for k, v in attrs.items():
                entry[2].setdefault(k, v)
            resources.update(resource.keys())
    lines = [f"Captured requests: {len(files)}", ""]
    lines.append("LOG EVENTS (count; attribute: records carrying it, example)")
    lines.append("The HomeStack Collector keeps only Codex response.completed token counts,")
    lines.append("model and effort; everything else below is discarded.")
    total = dropped = 0
    for event, (count, keys, examples) in sorted(log_events.items(), key=lambda e: -e[1][0]):
        total += count
        kept = event.endswith("[response.completed]")
        if not kept:
            dropped += count
        lines.append(f"\n  {event}: {count}{'' if kept else '   (discarded entirely)'}")
        for key, n in sorted(keys.items()):
            mark = "kept   " if kept and key in CODEX_KEPT else "dropped"
            lines.append(f"    {mark}  {key}: {n}  e.g. {short(examples[key])}")
    lines.append(f"\n  Log records: {total}, discarded entirely: {dropped}")
    lines.append("\nMETRICS (data points; attribute: points carrying it, example)")
    lines.append("The Collector forwards every metric with all attributes to Prometheus.")
    for metric, (count, keys, examples) in sorted(metric_points.items()):
        lines.append(f"\n  {metric}: {count}")
        for key, n in sorted(keys.items()):
            lines.append(f"    {key}: {n}  e.g. {short(examples[key])}")
    lines.append("\nRESOURCE ATTRIBUTES (records or points carrying them)")
    for key, n in sorted(resources.items()):
        lines.append(f"  {key}: {n}")
    text = "\n".join(lines) + "\n"
    with open(os.path.join(directory, "summary.txt"), "w") as file:
        file.write(text)
    print(text)
    print(f"Saved {os.path.join(directory, 'summary.txt')}")


# ---- receiver ----


def serve(directory, port):
    counter = itertools.count(len([f for f in os.listdir(directory) if f.endswith(".bin")]))

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.headers.get("Content-Encoding") == "gzip":
                body = gzip.decompress(body)
            kind = self.path.rstrip("/").rsplit("/", 1)[-1]
            name = f"{next(counter):05d}_{kind}.bin"
            with open(os.path.join(directory, name), "wb") as file:
                file.write(body)
            print(f"{name}: {len(body)} bytes", flush=True)
            self.send_response(200)
            self.send_header("Content-Type", "application/x-protobuf")
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"Listening on http://127.0.0.1:{port}, writing to {directory}; Ctrl+C to stop and summarize.")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print()
    finally:
        server.server_close()
    summarize(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=4319)
    parser.add_argument("--summary", action="store_true", help="only summarize captures in this directory")
    args = parser.parse_args()
    directory = os.getcwd()
    if args.summary:
        summarize(directory)
    else:
        serve(directory, args.port)


if __name__ == "__main__":
    main()
