"""Prepare the native PVE OCI cache without creating a container."""

import json
import subprocess
import sys

image, node, volume = sys.argv[1:]


def pve(*args):
    return json.loads(subprocess.check_output(["pvesh", *args, "--output-format", "json"]))


def cached():
    return any(
        item.get("volid") == volume
        for item in pve("get", f"/nodes/{node}/storage/local/content", "--content", "vztmpl")
    )


if cached():
    print(f"HomeStack image [REUSED]: {volume}", flush=True)
else:
    print(f"HomeStack image [PULLING]: {image}", flush=True)
    # The CLI waits for the worker and prints progress rather than the
    # asynchronous JSON task ID returned by the HTTP API.
    subprocess.run(
        ["pvesh", "create", f"/nodes/{node}/storage/local/oci-registry-pull", "--reference", image],
        check=True,
    )
    if not cached():
        raise RuntimeError("PVE OCI archive is absent after a successful pull")
    print(f"HomeStack image [READY]: {volume}", flush=True)
