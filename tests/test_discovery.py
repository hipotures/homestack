from __future__ import annotations

from contextlib import contextmanager
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from homestack import discovery
from homestack.transports import herdr


class FakeDiscoverySession:
    def __init__(self) -> None:
        self.commands: list[str] = []

    def run_json_value(self, command: str, **_: object) -> object:
        self.commands.append(command)
        responses: dict[str, object] = {
            "pvesh get /version --output-format json": {"version": "9.0"},
            "pvesh get /nodes --output-format json": [
                {"node": "pve1", "status": "online"},
            ],
            "pvesh get /cluster/resources --type vm --output-format json": [
                {
                    "type": "qemu",
                    "vmid": 101,
                    "name": "gold",
                    "node": "pve1",
                    "tags": "homestack-gold",
                },
            ],
            "pvesh get /storage --output-format json": [
                {"storage": "local-lvm", "content": "images"},
            ],
        }
        if command not in responses:
            raise AssertionError(f"unexpected discovery command: {command}")
        return responses[command]

    def execution_info(self) -> dict[str, object]:
        return {"type": "herdr", "host": "pve1", "uid": 0}


class HerdrCandidateDiscoveryTests(unittest.TestCase):
    def test_ssh_target_parser_skips_options(self) -> None:
        self.assertEqual(
            herdr._ssh_target_from_argv(
                ["ssh", "-o", "BatchMode=yes", "-p", "22", "root@pve2"]
            ),
            "root@pve2",
        )

    def test_discovery_keeps_only_single_pane_foreground_ssh_tabs(self) -> None:
        def fake_json(args: list[str], **_: object) -> dict[str, object]:
            if args == ["workspace", "list"]:
                return {
                    "result": {
                        "workspaces": [
                            {"label": "infra", "workspace_id": "ws1"},
                        ]
                    }
                }
            if args == ["tab", "list", "--workspace", "ws1"]:
                return {
                    "result": {
                        "tabs": [
                            {"label": "pve", "tab_id": "tab1"},
                            {"label": "shell", "tab_id": "tab2"},
                            {"label": "split", "tab_id": "tab3"},
                        ]
                    }
                }
            if args == ["pane", "list", "--workspace", "ws1"]:
                return {
                    "result": {
                        "panes": [
                            {"tab_id": "tab1", "pane_id": "pane1"},
                            {"tab_id": "tab2", "pane_id": "pane2"},
                            {"tab_id": "tab3", "pane_id": "pane3a"},
                            {"tab_id": "tab3", "pane_id": "pane3b"},
                        ]
                    }
                }
            if args == ["pane", "process-info", "--pane", "pane1"]:
                return {
                    "result": {
                        "process_info": {
                            "foreground_processes": [
                                {
                                    "name": "ssh",
                                    "argv": ["ssh", "root@pve2"],
                                    "cmdline": "ssh root@pve2",
                                }
                            ]
                        }
                    }
                }
            if args == ["pane", "process-info", "--pane", "pane2"]:
                return {
                    "result": {
                        "process_info": {
                            "foreground_processes": [
                                {"name": "bash", "argv": ["bash"], "cmdline": "bash"}
                            ]
                        }
                    }
                }
            raise AssertionError(f"unexpected Herdr request: {args}")

        def fake_run(command: list[str], **_: object) -> subprocess.CompletedProcess[str]:
            self.assertEqual(command[:3], ["herdr", "pane", "read"])
            return subprocess.CompletedProcess(
                command,
                0,
                "root@pve2:~# ",
                "",
            )

        with patch.object(herdr.shutil, "which", return_value="/usr/bin/herdr"), patch.object(
            herdr, "herdr_json", side_effect=fake_json
        ), patch.object(herdr, "run_local", side_effect=fake_run):
            candidates = herdr.discover_herdr_candidates()

        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].workspace, "infra")
        self.assertEqual(candidates[0].tab, "pve")
        self.assertEqual(candidates[0].ssh_target, "root@pve2")
        self.assertEqual(candidates[0].prompt_host, "pve2")


