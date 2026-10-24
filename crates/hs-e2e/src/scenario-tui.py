"""Drive the actual installed client's non-mutating installer cancellation journey."""
import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
child = subprocess.Popen([sys.argv[1], "tui"], stdin=slave, stdout=slave, stderr=slave,
                         env={**os.environ, "TERM": "xterm-256color"})
output = bytearray()


def wait_for(text, limit=45):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if select.select([master], [], [], .1)[0]:
            output.extend(os.read(master, 65536))
        plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(output)).decode(errors="replace")
        if text in plain:
            return
        if child.poll() is not None:
            break
    raise RuntimeError("TUI did not display " + repr(text))


try:
    wait_for("settings")
    os.write(master, b"\x1b[21~")  # F10: Settings.
    wait_for("General")
    os.write(master, b"\r")
    wait_for("Reinstall")
    os.write(master, b"\x1b[B\x1b[B\r")
    wait_for("HomeStack reinstall")
    os.write(master, b"\x1b")  # Cancel on access, before opening root.
    time.sleep(.2)
    os.write(master, b"\x11")  # Ctrl+Q: quit the actual TUI.
    child.wait(timeout=10)
    assert child.returncode == 0, "TUI exited unsuccessfully"
    print("Opened Settings > General > Reinstall; cancelled before root access; exited cleanly.")
except Exception:
    # Screen data can contain infrastructure metadata, but this journey never
    # opens root, displays token files, or requests authentication material.
    print(re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(output)).decode(errors="replace")[-12000:])
    raise
finally:
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    os.close(master)
    os.close(slave)
