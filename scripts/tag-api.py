#!/usr/bin/env python3
"""Publish the next numeric API release from a clean, already pushed commit."""

import argparse
import re
import subprocess
import sys

VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", re.ASCII)
FIRST_VERSION = (0, 2, 0)


def next_version(tags):
    versions = [tuple(map(int, match.groups())) for tag in tags
                if (match := VERSION.fullmatch(tag))]
    latest = max(versions, default=(0, 0, 0))
    version = FIRST_VERSION if latest < FIRST_VERSION else (*latest[:2], latest[2] + 1)
    return ".".join(map(str, version))


def git(*args):
    return subprocess.run(["git", *args], check=True, text=True, capture_output=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="fetch tags and print the next version without publishing")
    args = parser.parse_args()
    try:
        branch = git("symbolic-ref", "--short", "HEAD")
        remote = git("config", f"branch.{branch}.remote")
        if remote == ".":
            raise ValueError("The branch must track a remote repository")
        if git("status", "--porcelain"):
            raise ValueError("Commit all changes before tagging an API release")
        git("fetch", remote, "--tags")
        if git("rev-parse", "HEAD") != git("rev-parse", "@{upstream}"):
            raise ValueError("Push the branch and resolve any upstream changes before tagging")
        tag = next_version(git("tag", "--list").splitlines())
        if not args.dry_run:
            git("tag", "-a", tag, "-m", f"API release {tag}")
            git("push", remote, f"refs/tags/{tag}")
        print(tag)
        return 0
    except (subprocess.CalledProcessError, ValueError) as error:
        detail = error.stderr.strip() if isinstance(error, subprocess.CalledProcessError) else str(error)
        print(f"tag-api: {detail}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
