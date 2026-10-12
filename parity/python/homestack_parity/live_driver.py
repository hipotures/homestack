"""Run HomeStack functions that have no `--json` command on the real
cluster, through the transport of the given config (read-only).

    python -m homestack_parity.live_driver --config FILE -- NAME [ARGS]

NAME is one of the read functions of `func_driver` (`gold-readiness
[VMID]`, `networks`); the output is the same JSON.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from homestack.config import load_config
from homestack.transports import open_transport

from .func_driver import run_gold_readiness, run_networks

FUNCTIONS = {
    "gold-readiness": run_gold_readiness,
    "networks": run_networks,
}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="live_driver")
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("name", choices=sorted(FUNCTIONS))
    parser.add_argument("args", nargs="*")
    args = parser.parse_args([arg for arg in argv if arg != "--"])
    cfg = load_config(args.config)
    with open_transport(cfg) as session:
        result = FUNCTIONS[args.name](session, cfg, *args.args)
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
