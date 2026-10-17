#!/usr/bin/python3
"""A TAP confined to the disposable PVE VM, carried over pinned SSH."""

import fcntl
import os
import select
import socket
import struct
import subprocess

with open('/etc/homestack-e2e.json') as marker:
    assert 'run_id' in marker.read()
fd = os.open('/dev/net/tun', os.O_RDWR)
try:
    fcntl.ioctl(fd, 0x400454CA, struct.pack('16sH', b'hsewan0', 0x0002 | 0x1000))
    subprocess.run(['ip', 'address', 'add', '10.78.0.15/24', 'dev', 'hsewan0'], check=True)
    subprocess.run(['ip', 'link', 'set', 'hsewan0', 'up'], check=True)
    subprocess.run(['ip', 'route', 'replace', 'default', 'via', '10.78.0.2', 'dev', 'hsewan0'], check=True)
    subprocess.run(['resolvectl', 'dns', 'hsewan0', '10.78.0.3'], check=True)
    subprocess.run(['resolvectl', 'domain', 'hsewan0', '~.'], check=True)
    with socket.create_connection(('127.0.0.1', 3132), timeout=5) as tunnel:
        tunnel.settimeout(None)
        pending = bytearray()
        while True:
            ready, _, _ = select.select((fd, tunnel), (), ())
            if fd in ready:
                frame = os.read(fd, 65536)
                tunnel.sendall(struct.pack('!I', len(frame)) + frame)
            if tunnel in ready:
                data = tunnel.recv(65536)
                if not data:
                    break
                pending.extend(data)
                while len(pending) >= 4:
                    length = struct.unpack('!I', pending[:4])[0]
                    if not 14 <= length <= 65536:
                        raise ValueError('Invalid Internet frame length')
                    if len(pending) < 4 + length:
                        break
                    os.write(fd, pending[4:4 + length])
                    del pending[:4 + length]
finally:
    # Closing the only nonpersistent TAP descriptor also removes its routes.
    os.close(fd)
