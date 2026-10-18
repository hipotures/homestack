"""Join a disposable node and require the Proxmox worker to finish successfully."""

import json
import re
import subprocess
import sys
import time


def join(arguments):
    command = ["pvesh", "create", "/cluster/config/join", "--output-format", "json"]
    for key, value in arguments.items():
        command.extend(["--" + key, value])
    result = subprocess.run(command, check=True, capture_output=True, text=True)
    print(result.stdout, end="", flush=True)
    print(result.stderr, end="", file=sys.stderr, flush=True)
    tasks = re.findall(r"UPID:[^\s\"']+", result.stdout)
    if len(tasks) != 1:
        raise RuntimeError("Cluster join did not return exactly one task identifier")
    node = subprocess.check_output(["hostname", "-s"], text=True).strip()
    for _ in range(120):
        status = json.loads(subprocess.check_output([
            "pvesh", "get", f"/nodes/{node}/tasks/{tasks[0]}/status", "--output-format", "json"
        ]))
        if status["status"] == "stopped":
            if status.get("exitstatus") != "OK":
                raise RuntimeError("Cluster join failed: " + status.get("exitstatus", "unknown worker result"))
            return
        time.sleep(1)
    raise RuntimeError("Cluster join worker did not finish before its deadline")
