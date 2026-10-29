#!/usr/bin/env python3
"""Publish the next independent service image release from a clean, pushed HEAD."""

import argparse
from pathlib import Path
import re
import subprocess
import sys


def git(*args):
    return subprocess.run(["git", *args], check=True, text=True, capture_output=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service", choices=["pangolin", "otel", "ai-usage", "metrics"], default="pangolin")
    parser.add_argument("--dry-run", action="store_true", help="fetch tags and show the next release without publishing")
    args = parser.parse_args()
    version_pattern = re.compile(rf"{args.service}-(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", re.ASCII)
    try:
        branch = git("symbolic-ref", "--short", "HEAD")
        remote = git("config", f"branch.{branch}.remote")
        if remote == ".":
            raise ValueError("The branch must track a remote repository")
        if git("status", "--porcelain"):
            raise ValueError("Commit all changes before tagging an image release")
        git("fetch", remote, "--tags")
        if git("rev-parse", "HEAD") != git("rev-parse", "@{upstream}"):
            raise ValueError("Push the branch and resolve any upstream changes before tagging")
        tags = git("tag", "--list").splitlines()
        if any(version_pattern.fullmatch(tag) for tag in git("tag", "--points-at", "HEAD").splitlines()):
            raise ValueError(f"HEAD already has a {args.service} image release")
        versions = [tuple(map(int, match.groups())) for tag in tags if (match := version_pattern.fullmatch(tag))]
        if args.service == "pangolin":
            # Preserve the original Pangolin independent release sequence.
            containerfile = Path(__file__).resolve().parent.parent / "deploy/pangolin-site/Containerfile"
            pinned = re.search(r"^ARG PANGOLIN_CLI_VERSION=(\d+)\.(\d+)\.(\d+)$", containerfile.read_text(), re.MULTILINE)
            if pinned is None:
                raise ValueError("Pinned Pangolin CLI version is missing")
            latest = max(versions) if versions else tuple(map(int, pinned.groups()))
            version = f"{latest[0]}.{latest[1]}.{latest[2] + 1}"
        else:
            latest = max(versions) if versions else None
            version = f"{latest[0]}.{latest[1]}.{latest[2] + 1}" if latest else "0.1.0"
        tag = f"{args.service}-{version}"
        if not args.dry_run:
            git("tag", "-a", tag, "-m", f"{args.service} image release {version}")
            git("push", remote, f"refs/tags/{tag}")
        print(tag)
        return 0
    except (subprocess.CalledProcessError, ValueError) as error:
        detail = error.stderr.strip() if isinstance(error, subprocess.CalledProcessError) else str(error)
        print(f"tag-image: {detail}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
