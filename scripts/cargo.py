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
import tempfile

ROOT = Path(__file__).resolve().parent.parent
HASHED_EXECUTABLE = re.compile(r"[A-Za-z0-9_]+-[0-9a-f]{16}")
UNIT_FILE = re.compile(r"(?:lib)?([A-Za-z0-9_]+-[0-9a-f]{16})(?:\..+)?")
JSON_MESSAGES = "--message-format=json-render-diagnostics"


def size(path):
    """Bytes freed by unlinking path; hard-linked files free nothing."""
    info = path.lstat()
    return info.st_blocks * 512 if info.st_nlink == 1 else 0


def prune(profile, used):
    """Remove dependency and build-script units this invocation did not use."""
    deps, build = profile / "deps", profile / "build"
    units, scripts = set(), set()
    for path in used:
        if path.parent == deps and (match := UNIT_FILE.fullmatch(path.name)):
            units.add(match.group(1))
        elif path.is_relative_to(build) and path != build:
            scripts.add(path.relative_to(build).parts[0])
    # A build of another profile or target, or a run that saw no units, leaves this profile alone.
    if not units:
        return 0
    reclaimed = 0
    if deps.is_dir() and not deps.is_symlink():
        for path in deps.iterdir():
            match = UNIT_FILE.fullmatch(path.name)
            if match and match.group(1) not in units and stat.S_ISREG(path.lstat().st_mode):
                reclaimed += size(path)
                path.unlink()
    if build.is_dir() and not build.is_symlink():
        for path in build.iterdir():
            if path.name in scripts or path.is_symlink() or not path.is_dir():
                continue
            for base, _, files in os.walk(path, followlinks=False):
                reclaimed += sum(size(Path(base) / name) for name in files)
            shutil.rmtree(path)
    return reclaimed


def cleanup(target, used=()):
    """Keep applications, libraries and E2E data; remove relinkable executables.

    With the artifacts used by this invocation, also drop every other dependency
    variant: feature unification differs between package selections, so each
    `-p` combination otherwise leaves a full set of libraries behind forever.
    """
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
                    reclaimed += size(path)
                    path.unlink()
                    count += 1
        incremental = profile / "incremental"
        if incremental.is_dir() and not incremental.is_symlink():
            for base, _, files in os.walk(incremental, followlinks=False):
                reclaimed += sum(size(Path(base) / name) for name in files)
            shutil.rmtree(incremental)
        reclaimed += prune(profile, {Path(path) for path in used})
    return count, reclaimed


def record(arguments, log):
    """Run a Cargo build command and append every artifact it used, fresh or rebuilt, to log."""
    split = arguments.index("--") if "--" in arguments else len(arguments)
    command = ["cargo", *arguments[:split], JSON_MESSAGES, *arguments[split:]]
    with subprocess.Popen(command, stdout=subprocess.PIPE, text=True) as process, \
            open(log, "a", encoding="utf-8") as output:
        for line in process.stdout:
            try:
                message = json.loads(line)
            except ValueError:
                sys.stdout.write(line)
                continue
            if message.get("reason") == "compiler-artifact":
                output.writelines(f"{path}\n" for path in message["filenames"])
            elif message.get("reason") == "build-script-executed":
                output.write(f"{message['out_dir']}\n")
    return process.returncode


def run_cargo(arguments, log):
    """Run one Cargo command, recording the artifacts it uses."""
    split = arguments.index("--") if "--" in arguments else len(arguments)
    options = arguments[1:split]
    if arguments[0] in {"build", "check", "clippy"} or (arguments[0] == "test" and "--no-run" in options):
        return record(arguments, log)
    if arguments[0] in {"test", "run"}:
        # Test and program output share stdout with JSON messages, so build first.
        probe = ["test", *options, "--no-run"] if arguments[0] == "test" else ["build", *options]
        if code := record(probe, log):
            return code
    return subprocess.run(["cargo", *arguments]).returncode


def status(code):
    return code if code >= 0 else 128 - code


def main():
    arguments = sys.argv[1:]
    if os.environ.get("HOMESTACK_BUILD_WRAPPER") == "1" and arguments and arguments[0] != "--check":
        # Cargo commands of scripts/check.sh, already inside the wrapper's lock.
        return status(run_cargo(arguments, os.environ["HOMESTACK_ARTIFACT_LOG"]))
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
    with (target / ".homestack-build.lock").open("a+b") as lock, \
            tempfile.NamedTemporaryFile("r", encoding="utf-8", dir=target, prefix=".homestack-used-") as log:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if arguments[0] == "--check":
            environment = os.environ.copy()
            environment["HOMESTACK_BUILD_WRAPPER"] = "1"
            environment["HOMESTACK_ARTIFACT_LOG"] = log.name
            code = subprocess.run(["bash", "scripts/check.sh", *arguments[1:]], env=environment).returncode
        else:
            code = run_cargo(arguments, log.name)
        if code:
            return status(code)
        count, reclaimed = cleanup(target, log.read().splitlines())
        print(f"Build cleanup: removed {count} hashed executables, incremental cache and unused "
              f"dependency variants ({reclaimed / 1024**3:.2f} GiB); applications and libraries "
              f"used by this build retained.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
