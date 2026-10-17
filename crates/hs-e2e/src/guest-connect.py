#!/usr/bin/python3
"""Forward SSH only into this disposable lab's workload subnet."""

import ipaddress
import os
import pathlib
import sys

directory, ssh, host, port = sys.argv[1:]
try:
    address = ipaddress.ip_address(host)
    number = int(port)
    if address not in ipaddress.ip_network("10.77.0.0/24") or address in (
        ipaddress.ip_address("10.77.0.0"), ipaddress.ip_address("10.77.0.255")
    ) or not 1 <= number <= 65535:
        raise ValueError()
except ValueError:
    sys.exit("E2E SSH permits only guest addresses in 10.77.0.0/24 and valid TCP ports")
os.execv(ssh, [ssh, "-F", str(pathlib.Path(directory) / "ssh_config"), "-o", "ConnectTimeout=10",
              "-W", f"{address}:{number}", "pve-e2e"])
