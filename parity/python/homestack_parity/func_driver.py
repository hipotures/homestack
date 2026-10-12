"""Run HomeStack read functions that have no `--json` command against pve-sim.

    python -m homestack_parity.func_driver --sim-url URL --ca-file FILE \\
        --token-file FILE --record-out FILE --config FILE -- NAME

NAME is one of:

- `discovery`: `discovery.discover_environment`, with the Herdr scan replaced
  by one session on the simulator; prints that session's Proxmox part.
- `networks`: `discovery.discover_workspace_networks` for the configured
  Gold, as the installer calls it.
- `gold-readiness`: `gold.check_gold_readiness` for the configured Gold.

The functions run unchanged on a RecordingTransport around a
FixtureTransport, like cli_driver. Output is JSON on stdout; the commands
and operations go to --record-out.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import json
import sys
from pathlib import Path
from typing import Any, Iterator
from unittest import mock

from homestack import discovery, gold
from homestack.config import load_config
from homestack.proxmox import cluster_vm_resources

from .transports import FixtureTransport, HttpsApi, RecordingTransport


def run_discovery(session: RecordingTransport, cfg: Any) -> Any:
    candidate = discovery.HerdrCandidate(
        workspace="PVE",
        workspace_id="w1",
        tab="sim",
        tab_id="t1",
        pane_id="p1",
        ssh_target="root@sim",
        ssh_host="sim",
        ssh_user="root",
        ssh_cmdline="ssh root@sim",
        prompt_host="sim",
    )

    @contextlib.contextmanager
    def open_candidate(_candidate: Any) -> Iterator[RecordingTransport]:
        yield session

    with mock.patch.object(discovery.shutil, "which", return_value="/usr/bin/tool"), mock.patch.object(
        discovery, "discover_hardware_identities", return_value=[]
    ), mock.patch.object(discovery, "discover_herdr_candidates", return_value=[candidate]), mock.patch.object(
        discovery, "open_herdr_candidate", side_effect=open_candidate
    ):
        report = discovery.discover_environment()
    entry = report["sessions"][0]
    if not entry["verified"]:
        raise SystemExit(f"discovery failed: {entry['error']}")
    return {key: entry[key] for key in ("proxmox_version", "nodes", "gold_candidates", "storages")}


def _gold_node(session: RecordingTransport, cfg: Any) -> str:
    for item in cluster_vm_resources(session):
        if item["vmid"] == cfg.gold_vmid:
            return str(item.get("node") or "")
    raise SystemExit(f"Gold VM {cfg.gold_vmid} does not exist")


def run_networks(session: RecordingTransport, cfg: Any) -> Any:
    resources = cluster_vm_resources(session)
    gold_node = next(str(item.get("node") or "") for item in resources if item["vmid"] == cfg.gold_vmid)
    candidates = discovery.discover_workspace_networks(session, cfg, resources, cfg.gold_vmid, gold_node)
    return [
        {**dataclasses.asdict(candidate), "dns_servers": list(candidate.dns_servers)}
        for candidate in candidates
    ]


def run_gold_readiness(session: RecordingTransport, cfg: Any) -> Any:
    node = _gold_node(session, cfg)
    readiness = gold.check_gold_readiness(session, cfg, node, cfg.gold_vmid)
    return {
        "vmid": readiness.vmid,
        "node": readiness.node,
        "power_state": readiness.power_state,
        "ok": readiness.ok,
        "guest_checked": readiness.guest_checked,
        "checks": [dataclasses.asdict(check) for check in readiness.checks],
    }


FUNCTIONS = {
    "discovery": run_discovery,
    "networks": run_networks,
    "gold-readiness": run_gold_readiness,
}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="func_driver")
    parser.add_argument("--sim-url", required=True)
    parser.add_argument("--ca-file", type=Path, required=True)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--record-out", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("name", choices=sorted(FUNCTIONS))
    args = parser.parse_args([arg for arg in argv if arg != "--"])

    cfg = load_config(args.config)
    token = args.token_file.read_text(encoding="utf-8").strip()
    session = RecordingTransport(FixtureTransport(cfg, HttpsApi(args.sim_url, args.ca_file, token), args.sim_url))
    try:
        result = FUNCTIONS[args.name](session, cfg)
    finally:
        record = {"commands": session.commands, "ops": session.ops}
        args.record_out.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
