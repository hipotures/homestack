#!/usr/bin/python3
"""Exercise DHCP, DNS and public HTTPS from a temporary lab-only namespace."""

import http.client
import json
import pathlib
import secrets
import socket
import struct
import subprocess
import sys


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def client():
    mac = bytes.fromhex(json.loads(run("ip", "-j", "link", "show", "eth0"))[0]["address"].replace(":", ""))
    xid = secrets.randbits(32)
    address = None
    # DHCP starts before the interface has an IP address or a return route.
    with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0800)) as dhcp:
        dhcp.settimeout(2)
        dhcp.bind(("eth0", 0))

        def packet(kind, offered=None):
            header = struct.pack("!BBBBIHH4s4s4s4s16s64s128s", 1, 1, 6, 0, xid, 0, 0x8000,
                                 socket.inet_aton(address) if kind == 7 else b"\0" * 4,
                                 b"\0" * 4, b"\0" * 4, b"\0" * 4, mac, b"", b"")
            options = b"\x35\x01" + bytes([kind]) + b"\x3d\x07\x01" + mac
            if offered:
                options += b"\x32\x04" + socket.inet_aton(offered)
            if kind in (3, 7):
                options += b"\x36\x04" + socket.inet_aton("10.77.0.1")
            return header + b"\x63\x82\x53\x63" + options + b"\x37\x03\x01\x03\x06\xff"

        def broadcast(payload):
            udp = struct.pack("!4H", 68, 67, len(payload) + 8, 0) + payload
            header = struct.pack("!BBHHHBBH4s4s", 0x45, 0, len(udp) + 20, 0, 0, 64, 17, 0,
                                 b"\0" * 4, b"\xff" * 4)
            checksum = sum(struct.unpack("!10H", header))
            while checksum >> 16:
                checksum = (checksum & 0xffff) + (checksum >> 16)
            header = header[:10] + struct.pack("!H", ~checksum & 0xffff) + header[12:]
            dhcp.send(b"\xff" * 6 + mac + b"\x08\x00" + header + udp)

        def exchange(kind, expected, offered=None):
            for _ in range(3):
                broadcast(packet(kind, offered))
                while True:
                    try:
                        frame = dhcp.recv(4096)
                    except socket.timeout:
                        break
                    if len(frame) < 42 or frame[23] != 17:
                        continue
                    offset = 14 + (frame[14] & 15) * 4
                    if frame[offset:offset + 4] != struct.pack("!HH", 67, 68):
                        continue
                    reply = frame[offset + 8:]
                    if len(reply) < 240 or reply[0] != 2 or struct.unpack("!I", reply[4:8])[0] != xid:
                        continue
                    options = {}
                    offset = 240
                    while offset < len(reply):
                        code = reply[offset]
                        offset += 1
                        if code == 255:
                            break
                        if code == 0:
                            continue
                        length = reply[offset]
                        offset += 1
                        options[code] = reply[offset:offset + length]
                        offset += length
                    if options.get(53) == bytes([expected]):
                        return socket.inet_ntoa(reply[16:20]), options
            raise RuntimeError(f"Lab DHCP did not answer message {kind}")

        offered, _ = exchange(1, 2)
        address, options = exchange(3, 5, offered)
        try:
            assert address == offered and address.startswith("10.77.0.")
            assert 50 <= int(address.rsplit(".", 1)[1]) <= 99
            assert options[1] == socket.inet_aton("255.255.255.0")
            assert options[3] == socket.inet_aton("10.77.0.1")
            assert options[6] == socket.inet_aton("10.77.0.1")
            assert options[54] == socket.inet_aton("10.77.0.1")
            run("ip", "address", "add", address + "/24", "dev", "eth0")
            run("ip", "route", "add", "default", "via", "10.77.0.1")
            host = "deb.debian.org"
            # This process runs in a mount-isolated namespace created by ip netns exec.
            # Its own resolv.conf override is provided by the parent, never the PVE resolver.
            assert socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)
            connection = http.client.HTTPSConnection(host, timeout=15)
            try:
                connection.request("HEAD", "/debian/dists/trixie/InRelease")
                response = connection.getresponse()
                assert response.status == 200
            finally:
                connection.close()
            print(json.dumps({"address": address, "cidr": 24, "gateway": "10.77.0.1", "dns": "10.77.0.1",
                              "internet": "public IPv4 with host/private network isolation"}))
        finally:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as release:
                release.bind((address, 68))
                release.sendto(packet(7), ("10.77.0.1", 67))


if __name__ == "__main__":
    if sys.argv[1:] == ["client"]:
        client()
    else:
        with open("/etc/homestack-e2e.json") as marker:
            json.load(marker)
        suffix = secrets.token_hex(3)
        namespace, host, peer = "hs-e2e-check-" + suffix, "hse" + suffix, "hsp" + suffix
        resolver_dir = pathlib.Path("/etc/netns") / namespace
        resolver_dir.mkdir(parents=True)
        (resolver_dir / "resolv.conf").write_text("nameserver 10.77.0.1\n")
        try:
            run("ip", "netns", "add", namespace)
            run("ip", "link", "add", host, "type", "veth", "peer", "name", peer)
            run("ip", "link", "set", host, "master", "vmbr0")
            run("ip", "link", "set", host, "up")
            run("ip", "link", "set", peer, "netns", namespace)
            run("ip", "-n", namespace, "link", "set", peer, "name", "eth0")
            run("ip", "-n", namespace, "link", "set", "eth0", "up")
            run("ip", "-n", namespace, "link", "set", "lo", "up")
            subprocess.run(["ip", "netns", "exec", namespace, "python3", __file__, "client"], check=True, timeout=30)
        finally:
            subprocess.run(["ip", "link", "del", host], stderr=subprocess.DEVNULL)
            subprocess.run(["ip", "netns", "del", namespace], stderr=subprocess.DEVNULL)
            (resolver_dir / "resolv.conf").unlink()
            resolver_dir.rmdir()