class EnvironmentDiscoveryTests(unittest.TestCase):
    def _candidate(self) -> herdr.HerdrCandidate:
        return herdr.HerdrCandidate(
            workspace="infra",
            workspace_id="ws1",
            tab="pve",
            tab_id="tab1",
            pane_id="pane1",
            ssh_target="root@pve1",
            ssh_host="pve1",
            ssh_user="root",
            ssh_cmdline="ssh root@pve1",
            prompt_host="pve1",
        )

    def test_discovery_uses_only_read_only_pvesh_queries(self) -> None:
        session = FakeDiscoverySession()

        @contextmanager
        def fake_open(candidate: herdr.HerdrCandidate):
            self.assertEqual(candidate, self._candidate())
            yield session

        with patch.object(discovery.shutil, "which", return_value="/usr/bin/tool"), patch.object(
            discovery, "discover_hardware_identities", return_value=[]
        ), patch.object(
            discovery, "discover_herdr_candidates", return_value=[self._candidate()]
        ), patch.object(discovery, "open_herdr_candidate", side_effect=fake_open):
            report = discovery.discover_environment()

        self.assertTrue(report["ok"])
        self.assertFalse(report["config_used"])
        self.assertTrue(report["read_only"])
        self.assertTrue(session.commands)
        self.assertTrue(all(command.startswith("pvesh get ") for command in session.commands))
        self.assertEqual(report["sessions"][0]["nodes"][0]["node"], "pve1")
        self.assertEqual(report["sessions"][0]["gold_candidates"][0]["vmid"], 101)

    def test_workspace_network_discovery_prefers_existing_workspace_on_gold_bridge(self) -> None:
        resources = [
            {
                'type': 'qemu',
                'vmid': 101,
                'name': 'gold',
                'node': 'pve2',
                'tags': 'homestack-gold',
            },
            {
                'type': 'qemu',
                'vmid': 200,
                'name': 'ws',
                'node': 'pve2',
                'tags': 'homestack-ws',
            },
        ]

        def fake_qm(_session: object, _cfg: object, _node: str, vmid: int):
            if vmid == 101:
                return {'net0': 'virtio=AA:BB:CC:DD:EE:01,bridge=vmbr1'}
            if vmid == 200:
                return {
                    'net0': 'virtio=AA:BB:CC:DD:EE:02,bridge=vmbr1',
                    'ipconfig0': 'ip=192.168.100.200/24,gw=192.168.100.1',
                }
            raise AssertionError(vmid)

        with patch.object(discovery, 'qm_config_on_node', side_effect=fake_qm), patch.object(
            discovery,
            'node_network_inventory',
            return_value=[
                {
                    'iface': 'vmbr0',
                    'type': 'bridge',
                    'address': '192.168.1.11',
                    'netmask': '255.255.255.0',
                    'gateway': '192.168.1.1',
                },
                {
                    'iface': 'vmbr1',
                    'type': 'bridge',
                    'address': '192.168.100.11',
                    'netmask': '255.255.255.0',
                    'gateway': '192.168.100.1',
                },
            ],
        ), patch.object(
            discovery,
            'node_dns_config',
            return_value={'dns1': '192.168.100.1'},
        ):
            candidates = discovery.discover_workspace_networks(
                object(),
                object(),
                resources,
                101,
                'pve2',
            )

        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].cidr, '192.168.100.0/24')
        self.assertEqual(candidates[0].bridge, 'vmbr1')
        self.assertEqual(candidates[0].gateway, '192.168.100.1')
        self.assertEqual(candidates[0].dns_servers, ('192.168.100.1',))
        self.assertEqual(candidates[0].source, 'HomeStack workspace VM 200')

    def test_environment_discovery_reports_progress_without_changing_read_only_queries(self) -> None:
        session = FakeDiscoverySession()
        events: list[tuple[str, int, int]] = []

        @contextmanager
        def fake_open(_candidate: herdr.HerdrCandidate):
            yield session

        with patch.object(discovery.shutil, "which", return_value="/usr/bin/tool"), patch.object(
            discovery, "discover_hardware_identities", return_value=[]
        ), patch.object(
            discovery, "discover_herdr_candidates", return_value=[self._candidate()]
        ), patch.object(discovery, "open_herdr_candidate", side_effect=fake_open):
            report = discovery.discover_environment(
                progress=lambda description, completed, total: events.append(
                    (description, completed, total)
                )
            )

        self.assertTrue(report["ok"])
        self.assertEqual(events[-1], ("Environment discovery complete", 2, 2))
        self.assertTrue(all(command.startswith("pvesh get ") for command in session.commands))

    def test_hardware_identity_discovery_accepts_suffix_after_sk(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            ssh_dir = home / ".ssh"
            ssh_dir.mkdir()
            (ssh_dir / "id_ed25519_sk_yubikey1").write_text("stub")
            (ssh_dir / "id_ecdsa_sk_trezor").write_text("stub")
            (ssh_dir / "id_ed25519_sk_yubikey1.pub").write_text("public")
            (ssh_dir / "id_ed25519").write_text("software")
            with patch.object(discovery.Path, "home", return_value=home):
                found = discovery.discover_hardware_identities()
        self.assertEqual(
            found,
            [
                "~/.ssh/id_ecdsa_sk_trezor",
                "~/.ssh/id_ed25519_sk_yubikey1",
            ],
        )


if __name__ == "__main__":
    unittest.main()
