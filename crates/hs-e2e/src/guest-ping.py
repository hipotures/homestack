"""Run the deployer's real ICMP probe inside the owned, isolated Proxmox lab."""
import ipaddress
import json
import os
import pathlib
import shlex
import sys
import uuid

# Accept only the exact bounded probe used by the public deployer. No host
# routes, network privileges, authentication changes or synthetic success.
directory, ssh, *arguments = sys.argv[1:]
if len(arguments) != 6 or arguments[:5] != ["-n", "-c", "1", "-W", "1"]:
    sys.exit("E2E ping permits only the deployer's single-packet probe")
try:
    address = ipaddress.IPv4Address(arguments[5])
    if address not in ipaddress.IPv4Network("10.77.0.0/24") or address.packed[-1] in (0, 255):
        raise ValueError()
except ValueError:
    sys.exit("E2E ping permits only isolated guest addresses in 10.77.0.0/24")
directory = pathlib.Path(directory)
run_id = str(uuid.UUID(json.loads((directory / "service.json").read_text())["run_id"]))
guard = ("import json,subprocess,sys; "
         f"assert json.load(open('/etc/homestack-e2e.json'))['run_id']=={run_id!r}; "
         "sys.exit(subprocess.call(sys.argv[1:]))")
os.execv(ssh, [ssh, "-F", str(directory / "ssh_config"), "-o", "BatchMode=yes",
              "-o", "ConnectTimeout=5", "pve-e2e",
              shlex.join(["python3", "-c", guard, "ping", *arguments])])
