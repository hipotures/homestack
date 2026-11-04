#!/usr/bin/env python3
"""Export passive Antigravity CLI hook observations as OTLP/HTTP JSON logs."""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import syslog
import time
import urllib.request

EVENTS = ("PostToolUse", "PostInvocation", "Stop")
MAX_BYTES = 4 * 1024 * 1024


def attributes(values):
    result = []
    for key, value in values.items():
        kind = "boolValue" if isinstance(value, bool) else "intValue" if isinstance(value, int) else "stringValue"
        result.append({"key": key, "value": {kind: str(value) if kind == "intValue" else value}})
    return result


def invocation_usage(payload):
    """Read only the exact completed model step, never the previous response."""
    index = payload.get("initialNumSteps")
    path = payload.get("transcriptPath")
    if type(index) is not int or index < 0 or not isinstance(path, str):
        return {}
    with Path(path).expanduser().open("rb") as stream:
        stream.seek(0, 2)
        offset = max(0, stream.tell() - MAX_BYTES)
        stream.seek(offset)
        if offset:
            stream.readline()
        lines = stream.read(MAX_BYTES).splitlines()
    response = None
    for line in lines:
        try:
            row = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        if (isinstance(row, dict) and row.get("step_index") == index
                and row.get("source") == "MODEL" and row.get("status") == "DONE"):
            response = row
    if response is None:
        return {}
    counts = {}
    for source, target in [("input_tokens", "input"), ("cache_read_tokens", "cacheRead"),
                           ("cache_creation_input_tokens", "cacheCreation"), ("output_tokens", "output")]:
        if source in response:
            count = response[source]
            if type(count) is not int or not 0 <= count < 2**63:
                raise ValueError("Invalid token count")
            counts[target] = count
    return counts if "input" in counts and "output" in counts else {}


def export(endpoint, event, payload):
    project = socket.gethostname()
    for directory in payload.get("workspacePaths", []):
        result = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                                cwd=directory, capture_output=True, text=True, timeout=2)
        if result.returncode == 0:
            common = Path(result.stdout.strip())
            project = common.parent.name if common.name == ".git" else common.name
            break
    resource = attributes({"service.name": "antigravity-cli", "host.name": socket.gethostname(), "project": project})
    model = payload.get("modelName") or "unknown"
    values = {"event.name": "agy." + event, "model": model, "homestack.one": 1}
    if event == "PostToolUse":
        values.update(tool=payload.get("toolName") or payload.get("toolCall", {}).get("name") or "unknown",
                      success="false" if payload.get("error") else "true")
    if event == "Stop":
        values["termination_reason"] = payload.get("terminationReason") or "unknown"
        values["fully_idle"] = bool(payload.get("fullyIdle"))
    # The pinned sum connector adds the source value once per configured
    # attribute. One encoded dimension map keeps each observation counted once.
    dimensions = {key: values[key] for key in ("model", "tool", "success", "termination_reason") if key in values}
    values["agy.dimensions"] = json.dumps(dimensions)
    timestamp = str(time.time_ns())
    records = [{"timeUnixNano": timestamp, "attributes": attributes(values)}]
    if event == "PostInvocation":
        try:
            counts = invocation_usage(payload)
            for category, count in counts.items():
                records.append({"timeUnixNano": timestamp, "attributes": attributes({
                    "event.name": "agy.token_usage", "model": model, "type": category, "token_count": count,
                    "agy.dimensions": json.dumps({"model": model, "type": category}),
                })})
        except (OSError, ValueError):
            syslog.syslog(syslog.LOG_WARNING, "agy telemetry: token observation unavailable")
    document = {"resourceLogs": [{"resource": {"attributes": resource}, "scopeLogs": [{
        "scope": {"name": "homestack.agy"}, "logRecords": records,
    }]}]}
    request = urllib.request.Request(endpoint, data=json.dumps(document).encode(),
                                     headers={"Content-Type": "application/json"})
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=3) as response:
        result = json.loads(response.read(4096) or b"{}")
        if int(result.get("partialSuccess", {}).get("rejectedLogRecords", 0)):
            raise ValueError("Collector rejected observations")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("event", choices=EVENTS)
    parser.add_argument("--endpoint", required=True)
    args = parser.parse_args()
    passive = {"decision": ""} if args.event == "Stop" else {}
    try:
        raw = sys.stdin.buffer.read(MAX_BYTES + 1)
        if len(raw) > MAX_BYTES:
            raise ValueError("Hook payload too large")
        payload = json.loads(raw)
        if not isinstance(payload, dict):
            raise ValueError("Expected hook object")
        # Detach both process and inherited pipes so network export cannot
        # hold AGY's hook output open. The intermediate child is reaped here.
        child = os.fork()
        if child == 0:
            try:
                os.setsid()
                if os.fork() != 0:
                    os._exit(0)
                with open(os.devnull, "r+b") as null:
                    for descriptor in (0, 1, 2):
                        os.dup2(null.fileno(), descriptor)
                try:
                    export(args.endpoint, args.event, payload)
                except Exception as error:
                    syslog.syslog(syslog.LOG_WARNING, f"agy telemetry: observation dropped ({type(error).__name__})")
            finally:
                os._exit(0)
        os.waitpid(child, 0)
    except Exception as error:
        print(f"agy telemetry: observation dropped ({type(error).__name__})", file=sys.stderr)
    finally:
        print(json.dumps(passive))


if __name__ == "__main__":
    main()
