#!/usr/bin/env python3
"""Focused release-image admission using the official MCP Python SDK (mcp==2.3.0).

Run with uv run --with mcp==2.3.0 deploy/metrics/verify-image.py IMAGE.
Use --runtime podman locally, or --binary PATH for native diagnostics.
Only a disposable local Prometheus fixture is contacted.
"""
import argparse
import asyncio
import json
import os
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit
from urllib.request import urlopen

from mcp.shared.exceptions import MCPError
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client

EXPECTED = {
    "prometheus_query", "prometheus_query_range", "prometheus_series",
    "prometheus_label_names", "prometheus_label_values", "prometheus_metadata",
}
ENVELOPE = {"status": "success", "data": {"resultType": "vector", "result": [
    {"metric": {"__name__": "up", "job": "fixture"}, "value": [123.125, "1.00000000000001"]}
]}, "warnings": ["fixture warning"], "infos": ["fixture info"]}


class Fixture(BaseHTTPRequestHandler):
    requests = []
    active = 0
    peak = 0
    lock = threading.Lock()

    def do_GET(self):
        url = urlsplit(self.path)
        params = parse_qs(url.query)
        with self.lock:
            self.requests.append((url.path, params))
            type(self).active += 1
            type(self).peak = max(self.peak, self.active)
        try:
            value = {"status": "success", "data": ["up", "fixture_metric"]}
            status = 200
            if url.path in ("/api/v1/query", "/api/v1/query_range"):
                query = params["query"][0]
                if query == "slow":
                    time.sleep(0.4)
                if query == "upstream_error":
                    value = {"status": "error", "errorType": "bad_data", "error": "fixture parse error"}
                    status = 422
                elif query == "oversized":
                    value = {"status": "success", "data": "x" * (8 * 1024 * 1024)}
                else:
                    value = ENVELOPE
            elif url.path == "/api/v1/metadata":
                value = {"status": "success", "data": {"up": [{"type": "gauge", "unit": "", "help": "Fixture availability", "extra": "preserved"}]}}
            elif url.path == "/api/v1/series":
                value = {"status": "success", "data": [{"__name__": "up", "job": "fixture"}]}
            elif url.path not in ("/api/v1/labels", "/api/v1/label/__name__/values"):
                raise AssertionError(f"Unexpected upstream path: {url.path}")
            body = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                # Metrics deliberately closes oversized responses before consuming them.
                pass
        finally:
            with self.lock:
                type(self).active -= 1

    def log_message(self, *_):
        pass


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


