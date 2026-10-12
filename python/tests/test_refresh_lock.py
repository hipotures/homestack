from __future__ import annotations

import unittest
from unittest.mock import patch

from homestack import lifecycle, models, proxmox

from support import test_config


class RefreshLockTests(unittest.TestCase):
    def test_lock_is_exact_tag_and_error_explains_removal(self) -> None:
        with self.assertRaisesRegex(models.AppError, r"(?i)homestack-lock.*remove"):
            proxmox.require_refresh_unlocked(
                200, {"tags": "homestack-ws; homestack-lock "}
            )
        proxmox.require_refresh_unlocked(
            200, {"tags": "homestack-ws;homestack-lock-extra"}
        )

    def test_role_tags_allow_lock_but_reject_missing_gold_or_unknown(self) -> None:
        proxmox.verify_workspace_role_tags(
            200,
            {"tags": f"{models.WORKSPACE_TAG};{models.REFRESH_LOCK_TAG}"},
        )
        for tags in (
            models.REFRESH_LOCK_TAG,
            f"{models.WORKSPACE_TAG};{models.GOLD_TAG}",
            f"{models.WORKSPACE_TAG};unexpected",
        ):
            with self.subTest(tags=tags), self.assertRaises(models.AppError):
                proxmox.verify_workspace_role_tags(200, {"tags": tags})

    def test_build_plan_rejects_lock_before_reading_journal(self) -> None:
        with patch.object(
            lifecycle,
            "cluster_vm_resource",
            return_value={"node": "example-node-1", "status": "stopped"},
        ), patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={"tags": f"{models.WORKSPACE_TAG};{models.REFRESH_LOCK_TAG}"},
        ), patch.object(lifecycle, "_read_refresh_journal") as read_journal:
            with self.assertRaisesRegex(models.AppError, "homestack-lock"):
                lifecycle.build_refresh_plan(object(), test_config(), 200)
        read_journal.assert_not_called()

    def test_direct_recovery_rejects_lock_before_recovering(self) -> None:
        plan = {
            "mode": "recover",
            "vmid": 200,
            "node": "example-node-1",
            "journal": {"node": "example-node-1"},
        }
        with patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={"tags": f"{models.WORKSPACE_TAG};{models.REFRESH_LOCK_TAG}"},
        ), patch.object(lifecycle, "_recover_refresh") as recover:
            with self.assertRaisesRegex(models.AppError, "homestack-lock"):
                lifecycle.refresh_workspace(object(), test_config(), plan, json_mode=True)
        recover.assert_not_called()

    def test_direct_replace_rejects_lock_before_journal_or_import(self) -> None:
        home = "example-storage-a:vm-200-hs-home-user"
        plan = {
            "mode": "replace",
            "vmid": 200,
            "node": "example-node-1",
            "name": "test1",
            "status": "stopped",
            "ip": "192.0.2.200",
            "cidr": 24,
            "gateway": "192.0.2.1",
            "home_label": "HS_HOME_200",
            "home_volume": home,
        }
        current = {
            "status": "stopped",
            "home_volume": home,
            "vm_config": {
                "tags": f"{models.WORKSPACE_TAG};{models.REFRESH_LOCK_TAG}"
            },
        }
        with patch.object(lifecycle, "resolve_existing_workspace", return_value=current), patch.object(
            lifecycle, "_write_refresh_journal"
        ) as write_journal, patch.object(lifecycle, "run_transfer_with_progress") as import_root:
            with self.assertRaisesRegex(models.AppError, "homestack-lock"):
                lifecycle.refresh_workspace(object(), test_config(), plan, json_mode=True)
        write_journal.assert_not_called()
        import_root.assert_not_called()
