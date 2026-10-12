"""Run the real HomeStack CLI against pve-sim.

    python -m homestack_parity.cli_driver --sim-url URL --ca-file FILE \\
        --token-file FILE --record-out FILE -- <homestack arguments>

The CLI runs unchanged except that open_transport yields a RecordingTransport
around a FixtureTransport. Its stdout and exit code pass through; the commands
it ran and their operations go to --record-out as JSON.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import sys
from pathlib import Path
from typing import Any, Iterator

from homestack import cli

from .transports import FixtureTransport, HttpsApi, RecordingTransport


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="cli_driver")
    parser.add_argument("--sim-url", required=True)
    parser.add_argument("--ca-file", type=Path, required=True)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--record-out", type=Path, required=True)
    parser.add_argument("homestack_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    homestack_args = args.homestack_args[1:] if args.homestack_args[:1] == ["--"] else args.homestack_args

    token = args.token_file.read_text(encoding="utf-8").strip()
    api = HttpsApi(args.sim_url, args.ca_file, token)
    recordings: list[RecordingTransport] = []

    @contextlib.contextmanager
    def open_fixture_transport(cfg: Any) -> Iterator[RecordingTransport]:
        recording = RecordingTransport(FixtureTransport(cfg, api, args.sim_url))
        recordings.append(recording)
        yield recording

    cli.open_transport = open_fixture_transport  # type: ignore[assignment]
    sys.argv = ["homestack", *homestack_args]
    try:
        code = cli.main()
    finally:
        record = {
            "commands": [command for recording in recordings for command in recording.commands],
            "ops": [op for recording in recordings for op in recording.ops],
        }
        args.record_out.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
