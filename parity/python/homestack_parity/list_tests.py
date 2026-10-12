"""Print every Python test as tests/<file>.py::<Class>::<method>, one per line.

    python -m homestack_parity.list_tests [TESTS_DIR]   (run from python/)

Uses the same unittest discovery as the project's test command. A module that
fails to import is an error, not a missing test.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import Iterator


def walk(suite: unittest.TestSuite) -> Iterator[unittest.TestCase]:
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from walk(item)
        else:
            yield item


def manifest_ids(tests_dir: Path) -> list[str]:
    sys.path.insert(0, str(tests_dir))
    suite = unittest.defaultTestLoader.discover(str(tests_dir), pattern="test*.py", top_level_dir=str(tests_dir))
    ids = []
    for case in walk(suite):
        if isinstance(case, unittest.loader._FailedTest):  # noqa: SLF001
            raise SystemExit(f"cannot import {case.id()}")
        module, cls, method = case.id().split(".")
        ids.append(f"{tests_dir.name}/{module}.py::{cls}::{method}")
    return sorted(ids)


if __name__ == "__main__":
    directory = Path(sys.argv[1] if len(sys.argv) > 1 else "tests")
    print("\n".join(manifest_ids(directory)))