async def protocol(base):
    async with streamable_http_client(base + "/mcp") as (read, write):
        async with ClientSession(read, write) as session:
            initialized = await session.initialize()
            assert initialized.server_info.name == "metrics"
            print("initialize:", initialized.model_dump_json(by_alias=True, exclude_none=True))
            listed = await session.list_tools()
            print("tools/list:", listed.model_dump_json(by_alias=True, exclude_none=True))
            assert {t.name for t in listed.tools} == EXPECTED
            assert all(t.description and t.input_schema.get("type") == "object" and t.input_schema.get("properties") for t in listed.tools)
            names = await session.call_tool("prometheus_label_values", {"label": "__name__"})
            assert not names.is_error and names.structured_content["data"] == ["up", "fixture_metric"]
            query = 'sum(up{job="fixture"})'
            result = await session.call_tool("prometheus_query", {"query": query, "time": 123.125, "limit": 3})
            assert not result.is_error and result.structured_content == ENVELOPE
            assert json.loads(result.content[0].text) == ENVELOPE
            assert Fixture.requests[-1] == ("/api/v1/query", {"query": [query], "time": ["123.125"], "limit": ["3"]})
            print("prometheus_query:", result.model_dump_json(by_alias=True, exclude_none=True))
            # Long histories are allowed when the evaluation point count is small.
            result = await session.call_tool("prometheus_query_range", {"query": "up", "start": "2025-01-01T00:00:00Z", "end": "2026-01-01T00:00:00Z", "step": "1d"})
            assert not result.is_error
            result = await session.call_tool("prometheus_series", {"matchers": ["up", '{job="fixture"}'], "start": 0, "end": 1})
            assert not result.is_error
            assert Fixture.requests[-1][1]["match[]"] == ["up", '{job="fixture"}']
            assert not (await session.call_tool("prometheus_label_names", {})).is_error
            result = await session.call_tool("prometheus_metadata", {"metric": "up", "limit_per_metric": 2})
            assert not result.is_error and result.structured_content["data"]["up"][0]["extra"] == "preserved"
            before = len(Fixture.requests)
            invalid = [
                ("prometheus_query_range", {"query": "up", "start": 0, "end": 11000, "step": 1}),
                ("prometheus_query_range", {"query": "up", "start": 2, "end": 1, "step": "1s"}),
                ("prometheus_query_range", {"query": "up", "start": 0, "end": 1, "step": "NaN"}),
                ("prometheus_query", {"query": "up", "time": "Infinity"}),
                ("prometheus_query", {"query": "x" * 32769}),
                ("prometheus_query", {"query": "up", "limit": 0}),
                ("prometheus_series", {"matchers": []}),
                ("prometheus_label_values", {"label": "../../admin/tsdb/delete_series"}),
                ("prometheus_label_values", {"label": "__name__%2f.."}),
                ("prometheus_request", {"method": "POST", "path": "/-/reload"}),
                ("prometheus_query", {"query": "up", "path": "/api/v1/status/config"}),
            ]
            for name, arguments in invalid:
                try:
                    result = await session.call_tool(name, arguments)
                    assert result.is_error, (name, arguments, result)
                except MCPError as error:
                    assert error.code in (-32601, -32602), error
            assert len(Fixture.requests) == before, "Invalid requests reached upstream"
            result = await session.call_tool("prometheus_query", {"query": "upstream_error"})
            assert result.is_error and result.structured_content == {"status": "error", "errorType": "bad_data", "error": "fixture parse error"}
            assert (await session.call_tool("prometheus_query", {"query": "oversized"})).is_error
            results = await asyncio.gather(*(session.call_tool("prometheus_query", {"query": "slow"}) for _ in range(24)))
            assert 1 < Fixture.peak <= 16, Fixture.peak
            assert sum(not r.is_error for r in results) >= 2
            print(f"Concurrency: upstream peak={Fixture.peak}, successful={sum(not r.is_error for r in results)}/24; invalid inputs rejected before upstream")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", nargs="?")
    parser.add_argument("--runtime", default="docker")
    parser.add_argument("--binary")
    args = parser.parse_args()
    if not args.image and not args.binary:
        parser.error("Supply IMAGE or --binary PATH")
    fixture = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    env = {"HOMESTACK_PROMETHEUS_URL": f"http://127.0.0.1:{fixture.server_port}/ignored?ignored=yes#ignored", "HOMESTACK_HEARTBEAT_SECONDS": "1", "HOMESTACK_LISTEN": f"127.0.0.1:{port}"}
    process = None
    container = None
    try:
        if args.binary:
            process = subprocess.Popen([args.binary], env={**os.environ, **env})
        else:
            command = [args.runtime, "run", "-d", "--network=host", "--cpus=1", "--memory=256m", "--memory-swap=256m"]
            for key, value in env.items():
                command.extend(["-e", f"{key}={value}"])
            container = subprocess.check_output(command + [args.image], text=True).strip()
        for _ in range(100):
            try:
                with urlopen(base + "/health/live", timeout=1) as response:
                    assert response.headers.get_content_type() == "application/health+json"
                    assert json.load(response)["status"] == "pass"
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("Metrics did not become live")
        time.sleep(2)
        for path in ["/health/ready", "/.well-known/homestack/status"]:
            with urlopen(base + path) as response:
                value = json.load(response)
                assert value["status"] == "pass"
                if "checks" in value:
                    assert value["checks"]["heartbeat:count"][0]["observedValue"] > 0
                else:
                    assert value["schema"] == 3 and value["process_rss_bytes"] > 0
                print(path, value)
        asyncio.run(protocol(base))
        if container:
            subprocess.run([args.runtime, "stats", "--no-stream", container], check=True)
            info = json.loads(subprocess.check_output([args.runtime, "inspect", container], text=True))[0]
            assert not info["State"].get("OOMKilled", False)
            subprocess.run([args.runtime, "exec", container, "cat", "/sys/fs/cgroup/memory.peak", "/sys/fs/cgroup/memory.events"], check=True)
            subprocess.run([args.runtime, "image", "inspect", "--format", "{{.Size}}", args.image], check=True)
        print("Metrics focused admission passed")
    finally:
        if process:
            process.terminate()
            process.wait(timeout=10)
        if container:
            subprocess.run([args.runtime, "rm", "-f", container], check=True)
        fixture.shutdown()


if __name__ == "__main__":
    main()
