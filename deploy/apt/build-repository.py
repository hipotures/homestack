#!/usr/bin/env python3
"""Build and sign a stable/main amd64 APT archive from released .deb files."""

import argparse
from datetime import datetime, timezone
from email.utils import format_datetime
import gzip
import hashlib
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--gnupg-home", required=True, type=Path)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    fingerprint = (source / "signing-key.fingerprint").read_text().strip()
    output = args.output.resolve()
    if output.exists() and any(output.iterdir()):
        parser.error("output must be empty")
    pool = output / "pool/main/h/homestack-pve-agent"
    pool.mkdir(parents=True)
    packages = sorted(args.packages.glob("*.deb"))
    if not packages:
        parser.error("no Debian packages found")
    versions = set()
    for package in packages:
        fields = subprocess.run(["dpkg-deb", "--field", str(package), "Package", "Version", "Architecture"],
                                check=True, capture_output=True, text=True).stdout
        values = dict(line.split(": ", 1) for line in fields.splitlines())
        version = values.get("Version")
        if values.get("Package") != "homestack-pve-agent" or values.get("Architecture") != "amd64" or version in versions:
            parser.error(f"unexpected or duplicate package: {package.name}")
        versions.add(version)
        expected = f"homestack-pve-agent_{version}_amd64.deb"
        if package.name != expected:
            parser.error(f"unexpected package filename: {package.name}")
        shutil.copyfile(package, pool / package.name)
    index = subprocess.run(["dpkg-scanpackages", "--multiversion", "pool", "/dev/null"],
                           cwd=output, check=True, capture_output=True).stdout
    distribution = output / "dists/stable"
    binary = distribution / "main/binary-amd64"
    binary.mkdir(parents=True)
    files = {"main/binary-amd64/Packages": index, "main/binary-amd64/Packages.gz": gzip.compress(index, mtime=0)}
    for name, data in files.items():
        (distribution / name).write_bytes(data)
    release = "\n".join([
        "Origin: HomeStack", "Label: HomeStack", "Suite: stable", "Codename: stable",
        "Architectures: amd64", "Components: main", "Description: HomeStack PVE agent packages",
        f"Date: {format_datetime(datetime.now(timezone.utc), usegmt=True)}", "SHA256:",
        *[f" {hashlib.sha256(data).hexdigest()} {len(data):16d} {name}" for name, data in sorted(files.items())], "",
    ])
    (distribution / "Release").write_text(release)
    gpg = ["gpg", "--homedir", str(args.gnupg_home), "--batch", "--yes", "--local-user", fingerprint]
    subprocess.run([*gpg, "--digest-algo", "SHA256", "--clearsign", "--output", str(distribution / "InRelease"),
                    str(distribution / "Release")], check=True)
    subprocess.run([*gpg, "--digest-algo", "SHA256", "--armor", "--detach-sign", "--output", str(distribution / "Release.gpg"),
                    str(distribution / "Release")], check=True)
    shutil.copyfile(source / "homestack-archive-keyring.gpg", output / "homestack-archive-keyring.gpg")
    shutil.copyfile(source / "signing-key.fingerprint", output / "signing-key.fingerprint")
    shutil.copyfile(source / "setup.sh", output / "setup.sh")
    (output / "index.html").write_text(
        '<!doctype html><html lang="en"><meta charset="utf-8"><title>HomeStack APT</title>'
        '<h1>HomeStack APT repository</h1><p>Suite: stable. Component: main. Architecture: amd64.</p>'
        '<p><a href="https://github.com/hipotures/homestack/blob/main/deploy/apt/README.md">Installation instructions</a></p>'
        f'<p>Signing key fingerprint: <code>{fingerprint}</code></p></html>\n')
    print(f"Signed APT archive: {output} ({len(packages)} versions)")


if __name__ == "__main__":
    main()
