"""Prepare private QEMU tools from the host's verified package metadata."""

import fcntl
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import urllib.request


def packages():
    result = subprocess.run(
        ["pacman", "-Sp", "--print-format", "%l %h", "qemu-system-x86", "qemu-img"],
        check=True, capture_output=True, text=True,
    )
    rows = []
    for line in result.stdout.splitlines():
        url, digest = line.split()
        if not url.startswith("https://") or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise RuntimeError("Package metadata must contain HTTPS URLs and SHA-256 checksums")
        rows.append((url, digest))
    if not rows:
        raise RuntimeError("The package manager returned no QEMU packages")
    return rows


def download(url, digest, destination):
    actual = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(url, timeout=60) as response, destination.open("xb") as output:
        if not response.url.startswith("https://"):
            raise RuntimeError("Package download redirected away from HTTPS")
        while chunk := response.read(1024 * 1024):
            size += len(chunk)
            if size > 512 * 1024 * 1024:
                raise RuntimeError("QEMU package exceeds the download size limit")
            actual.update(chunk)
            output.write(chunk)
    if actual.hexdigest() != digest:
        raise RuntimeError("QEMU package SHA-256 does not match the host package database")


def wrappers(directory, root):
    directory.mkdir(mode=0o700)
    for name in ("qemu-system-x86_64", "qemu-img"):
        executable = root / "usr/bin" / name
        arguments = ""
        if name == "qemu-system-x86_64":
            arguments = f" -L {shlex.quote(str(root / 'usr/share/qemu'))}"
        wrapper = directory / name
        wrapper.write_text(
            "#!/bin/sh\n"
            f"export LD_LIBRARY_PATH={shlex.quote(str(root / 'usr/lib'))}${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\n"
            f"export QEMU_MODULE_DIR={shlex.quote(str(root / 'usr/lib/qemu'))}\n"
            f"exec {shlex.quote(str(executable))}{arguments} \"$@\"\n"
        )
        wrapper.chmod(0o700)


def verify(directory):
    for name in ("qemu-system-x86_64", "qemu-img"):
        subprocess.run([str(directory / name), "--version"], check=True,
                       stdout=subprocess.DEVNULL)


def prepare(cache):
    cache.mkdir(parents=True, mode=0o700, exist_ok=True)
    cache.chmod(0o700)
    with (cache / "prepare.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        installed = cache / "qemu"
        if installed.exists():
            verify(installed / "bin")
            return installed / "bin"
        if not shutil.which("pacman"):
            raise RuntimeError(
                "Automatic QEMU provisioning requires pacman on Arch/CachyOS; "
                "on other hosts install qemu-system-x86_64 and qemu-img"
            )
        print("Preparing private QEMU tools in the E2E cache (no system installation)", file=sys.stderr)
        with tempfile.TemporaryDirectory(prefix=".qemu-", dir=cache) as work:
            work = Path(work)
            staging = work / "qemu"
            root = staging / "root"
            root.mkdir(parents=True)
            for index, (url, digest) in enumerate(packages()):
                print(f"Downloading {url.rsplit('/', 1)[-1]}", file=sys.stderr)
                archive = work / f"package-{index}.tar.zst"
                download(url, digest, archive)
                subprocess.run(["tar", "--zstd", "-xf", str(archive), "-C", str(root)], check=True)
                archive.unlink()
            # Firmware packages contain absolute links into /usr/share. Keep
            # those references inside the extracted package tree.
            for link in root.rglob("*"):
                if link.is_symlink():
                    target = Path(os.readlink(link))
                    if target.is_absolute() and (root / target.relative_to("/")).exists():
                        relative = os.path.relpath(root / target.relative_to("/"), link.parent)
                        link.unlink()
                        link.symlink_to(relative)
            wrappers(staging / "bin", root)
            verify(staging / "bin")
            # Wrappers must name the final location, not the temporary directory.
            shutil.rmtree(staging / "bin")
            wrappers(staging / "bin", installed / "root")
            staging.rename(installed)
        verify(installed / "bin")
        return installed / "bin"


if __name__ == "__main__":
    try:
        print(prepare(Path(sys.argv[1]).resolve()))
    except Exception as error:
        sys.exit(f"Cannot prepare E2E host tools: {error}")
