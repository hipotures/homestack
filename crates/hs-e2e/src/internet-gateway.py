#!/usr/bin/python3
"""Rootless user-space NAT with an external, fail-closed IPv4 boundary."""

import argparse
import asyncio
import ipaddress
import json
import os
import pathlib
import signal
import struct
import subprocess
import sys

WAN = ipaddress.ip_address("10.78.0.15")
DNS = ipaddress.ip_address("10.78.0.3")


def host_networks():
    routes = json.loads(subprocess.check_output(["ip", "-j", "route", "show", "table", "all"]))
    addresses = json.loads(subprocess.check_output(["ip", "-j", "address", "show"]))
    blocked = []
    for route in routes:
        if route.get("dst", "default") != "default":
            network = ipaddress.ip_network(route["dst"], strict=False)
            if network.version == 4:
                blocked.append(network)
    for interface in addresses:
        for address in interface.get("addr_info", []):
            if address["family"] == "inet":
                blocked.append(ipaddress.ip_network(address["local"] + "/32"))
    return blocked


def permitted(frame, blocked):
    if len(frame) < 14:
        return False
    kind = frame[12:14]
    if kind == b"\x08\x06":
        return (len(frame) >= 42 and frame[14:20] == b"\x00\x01\x08\x00\x06\x04"
                and frame[20:22] in (b"\x00\x01", b"\x00\x02")
                and frame[38:42] in (b"\x0a\x4e\x00\x02", b"\x0a\x4e\x00\x03"))
    if kind != b"\x08\x00" or len(frame) < 34 or frame[14] != 0x45:
        return False
    size = int.from_bytes(frame[16:18], "big")
    if size < 20 or size > len(frame) - 14 or frame[26:30] != WAN.packed:
        return False
    destination = ipaddress.ip_address(frame[30:34])
    if destination == DNS:
        # Only the virtual resolver, never arbitrary services on a host alias.
        return (frame[23] in (6, 17) and int.from_bytes(frame[20:22], "big") & 0x3fff == 0 and size >= 24
                and frame[36:38] == b"\x00\x35")
    return (destination.is_global and not destination.is_multicast and not destination.is_reserved
            and not any(destination in network for network in blocked))


async def packet(reader):
    length = struct.unpack("!I", await reader.readexactly(4))[0]
    if not 14 <= length <= 65536:
        raise ValueError("Invalid Ethernet frame length")
    return await reader.readexactly(length)


async def serve(args):
    directory = pathlib.Path(args.directory)
    state = directory / "internet.json"
    wire = directory / "internet-slirp.sock"
    filtered = directory / "internet-filter.sock"
    blocked = host_networks()
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    owner = os.pidfd_open(args.owner_pid)
    loop.add_reader(owner, stop.set)
    loop.add_reader(sys.stdin.fileno(), lambda: stop.set() if not os.read(0, 1024) else None)
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    children, connections = [], set()
    claimed = False
    try:
        qemu = await asyncio.create_subprocess_exec(args.qemu, "-machine", "none", "-nodefaults", "-display", "none", "-monitor", "none", "-serial", "none",
            "-netdev", "user,id=wan,net=10.78.0.0/24,host=10.78.0.2,dns=10.78.0.3,ipv6=off",
            "-netdev", f"stream,id=wire,addr.type=unix,addr.path={wire},server=on",
            "-netdev", "hubport,id=wanport,hubid=0,netdev=wan", "-netdev", "hubport,id=wireport,hubid=0,netdev=wire",
            stdin=asyncio.subprocess.DEVNULL)
        children.append(qemu)
        for _ in range(100):
            if qemu.returncode is not None:
                raise RuntimeError("The user-space NAT process stopped")
            if wire.exists():
                break
            await asyncio.sleep(0.05)
        else:
            raise RuntimeError("The user-space NAT socket did not appear")

        async def connection(reader, writer):
            nonlocal claimed
            if claimed:
                writer.close()
                return
            claimed = True
            upstream = None
            connections.add(writer)
            try:
                source, upstream = await asyncio.open_unix_connection(wire)
                connections.add(upstream)

                async def outgoing():
                    while True:
                        frame = await packet(reader)
                        if permitted(frame, blocked):
                            upstream.write(struct.pack("!I", len(frame)) + frame)
                            await upstream.drain()

                async def incoming():
                    while True:
                        frame = await packet(source)
                        writer.write(struct.pack("!I", len(frame)) + frame)
                        await writer.drain()

                tasks = [asyncio.create_task(outgoing()), asyncio.create_task(incoming())]
                try:
                    _, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                    for task in pending:
                        task.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
                finally:
                    for task in tasks:
                        task.cancel()
            except (OSError, asyncio.IncompleteReadError, ValueError) as error:
                print(f"Lab Internet connection closed: {error}", file=sys.stderr, flush=True)
            finally:
                writer.close()
                if upstream:
                    upstream.close()
                stop.set()

        server = await asyncio.start_unix_server(connection, filtered)
        os.chmod(filtered, 0o600)
        try:
            ssh = await asyncio.create_subprocess_exec("ssh", "-F", str(directory / "ssh_config"), "-N", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=2", "-R", f"127.0.0.1:3132:{filtered}", "pve-e2e", stdin=asyncio.subprocess.DEVNULL)
            children.append(ssh)
            await asyncio.sleep(0.5)
            if ssh.returncode is not None:
                raise RuntimeError("The lab Internet tunnel could not start")
            descriptor = os.open(state, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as output:
                json.dump({"pid": os.getpid(), "owner_pid": args.owner_pid, "nat_pid": qemu.pid, "ssh_pid": ssh.pid,
                           "blocked_host_networks": [str(network) for network in blocked]}, output)
            watchers = [asyncio.create_task(child.wait()) for child in children]
            waiter = asyncio.create_task(stop.wait())
            await asyncio.wait([*watchers, waiter], return_when=asyncio.FIRST_COMPLETED)
            for watcher in [*watchers, waiter]:
                watcher.cancel()
        finally:
            # Close active streams before waiting for the listener to close.
            # Python 3.13+ waits for them in Server.wait_closed().
            server.close()
            for stream in connections:
                stream.close()
            await server.wait_closed()
    finally:
        for stream in connections:
            stream.close()
        for child in reversed(children):
            if child.returncode is None:
                child.terminate()
                try:
                    await asyncio.wait_for(child.wait(), 5)
                except asyncio.TimeoutError:
                    child.kill()
                    await child.wait()
        for path in (state, wire, filtered):
            path.unlink(missing_ok=True)
        loop.remove_reader(owner)
        os.close(owner)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("directory")
    parser.add_argument("qemu")
    parser.add_argument("owner_pid", type=int)
    asyncio.run(serve(parser.parse_args()))
