#!/usr/bin/env python3
"""Validate a locally built Collector image before publishing it (no PVE access)."""
import json
import gzip
import http.server
from pathlib import Path
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
import uuid


def docker(*args):
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True).stdout


def main():
    image = sys.argv[1]
    config = json.loads(docker("image", "inspect", image))[0]["Config"]
    assert config["User"] == "10001:10001", "Collector must retain its nonroot runtime user"
    assert config["Entrypoint"] == ["/otelcol-homestack"]
    assert config["Cmd"] == ["--config=/etc/otelcol/homestack.yaml"]
    # Reserve a host port, but leave the destination unavailable until after
    # restart. Delivery then proves that the queued metric itself survived.
    with socket.socket() as reservation:
        reservation.bind(("", 0))
        destination_port = reservation.getsockname()[1]
    environment = ["-e", f"HOMESTACK_PROMETHEUS_METRICS_ENDPOINT=http://host.docker.internal:{destination_port}/api/v1/otlp/v1/metrics",
                   "-e", "HOMESTACK_SENDING_QUEUE_SIZE=1000"]
    name = "hs-otel-verify-" + uuid.uuid4().hex[:12]
    try:
        docker("create", "--name", name, "--cpus=1", "--memory=512m", "--memory-swap=512m",
               "--add-host=host.docker.internal:host-gateway",
               "-p", "127.0.0.1::13133", "-p", "127.0.0.1::4318", *environment, image)
        with tempfile.TemporaryDirectory(prefix="hs-otel-verify-") as temporary:
            archive = Path(temporary) / "rootfs.tar"
            docker("export", "-o", str(archive), name)
            with tarfile.open(archive) as rootfs:
                # Docker injects hosts/resolver files at runtime, while Proxmox
                # needs a recognized distribution to configure them. Scratch
                # silently selects its unmanaged plugin, which skips DNS setup.
                os_release = rootfs.extractfile("etc/os-release").read().decode()
                assert "ID=alpine\n" in os_release, "Proxmox must recognize the runtime as Alpine"
                assert rootfs.getmember("etc/alpine-release").isfile()
                assert rootfs.getmember("etc/network").isdir()
                assert rootfs.getmember("bin/busybox").isfile()
                queue = rootfs.getmember("var/lib/otelcol/queue")
                assert queue.isdir() and (queue.uid, queue.gid, queue.mode & 0o7777) == (10001, 10001, 0o700)
                unpacked = sum(member.size for member in rootfs)
                assert unpacked < 2 * 1024**3, "Image exceeds declared rootfs size"
        docker("run", "--rm", "--network=none", *environment, image, "validate", "--config=/etc/otelcol/homestack.yaml")
        docker("start", name)
        health = "http://" + docker("port", name, "13133/tcp").strip() + "/health/live"
        otlp = "http://" + docker("port", name, "4318/tcp").strip() + "/v1/metrics"

        def healthy():
            for _ in range(100):
                try:
                    with urllib.request.urlopen(health, timeout=1) as response:
                        if response.status == 200:
                            return
                except (OSError, urllib.error.URLError):
                    pass
                time.sleep(0.2)
            raise RuntimeError("Collector did not become healthy")

        healthy()
        metrics = {"resourceMetrics": [{"scopeMetrics": [{"metrics": [{"name": "homestack.image.validation",
                   "gauge": {"dataPoints": [{"asDouble": 1, "timeUnixNano": str(time.time_ns())}]}}]}]}]}
        request = urllib.request.Request(otlp, data=json.dumps(metrics).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            assert response.status == 200
        # The exporter cannot reach the deliberately absent destination. Its
        # retrying data must be backed by a file before an ordinary restart.
        time.sleep(7)
        before = json.loads(docker("inspect", name))[0]
        stats = docker("stats", "--no-stream", "--format", "{{json .}}", name).strip()
        with tempfile.TemporaryDirectory(prefix="hs-otel-queue-") as temporary:
            docker("cp", name + ":/var/lib/otelcol/queue/.", temporary)
            files = [p for p in Path(temporary).rglob("*") if p.is_file() and p.stat().st_size > 0]
            assert files, "Persistent exporter queue has no backing file"
            names = sorted(p.name for p in files)
        docker("restart", name)
        # Docker can allocate a new ephemeral published port on restart.
        health = "http://" + docker("port", name, "13133/tcp").strip() + "/health/live"
        healthy()
        after = json.loads(docker("inspect", name))[0]
        assert before["Id"] == after["Id"] and not after["State"]["OOMKilled"]
        with tempfile.TemporaryDirectory(prefix="hs-otel-queue-") as temporary:
            docker("cp", name + ":/var/lib/otelcol/queue/.", temporary)
            assert all((Path(temporary) / filename).is_file() for filename in names)
        received = threading.Event()

        class Receiver(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                if self.headers.get("Content-Encoding") == "gzip":
                    body = gzip.decompress(body)
                if self.path == "/api/v1/otlp/v1/metrics" and b"homestack.image.validation" in body:
                    received.set()
                self.send_response(200)
                self.send_header("Content-Type", "application/x-protobuf")
                self.end_headers()

            def log_message(self, *_):
                pass

        with http.server.ThreadingHTTPServer(("", destination_port), Receiver) as receiver:
            threading.Thread(target=receiver.serve_forever, daemon=True).start()
            try:
                assert received.wait(45), "Queued metric was not delivered after restart and receiver recovery"
            finally:
                receiver.shutdown()
        print(json.dumps({"image": image, "uid": 10001, "gid": 10001, "queue_mode": "0700",
                          "unpacked_bytes": unpacked, "runtime": "Docker OCI (not Proxmox LXC)",
                          "workload": "one OTLP metric with unreachable exporter and same-container restart",
                          "stats": json.loads(stats), "health": "passed", "queue_restart_and_delivery": "passed"}, indent=2))
    except Exception:
        for args in [("logs", "--tail", "100", name),
                     ("inspect", "--format", "{{json .State}} {{json .NetworkSettings.Ports}}", name)]:
            diagnostic = subprocess.run(["docker", *args], capture_output=True, text=True, check=False)
            print(diagnostic.stdout, diagnostic.stderr, file=sys.stderr)
        raise
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True, check=False)


if __name__ == "__main__":
    main()
