"""Run HomeStack read functions that have no `--json` command against pve-sim.

    python -m homestack_parity.func_driver --sim-url URL --ca-file FILE \\
        --token-file FILE --record-out FILE --config FILE -- NAME

NAME is one of:

- `discovery`: `discovery.discover_environment`, with the Herdr scan replaced
  by one session on the simulator; prints that session's Proxmox part.
- `networks`: `discovery.discover_workspace_networks` for the configured
  Gold, as the installer calls it.
- `gold-readiness [VMID]`: `gold.check_gold_readiness` for the configured
  Gold, or for VMID.
- `verify-refresh VMID`: `lifecycle._verify_refreshed_guest` for a running
  workspace, with the name from its config, its `prefix.<vmid>` address and
  its home label; prints `ok` and `cloud_id`, or `ok` and the error.
- `workspace-keys [VMID]`: `workspace_ssh.get_workspace_authorized_keys`
  with Gold VMID instead of the configured one; prints `ok`, `keys` and
  `source`, or `ok` and the error.

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

from homestack import discovery, gold, lifecycle, workspace_ssh
from homestack.config import load_config
from homestack.guest import derive_ip
from homestack.models import AppError
from homestack.proxmox import cluster_vm_resources, home_label, qm_config_on_node

from .transports import FixtureTransport, HttpsApi, RecordingTransport


def run_discovery(session: RecordingTransport, cfg: Any, *_: str) -> Any:
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


def _vm_node(session: RecordingTransport, vmid: int) -> str:
    for item in cluster_vm_resources(session):
        if item["vmid"] == vmid:
            return str(item.get("node") or "")
    raise SystemExit(f"VM {vmid} does not exist")


def run_networks(session: RecordingTransport, cfg: Any, *_: str) -> Any:
    resources = cluster_vm_resources(session)
    gold_node = next(str(item.get("node") or "") for item in resources if item["vmid"] == cfg.gold_vmid)
    candidates = discovery.discover_workspace_networks(session, cfg, resources, cfg.gold_vmid, gold_node)
    return [
        {**dataclasses.asdict(candidate), "dns_servers": list(candidate.dns_servers)}
        for candidate in candidates
    ]


def run_gold_readiness(session: RecordingTransport, cfg: Any, *args: str) -> Any:
    vmid = int(args[0]) if args else cfg.gold_vmid
    node = _vm_node(session, vmid)
    readiness = gold.check_gold_readiness(session, cfg, node, vmid)
    return {
        "vmid": readiness.vmid,
        "node": readiness.node,
        "power_state": readiness.power_state,
        "ok": readiness.ok,
        "guest_checked": readiness.guest_checked,
        "checks": [dataclasses.asdict(check) for check in readiness.checks],
    }


def run_verify_refresh(session: RecordingTransport, cfg: Any, vmid_text: str) -> Any:
    vmid = int(vmid_text)
    node = _vm_node(session, vmid)
    name = qm_config_on_node(session, cfg, node, vmid)["name"]
    try:
        cloud_id = lifecycle._verify_refreshed_guest(
            session, cfg, node, vmid, name, derive_ip(cfg, vmid), cfg.network_cidr, home_label(vmid)
        )
    except AppError as exc:
        return {"ok": False, "error": str(exc)}
    return {"ok": True, "cloud_id": cloud_id}


def run_workspace_keys(session: RecordingTransport, cfg: Any, *args: str) -> Any:
    if args:
        cfg = dataclasses.replace(cfg, gold_vmid=int(args[0]))
    try:
        keys, source = workspace_ssh.get_workspace_authorized_keys(session, cfg)
    except AppError as exc:
        return {"ok": False, "error": str(exc)}
    return {"ok": True, "keys": keys, "source": source}


FUNCTIONS = {
    "discovery": run_discovery,
    "networks": run_networks,
    "gold-readiness": run_gold_readiness,
    "verify-refresh": run_verify_refresh,
    "workspace-keys": run_workspace_keys,
}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="func_driver")
    parser.add_argument("--sim-url", required=True)
    parser.add_argument("--ca-file", type=Path, required=True)
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--record-out", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("name", choices=sorted(FUNCTIONS))
    parser.add_argument("args", nargs="*")
    args = parser.parse_args([arg for arg in argv if arg != "--"])

    cfg = load_config(args.config)
    token = args.token_file.read_text(encoding="utf-8").strip()
    session = RecordingTransport(FixtureTransport(cfg, HttpsApi(args.sim_url, args.ca_file, token), args.sim_url))
    try:
        result = FUNCTIONS[args.name](session, cfg, *args.args)
    finally:
        record = {"commands": session.commands, "ops": session.ops}
        args.record_out.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
