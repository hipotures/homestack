#!/usr/bin/python3
"""Verify the boundary outside PVE and ownership of the gateway processes."""

import importlib.util
import ipaddress
import json
import os
import pathlib
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

SOURCE = pathlib.Path(sys.argv.pop(1)).resolve()
spec = importlib.util.spec_from_file_location("gateway", SOURCE)
gateway = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gateway)


def frame(destination, source="10.78.0.15", protocol=6, port=443, fragment=0):
    return (b"\0" * 12 + b"\x08\x00"
            + struct.pack("!BBHHHBBH4s4s", 0x45, 0, 40, 0, fragment, 64, protocol, 0,
                          socket.inet_aton(source), socket.inet_aton(destination))
            + struct.pack("!HH", 40000, port) + b"\0" * 16)


class Boundary(unittest.TestCase):
    def test_public_destinations_do_not_depend_on_distribution_or_port(self):
        for destination in ("1.1.1.1", "8.8.8.8", "151.101.2.132"):
            for port in (22, 53, 80, 443, 9418):
                for protocol in (6, 17):
                    self.assertTrue(gateway.permitted(frame(destination, port=port, protocol=protocol), []))

    def test_private_special_and_host_routes_are_blocked(self):
        blocked = [ipaddress.ip_network("8.8.8.8/32"), ipaddress.ip_network("151.101.0.0/16")]
        for destination in ("0.0.0.0", "10.0.2.2", "10.78.0.2", "10.77.0.1", "127.0.0.1",
                            "169.254.169.254", "172.16.0.1", "192.168.1.1", "100.100.100.200",
                            "224.0.0.1", "240.0.0.1", "255.255.255.255", "8.8.8.8", "151.101.2.132"):
            with self.subTest(destination=destination):
                self.assertFalse(gateway.permitted(frame(destination), blocked))
        self.assertFalse(gateway.permitted(frame("1.1.1.1", source="10.77.0.101"), []))

    def test_virtual_dns_is_limited_to_unfragmented_dns(self):
        for protocol in (6, 17):
            for fragment in (0, 0x4000):
                self.assertTrue(gateway.permitted(frame("10.78.0.3", protocol=protocol, port=53,
                                                       fragment=fragment), []))
        for options in ({"port": 80}, {"protocol": 1, "port": 53}, {"fragment": 1, "port": 53},
                        {"fragment": 0x2000, "port": 53}):
            self.assertFalse(gateway.permitted(frame("10.78.0.3", **options), []))

    def test_ethernet_encapsulation_and_malformed_packets_fail_closed(self):
        packet = frame("1.1.1.1")
        for size in range(34):
            self.assertFalse(gateway.permitted(packet[:size], []))
        for offset, replacement in ((12, b"\x86\xdd"), (12, b"\x81\x00"), (14, b"\x46"),
                                    (16, b"\xff\xff"), (16, b"\0\x13")):
            modified = packet[:offset] + replacement + packet[offset + len(replacement):]
            self.assertFalse(gateway.permitted(modified, []))

    def test_arp_only_reaches_virtual_gateway_and_resolver(self):
        for destination, expected in (("10.78.0.2", True), ("10.78.0.3", True), ("10.0.2.2", False)):
            arp = (b"\xff" * 6 + b"\0" * 6 + b"\x08\x06\x00\x01\x08\x00\x06\x04\x00\x01"
                   + b"\0" * 6 + socket.inet_aton("10.78.0.15") + b"\0" * 6 + socket.inet_aton(destination))
            self.assertEqual(gateway.permitted(arp, []), expected)


class Lifecycle(unittest.TestCase):
    def exercise(self, cause):
        with tempfile.TemporaryDirectory(prefix="hs-e2e-internet-test-") as path:
            directory = pathlib.Path(path)
            qemu = directory / "qemu"
            qemu.write_text("#!" + sys.executable + "\n" + '''
import pathlib, signal, socket, sys
wire = next(arg.split('addr.path=', 1)[1].split(',', 1)[0] for arg in sys.argv if 'addr.path=' in arg)
with socket.socket(socket.AF_UNIX) as server:
    server.bind(wire)
    server.listen()
    signal.pause()
''')
            ssh = directory / "ssh"
            ssh.write_text("#!" + sys.executable + "\nimport signal\nsignal.pause()\n")
            for executable in (qemu, ssh):
                executable.chmod(0o700)
            environment = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"])
            owner = subprocess.Popen([sys.executable, "-c", "import signal;signal.pause()"])
            helper = subprocess.Popen([sys.executable, str(SOURCE), str(directory), str(qemu), str(owner.pid)],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment)
            state = directory / "internet.json"
            children = []
            connection = socket.socket(socket.AF_UNIX)
            try:
                deadline = time.monotonic() + 10
                while not state.exists():
                    if helper.poll() is not None or time.monotonic() > deadline:
                        self.fail("Gateway failed to start: " + helper.communicate(timeout=2)[1].decode())
                    time.sleep(0.02)
                data = json.loads(state.read_text())
                children = [data["nat_pid"], data["ssh_pid"]]
                self.assertEqual(state.stat().st_mode & 0o777, 0o600)
                connection.connect(str(directory / "internet-filter.sock"))
                time.sleep(0.05)
                if cause == "eof":
                    helper.stdin.close()
                    helper.stdin = None
                elif cause == "owner":
                    owner.kill()
                    owner.wait()
                elif cause == "child":
                    os.kill(data["nat_pid"], signal.SIGTERM)
                else:
                    helper.terminate()
                output, errors = helper.communicate(timeout=10)
                self.assertEqual(helper.returncode, 0, (output + errors).decode())
                self.assertFalse(state.exists())
                for name in ("internet-filter.sock", "internet-slirp.sock"):
                    self.assertFalse((directory / name).exists())
                for pid in children:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
            finally:
                connection.close()
                for process in (helper, owner):
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)
                for pid in children:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_eof_reaps_gateway_and_tunnel(self):
        self.exercise("eof")

    def test_killed_owner_reaps_gateway_and_tunnel(self):
        self.exercise("owner")

    def test_child_failure_reaps_the_other_child(self):
        self.exercise("child")

    def test_termination_reaps_gateway_and_tunnel(self):
        self.exercise("signal")


if __name__ == "__main__":
    unittest.main()
