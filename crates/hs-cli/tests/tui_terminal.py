"""Verify explicit install access and native terminal copy mode in a private PTY."""

import fcntl
import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time
from pathlib import Path


with tempfile.TemporaryDirectory(prefix="homestack-tui-") as directory:
    root = Path(directory)
    config = root / "config/homestack"
    config.mkdir(parents=True)
    (config / "tui.toml").write_text(
        'install_method = "herdr"\ninstall_host = "old.invalid"\n'
    )
    pid, fd = pty.fork()
    if pid == 0:
        env = dict(os.environ)
        env.update(
            HOME=str(root),
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_STATE_HOME=str(root / "state"),
            XDG_CACHE_HOME=str(root / "cache"),
            TERM="xterm-256color",
        )
        env.pop("HOMESTACK_ASKPASS_SOCKET", None)
        os.execve(
            sys.argv[1],
            [sys.argv[1], "tui", "--ssh", "root@pve-e2e", "--server-domain", "e2e.homestack.app"],
            env,
        )
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    output = bytearray()

    def pump(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([fd], [], [], min(0.05, max(0, deadline - time.monotonic())))[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                output.extend(data)

    try:
        for _ in range(100):
            pump(0.05)
            if b"root@pve-e2e" in output:
                break
        assert b"HomeStack install" in output, "The install form did not open"
        assert b"root@pve-e2e" in output, "Explicit SSH host did not override saved access"
        start = len(output)
        os.write(fd, b"\x1bOQ")
        pump(0.2)
        assert b"Copy mode:" in output[start:], "F2 did not enter copy mode"
        assert b"\x1b[?1000l" in output[start:], "F2 did not release mouse capture"
        frozen = len(output)
        os.write(fd, b"\r")
        pump(1.2)
        assert len(output) == frozen, "Copy mode redrew the screen or accepted an install action"
        os.write(fd, b"\x1bOQ")
        pump(0.2)
        assert b"\x1b[?1000h" in output[frozen:], "F2 did not restore mouse capture"
        # A PTY deliberately does not answer cursor-position queries. Returning
        # from copy mode must not query the cursor or race the input reader.
        assert b"\x1b[6n" not in output[frozen:], "Copy mode queried the terminal cursor"
        assert b"Cannot change copy mode" not in output, bytes(output)
        os.write(fd, b"\x03")
        for _ in range(100):
            pump(0.05)
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                pid = None
                assert os.waitstatus_to_exitcode(status) == 0, bytes(output)
                break
        assert pid is None, "The TUI did not exit"
        assert b"\x1b[?1049l" in output, "The terminal was not restored"
    finally:
        if pid is not None:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
        os.close(fd)
