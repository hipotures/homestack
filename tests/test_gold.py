from __future__ import annotations

import unittest
from unittest.mock import patch

from homestack import gold, models

from support import test_config


class GoldStaticHelpersTests(unittest.TestCase):
    def test_agent_and_cloud_init_detection(self) -> None:
        self.assertTrue(gold._agent_enabled("1"))
        self.assertTrue(gold._agent_enabled("enabled=1,fstrim_cloned_disks=1"))
        self.assertFalse(gold._agent_enabled("0"))
        cfg = {
            "ide2": "local-lvm:cloudinit,media=cdrom",
            "scsi0": "local-lvm:vm-101-disk-0,size=20G",
        }
        self.assertEqual(gold._cloud_init_slots(cfg), ["ide2"])


class GoldReadinessTests(unittest.TestCase):
    def _vm_cfg(self) -> dict[str, str]:
        return {
            "tags": "homestack-gold",
            "scsi0": "local-lvm:vm-101-disk-0,size=20G",
            "net0": "virtio=02:00:00:00:01:01,bridge=vmbr0",
            "ide2": "local-lvm:cloudinit,media=cdrom",
            "agent": "1",
            "boot": "order=scsi0;ide2;net0",
            "sshkeys": "ssh-ed25519%20AAAATEST%20test",
        }

    def test_stopped_gold_can_be_pve_ready_with_configured_public_keys(self) -> None:
        cfg = test_config()
        with patch.object(gold, "qm_config_on_node", return_value=self._vm_cfg()), patch.object(
            gold, "qm_status_on_node", return_value="stopped"
        ):
            result = gold.check_gold_readiness(object(), cfg, "example-node-1", 101)
        self.assertTrue(result.ok)
        self.assertFalse(result.guest_checked)
        self.assertFalse(result.failures)

    def test_extra_home_disk_fails_gold_contract(self) -> None:
        cfg = test_config()
        vm_cfg = {
            **self._vm_cfg(),
            "scsi1": "local-lvm:vm-101-home,size=20G",
        }
        with patch.object(gold, "qm_config_on_node", return_value=vm_cfg), patch.object(
            gold, "qm_status_on_node", return_value="stopped"
        ):
            result = gold.check_gold_readiness(object(), cfg, "example-node-1", 101)
        self.assertFalse(result.ok)
        self.assertTrue(
            any(
                check.name == "Extra data disks" and check.status == "fail"
                for check in result.checks
            )
        )

    def test_running_gold_checks_guest_contract(self) -> None:
        cfg = test_config()

        def guest_value(
            _session: object,
            _cfg: object,
            _node: str,
            _vmid: int,
            command: str,
            **_: object,
        ) -> str:
            if command.startswith("for c in "):
                return ""
            if command.startswith("getent passwd"):
                return "user:x:1000:1000::/home/user:/bin/bash"
            if command.startswith("awk -F:"):
                return "user:1000:1000"
            if "command -v sudo" in command:
                return "ABSENT"
            if command.startswith("test -s /root"):
                return "OK"
            raise AssertionError(command)

        with patch.object(gold, "qm_config_on_node", return_value=self._vm_cfg()), patch.object(
            gold, "qm_status_on_node", return_value="running"
        ), patch.object(
            gold, "node_run", return_value=models.RemoteResult(0, "")
        ), patch.object(
            gold, "guest_out_on_node", side_effect=guest_value
        ):
            result = gold.check_gold_readiness(object(), cfg, "example-node-1", 101)
        self.assertTrue(result.ok)
        self.assertTrue(result.guest_checked)


if __name__ == "__main__":
    unittest.main()
