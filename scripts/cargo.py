#!/usr/bin/env python3
"""Run Cargo or the check suite, then discard disposable build artifacts."""

import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
HASHED_EXECUTABLE = re.compile(r"[A-Za-z0-9_]+-[0-9a-f]{16}")


def cleanup(target):
    """Keep applications, libraries and E2E data; remove relinkable executables."""
    reclaimed = 0
    count = 0
    # Serialize with Cargo builds as well as other invocations of this wrapper.
    # Only host debug output is disposable here; release/cross builds are retained.
    profile = target / "debug"
    if not profile.is_dir() or profile.is_symlink():
        return count, reclaimed
    with (profile / ".cargo-lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for directory in (profile / "deps", profile / "examples"):
            if directory.is_symlink() or not directory.is_dir():
                continue
            for path in directory.iterdir():
                info = path.lstat()
                if (HASHED_EXECUTABLE.fullmatch(path.name)
                        and stat.S_ISREG(info.st_mode) and info.st_mode & 0o111):
                    # A root-level application can be a hard link to this file.
                    # Unlinking this redundant name preserves that application.
                    reclaimed += info.st_blocks * 512 if info.st_nlink == 1 else 0
                    path.unlink()
                    count += 1
        incremental = profile / "incremental"
        if incremental.is_dir() and not incremental.is_symlink():
            for base, _, files in os.walk(incremental, followlinks=False):
                for name in files:
                    info = (Path(base) / name).lstat()
                    reclaimed += info.st_blocks * 512 if info.st_nlink == 1 else 0
            shutil.rmtree(incremental)
    return count, reclaimed


def main():
    arguments = sys.argv[1:]
    if not arguments or arguments[0] not in {"build", "test", "run", "check", "clippy", "--check"}:
        sys.exit("Usage: scripts/cargo.py build|test|run [CARGO ARGUMENTS] | --check [CHECK ARGUMENTS]")
    os.chdir(ROOT)
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        check=True, capture_output=True, text=True,
    )
    target = Path(json.loads(metadata.stdout)["target_directory"])
    cargo_arguments = arguments[:arguments.index("--")] if "--" in arguments else arguments
    for index, argument in enumerate(cargo_arguments):
        if argument.startswith("--target-dir="):
            target = Path(argument.split("=", 1)[1])
        elif argument == "--target-dir" and index + 1 < len(cargo_arguments):
            target = Path(cargo_arguments[index + 1])
    target = target.resolve()
    target.mkdir(parents=True, exist_ok=True)
    with (target / ".homestack-build.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        environment = os.environ.copy()
        environment["HOMESTACK_BUILD_WRAPPER"] = "1"
        command = (["bash", "scripts/check.sh", *arguments[1:]] if arguments[0] == "--check"
                   else ["cargo", *arguments])
        result = subprocess.run(command, env=environment)
        if result.returncode:
            return result.returncode if result.returncode > 0 else 128 - result.returncode
        count, reclaimed = cleanup(target)
        print(f"Build cleanup: removed {count} hashed executables and incremental cache "
              f"({reclaimed / 1024**3:.2f} GiB); applications and libraries retained.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
