"""Read one mounted virtiofs share identified by its HomeStack tag."""

import json
import os
import re
import signal
import sys


def unescape(value):
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), value)


def read(tag):
    if not re.fullmatch(r"HS_HOME_[0-9]+", tag):
        raise ValueError("Invalid HomeStack home tag")
    matches = []
    with open("/proc/self/mountinfo", encoding="utf-8") as mounts:
        for line in mounts:
            fields, filesystem = line.rstrip().split(" - ", 1)
            filesystem = filesystem.split()
            if filesystem[0] == "virtiofs" and unescape(filesystem[1]) == tag:
                fields = fields.split()
                matches.append((unescape(fields[4]), fields[2]))
    if len(matches) != 1:
        return None
    mountpoint, device = matches[0]
    fd = os.open(mountpoint, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        actual = os.fstat(fd).st_dev
        if device != f"{os.major(actual)}:{os.minor(actual)}":
            return None
        stats = os.fstatvfs(fd)
    finally:
        os.close(fd)
    return {
        "tag": tag,
        "mountpoint": mountpoint,
        "size_bytes": stats.f_blocks * stats.f_frsize,
        "used_bytes": (stats.f_blocks - stats.f_bfree) * stats.f_frsize,
        "free_bytes": stats.f_bavail * stats.f_frsize,
    }


def timeout(signum, frame):
    raise TimeoutError("Virtiofs statistics did not answer")


signal.signal(signal.SIGALRM, timeout)
signal.alarm(2)
print(json.dumps(read(sys.argv[1])))
