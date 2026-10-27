#!/usr/bin/env python3
"""Publish the next PVE agent package release from a clean, pushed HEAD."""

import argparse
import re
import subprocess
import sys


def git(*args):
    return subprocess.run(["git", *args], check=True, text=True, capture_output=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    pattern = re.compile(r"pve-agent-(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", re.ASCII)
    try:
        branch = git("symbolic-ref", "--short", "HEAD")
        remote = git("config", f"branch.{branch}.remote")
        if remote == "." or git("status", "--porcelain"):
            raise ValueError("Commit all changes and use a branch with a remote upstream")
        git("fetch", remote, "--tags")
        if git("rev-parse", "HEAD") != git("rev-parse", "@{upstream}"):
            raise ValueError("Push the current branch before releasing")
        if any(pattern.fullmatch(tag) for tag in git("tag", "--points-at", "HEAD").splitlines()):
            raise ValueError("HEAD already has a PVE agent release")
        versions = [tuple(map(int, match.groups())) for tag in git("tag", "--list").splitlines()
                    if (match := pattern.fullmatch(tag))]
        latest = max(versions) if versions else None
        version = f"{latest[0]}.{latest[1]}.{latest[2] + 1}" if latest else "0.1.0"
        tag = f"pve-agent-{version}"
        if not args.dry_run:
            git("tag", "-a", tag, "-m", f"PVE agent package release {version}")
            git("push", remote, f"refs/tags/{tag}")
        print(tag)
        return 0
    except (subprocess.CalledProcessError, ValueError) as error:
        detail = error.stderr.strip() if isinstance(error, subprocess.CalledProcessError) else str(error)
        print(f"tag-pve-agent: {detail}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
