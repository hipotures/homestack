"""Bounded, read-only observations over each survivor's independent SSH forward."""
import json
import pathlib
import subprocess
import sys
import time

result = {"observed_at_unix": time.time(), "commands": []}
for name in ["uptime", "sys/kernel/random/boot_id", "pressure/cpu", "pressure/io", "pressure/memory"]:
    try:
        result[name] = pathlib.Path("/proc", name).read_text()
    except OSError as error:
        result[name] = {"error": str(error)}

print(json.dumps(result), flush=True)

peer = "10.77.0.2" if sys.argv[1] == "pve1" else "10.77.0.1"
for command in [
    ["ping", "-n", "-c", "1", "-W", "1", peer],
    ["pvesh", "get", "/cluster/status", "--output-format", "json"],
    ["pvecm", "status"],
    ["corosync-cfgtool", "-s"],
    ["corosync-quorumtool", "-s"],
    ["ip", "-s", "-j", "link"],
    ["ip", "-j", "route"],
    ["ip", "-j", "neigh"],
    ["ss", "-uanp"],
    ["journalctl", "-u", "corosync", "--since=-5 minutes", "-n", "200", "--no-pager", "-o", "short-monotonic"],
    ["journalctl", "-k", "-n", "60", "--no-pager", "-o", "short-monotonic"],
]:
    record = {"command": command, "observed_at_unix": time.time()}
    try:
        output = subprocess.run(command, capture_output=True, text=True, errors="replace", timeout=2)
        record.update(exit_code=output.returncode, stdout=output.stdout, stderr=output.stderr)
    except (OSError, subprocess.TimeoutExpired) as error:
        record["error"] = str(error)
        if isinstance(error, subprocess.TimeoutExpired):
            # TimeoutExpired carries partial output as bytes even with text=True.
            for field in ["stdout", "stderr"]:
                partial = getattr(error, field)
                record[field] = partial.decode(errors="replace") if isinstance(partial, bytes) else partial
    print(json.dumps({"commands": [record]}), flush=True)
