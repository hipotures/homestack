#!/usr/bin/env python3
"""Download immutable PVE agent releases and prepare a signed Pages artifact."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def main():
    repository = "hipotures/homestack"
    source = Path(__file__).resolve().parent
    output = Path("target/apt-site")
    if output.exists():
        raise SystemExit("target/apt-site already exists; use a fresh publication workspace")
    with tempfile.TemporaryDirectory(prefix="homestack-apt-publish-") as temporary:
        work = Path(temporary)
        home = work / "gnupg"
        home.mkdir(mode=0o700)
        secret = os.environ.pop("APT_SIGNING_KEY", "")
        if not secret:
            raise SystemExit("APT_SIGNING_KEY is not configured")
        subprocess.run(["gpg", "--homedir", str(home), "--batch", "--import"], input=secret.encode(),
                       capture_output=True, check=True)
        del secret
        listing = subprocess.run(["gpg", "--homedir", str(home), "--batch", "--with-colons", "--list-secret-keys"],
                                 check=True, capture_output=True, text=True).stdout
        fingerprint = (source / "signing-key.fingerprint").read_text().strip()
        if not any(line.startswith("fpr:") and line.split(":")[9] == fingerprint for line in listing.splitlines()):
            raise SystemExit("APT signing secret does not match the committed public key")
        raw = subprocess.run(["gh", "api", "--paginate", "--slurp", f"repos/{repository}/releases"],
                             check=True, capture_output=True, text=True).stdout
        packages = work / "packages"
        packages.mkdir()
        for release in (release for page in json.loads(raw) for release in page):
            match = re.fullmatch(r"pve-agent-((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))", release["tag_name"])
            if not match or release["draft"] or release["prerelease"]:
                continue
            name = f"homestack-pve-agent_{match[1]}_amd64.deb"
            assets = [asset for asset in release["assets"] if asset["name"] == name]
            if len(assets) != 1:
                raise SystemExit(f"Release {release['tag_name']} must contain exactly {name}")
            subprocess.run(["gh", "release", "download", release["tag_name"], "--repo", repository,
                            "--pattern", name, "--dir", str(packages)], check=True)
            digest = assets[0].get("digest")
            if digest and digest != "sha256:" + hashlib.sha256((packages / name).read_bytes()).hexdigest():
                raise SystemExit(f"Release asset checksum mismatch: {name}")
        subprocess.run(["python3", str(source / "build-repository.py"), "--packages", str(packages),
                        "--output", str(output / "apt"), "--gnupg-home", str(home)], check=True)
    (output / ".nojekyll").touch()
    (output / "index.html").write_text('<!doctype html><html lang="en"><meta charset="utf-8"><title>HomeStack</title><a href="apt/">HomeStack APT repository</a></html>\n')


if __name__ == "__main__":
    main()
