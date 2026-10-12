"""Local execution adapter for the shared Setup pipeline."""
from __future__ import annotations

from dataclasses import replace
import os
from pathlib import Path
import pwd
import socket
import stat
import subprocess

from .models import AppError


def local_configuration(cfg):
    account = pwd.getpwuid(os.getuid())
    entries = tuple(entry for entry in cfg.setup.items if entry.handler == "backup")
    groups = tuple(group for group in cfg.setup.groups if group.id in {e.group for e in entries})
    return replace(cfg, user_name=account.pw_name, user_uid=os.getuid(), user_gid=os.getgid(),
                   repo_owner=None, setup=replace(cfg.setup, groups=groups, items=entries))


def local_target() -> dict:
    return {"local": True, "vmid": 0, "name": socket.gethostname(),
            "home": str(Path.home()), "user": pwd.getpwuid(os.getuid()).pw_name}


def local_catalog(cfg, **_):
    from .setup_catalog import load_catalog
    return load_catalog(cfg, repositories=False)


class LocalSetup:
    local = True

    def __init__(self, cfg, target):
        self.home = Path(target["home"])
        self.cfg = cfg

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return None

    def verify_identity(self):
        from .setup_guest import load_registry

        info = self.home.lstat()
        if (os.getuid() == 0 or not stat.S_ISDIR(info.st_mode)
                or info.st_uid != os.getuid() or info.st_gid != os.getgid()
                or self.home != Path(pwd.getpwuid(os.getuid()).pw_dir)):
            raise AppError("Local Setup requires the current non-root user's owned home directory")
        load_registry(self.home, vmid=0)
        return {"ok": True}

    def command(self, command: str, *, check: bool = False):
        # Use the system runtime, not the uv environment running HomeStack.
        environment = {"HOME": str(self.home), "USER": self.cfg.user_name,
                       "LOGNAME": self.cfg.user_name, "LANG": "C.UTF-8",
                       "PATH": "/usr/local/bin:/usr/bin:/bin"}
        for key in ("XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"):
            if key in os.environ:
                environment[key] = os.environ[key]
        result = subprocess.run(["/bin/sh", "-c", "umask 077\n" + command],
                                cwd=self.home, env=environment, text=True,
                                capture_output=True, check=False)
        if check and result.returncode:
            raise AppError(f"Local Setup command failed (exit {result.returncode}); output withheld")
        return result
