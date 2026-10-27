#!/usr/bin/env python3
"""Package the compiled static PVE agent as a native amd64 Debian package."""

import argparse
import gzip
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", args.version):
        parser.error("version must be numeric X.Y.Z")
    binary = args.binary.resolve(strict=True)
    headers = subprocess.run(["readelf", "-l", str(binary)], capture_output=True, text=True, check=True).stdout
    dynamic = subprocess.run(["readelf", "-d", str(binary)], capture_output=True, text=True, check=True).stdout
    if "INTERP" in headers or "(NEEDED)" in dynamic:
        parser.error("binary must be statically linked")
    identity = subprocess.run(["readelf", "-h", str(binary)], capture_output=True, text=True, check=True).stdout
    if "Advanced Micro Devices X86-64" not in identity:
        parser.error("binary must target amd64")
    source = Path(__file__).resolve().parent
    args.output.mkdir(parents=True, exist_ok=True)
    destination = args.output.resolve() / f"homestack-pve-agent_{args.version}_amd64.deb"
    if destination.exists():
        parser.error(f"refusing to overwrite {destination}")
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", subprocess.run(
        ["git", "log", "-1", "--format=%ct"], capture_output=True, text=True, check=True).stdout.strip()))
    with tempfile.TemporaryDirectory(prefix="homestack-deb-") as temporary:
        root = Path(temporary)
        root.chmod(0o755)
        for directory in ["DEBIAN", "usr/sbin", "usr/lib/systemd/system", "etc/homestack", "usr/share/doc/homestack-pve-agent"]:
            (root / directory).mkdir(parents=True, exist_ok=True)
        for directory in root.rglob("*"):
            if directory.is_dir():
                directory.chmod(0o755)
        shutil.copyfile(binary, root / "usr/sbin/homestack-pve-agent")
        (root / "usr/sbin/homestack-pve-agent").chmod(0o755)
        unit = (source / "homestack-pve-agent.service").read_text().replace("/usr/local/sbin/", "/usr/sbin/")
        (root / "usr/lib/systemd/system/homestack-pve-agent.service").write_text(unit)
        # New nodes must select their Collector before exporting anything.
        config = (source / "pve-agent.toml").read_text().replace(
            'otlp_endpoint = "http://otel-1.homestack.app:4318/v1/metrics"', 'otlp_endpoint = ""')
        (root / "etc/homestack/pve-agent.toml").write_text(config)
        (root / "etc/homestack/pve-agent.toml").chmod(0o640)
        doc = root / "usr/share/doc/homestack-pve-agent"
        (doc / "README.md").write_text((source / "README.md").read_text())
        (doc / "copyright").write_text("HomeStack contributors\nLicense: CC0-1.0\nhttps://creativecommons.org/publicdomain/zero/1.0/\n")
        (doc / "changelog.gz").write_bytes(gzip.compress(
            f"homestack-pve-agent ({args.version}) stable; urgency=medium\n\n  * PVE agent release {args.version}.\n".encode(), mtime=0))
        (root / "DEBIAN/control").write_text(
            f"Package: homestack-pve-agent\nVersion: {args.version}\nArchitecture: amd64\n"
            "Maintainer: HomeStack contributors <apt@homestack.app>\nSection: admin\nPriority: optional\n"
            "Depends: init-system-helpers (>= 1.54), systemd\nHomepage: https://github.com/hipotures/homestack\n"
            "Description: Node-side VM and LXC metrics for Proxmox VE\n"
            " Collects cgroup and filesystem metrics and exports OTLP to a local Collector.\n")
        (root / "DEBIAN/conffiles").write_text("/etc/homestack/pve-agent.toml\n")
        for name in ["preinst", "postinst", "prerm", "postrm"]:
            shutil.copyfile(source / "debian" / name, root / "DEBIAN" / name)
            (root / "DEBIAN" / name).chmod(0o755)
        executable = {"usr/sbin/homestack-pve-agent", *(f"DEBIAN/{name}" for name in ["preinst", "postinst", "prerm", "postrm"])}
        for file in root.rglob("*"):
            if file.is_file():
                name = file.relative_to(root).as_posix()
                file.chmod(0o755 if name in executable else 0o640 if name == "etc/homestack/pve-agent.toml" else 0o644)
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(root), str(destination)], check=True, env=env)
    print(destination)


if __name__ == "__main__":
    main()
