from __future__ import annotations

import unittest
from contextlib import ExitStack
from dataclasses import replace
from unittest.mock import patch

from support import FakeSession, test_config

from homestack import create_transfer, lifecycle, models


def gold_config() -> dict[str, str]:
    return {
        "tags": "homestack-gold",
        "scsi0": "source-storage:vm-101-disk-0,size=16G",
        "ide2": "source-storage:vm-101-cloudinit,media=cdrom",
        "net0": "virtio=02:00:00:00:01:01,bridge=vmbr0",
        "boot": "order=scsi0;ide2;net0",
    }


class StreamCommandTests(unittest.TestCase):
    def test_stream_routes_source_and_target_through_control(self) -> None:
        base = test_config()
        cases = (
            ("example-node-1", "example-node-2", 1),
            ("example-node-2", "example-node-1", 1),
            ("example-node-2", "example-node-3", 2),
        )
        for source, target, remote_count in cases:
            cfg = replace(base, control_node="example-node-1")
            command = create_transfer.build_stream_restore_command(
                cfg,
                source_node=source,
                target_node=target,
                vmid=200,
                name="test1",
                target_storage="example-storage-b",
            )
            self.assertIn("bash -o pipefail -c", command)
            self.assertIn("vzdump 101", command)
            self.assertIn("--stdout 1", command)
            self.assertIn("--mode snapshot", command)
            self.assertIn("--compress 0", command)
            self.assertIn("--fleecing enabled=0", command)
            self.assertIn("--script /bin/true", command)
            self.assertIn("--notification-mode legacy-sendmail", command)
            self.assertIn("--prune-backups keep-all=1", command)
            self.assertIn("--remove 0", command)
            self.assertIn("qm create 200 --archive - --storage example-storage-b", command)
            self.assertIn("--unique 1 --start 0 --template 0 --name test1", command)
            self.assertEqual(command.count("ssh -T -o BatchMode=yes"), remote_count)
            self.assertIn("--mailto", command)

    def test_empty_mailto_is_preserved_inside_nested_shell_quotes(self) -> None:
        command = create_transfer.build_stream_restore_command(
            test_config(),
            source_node="example-node-2",
            target_node="example-node-3",
            vmid=200,
            name="test1",
            target_storage="example-storage-b",
        )
        self.assertIn("--mailto", command)
        self.assertRegex(command, r"--mailto[^|]*legacy-sendmail")
        self.assertNotIn("--mailto root", command)

    def test_nonzero_pipeline_is_reported_without_retry(self) -> None:
        class Session(FakeSession):
            def run(self, command: str, **kwargs: object) -> models.RemoteResult:
                self.commands.append(command)
                self.kwargs = kwargs
                return models.RemoteResult(37, "restore failed")

        session = Session()
        with self.assertRaisesRegex(models.AppError, "exit code 37: restore failed"):
            create_transfer.stream_gold_restore(
                session,
                test_config(),
                source_node="example-node-2",
                target_node="example-node-3",
                vmid=200,
                name="test1",
                target_storage="example-storage-b",
            )
        self.assertEqual(len(session.commands), 1)
        self.assertEqual(session.kwargs["timeout"], 7200)
        self.assertFalse(session.kwargs["check"])


class CrossNodeValidationTests(unittest.TestCase):
    def _valid_patches(self):
        return (
            patch.object(
                create_transfer,
                "cluster_node_statuses",
                return_value=[
                    {"node": "example-node-1", "status": "online", "online": True},
                    {"node": "example-node-2", "status": "online", "online": True},
                ],
            ),
            patch.object(create_transfer, "check_remote_requirements"),
            patch.object(create_transfer, "command_exists_on_node", return_value=True),
            patch.object(
                create_transfer,
                "resolve_homestack_storage",
                return_value="example-storage-b",
            ),
            patch.object(
                create_transfer,
                "node_storage_inventory",
                return_value=[
                    {
                        "storage": "example-storage-b",
                        "content": "images",
                        "enabled": 1,
                        "active": 1,
                    },
                    {
                        "storage": "local",
                        "content": "snippets",
                        "enabled": 1,
                        "active": 1,
                    },
                ],
            ),
            patch.object(
                create_transfer,
                "node_network_inventory",
                return_value=[{"iface": "vmbr0", "type": "bridge"}],
            ),
        )

    def test_validates_online_target_and_captures_safe_metadata(self) -> None:
        cfg = test_config()
        patches = self._valid_patches()
        with ExitStack() as stack:
            for item in patches:
                stack.enter_context(item)
            result = create_transfer.validate_cross_node_create(
                FakeSession(),
                cfg,
                source_node="example-node-1",
                target_node="example-node-2",
                gold_vmid=101,
                gold_cfg=gold_config(),
                root_disk="scsi0",
                requested_storage=None,
            )
        self.assertEqual(result["target_storage"], "example-storage-b")
        self.assertEqual(result["source_metadata"]["root_disk"], "scsi0")
        self.assertEqual(result["source_metadata"]["cloudinit_slots"], ["ide2"])
        self.assertNotIn("tags", result["source_metadata"])

    def test_rejects_offline_target_before_storage_mutation(self) -> None:
        cfg = test_config()
        with patch.object(
            create_transfer,
            "cluster_node_statuses",
            return_value=[
                {"node": "example-node-1", "status": "online", "online": True},
                {"node": "example-node-2", "status": "offline", "online": False},
            ],
        ), patch.object(
            create_transfer, "check_remote_requirements"
        ) as requirements, self.assertRaisesRegex(models.AppError, "Target node .*offline"):
            create_transfer.validate_cross_node_create(
                FakeSession(),
                cfg,
                source_node="example-node-1",
                target_node="example-node-2",
                gold_vmid=101,
                gold_cfg=gold_config(),
                root_disk="scsi0",
                requested_storage=None,
            )
        requirements.assert_not_called()

    def test_rejects_extra_disk_and_unbacked_required_devices(self) -> None:
        cfg = test_config()
        patches = self._valid_patches()
        for item in patches:
            item.start()
        self.addCleanup(lambda: [item.stop() for item in patches])
        with self.assertRaisesRegex(models.AppError, "extra data disks"):
            create_transfer.validate_cross_node_create(
                FakeSession(),
                cfg,
                source_node="example-node-1",
                target_node="example-node-2",
                gold_vmid=101,
                gold_cfg={
                    **gold_config(),
                    "scsi1": "source-storage:vm-101-data,size=1G",
                },
                root_disk="scsi0",
                requested_storage=None,
            )

        with self.assertRaisesRegex(models.AppError, "backup=0"):
            create_transfer.validate_cross_node_create(
                FakeSession(),
                cfg,
                source_node="example-node-1",
                target_node="example-node-2",
                gold_vmid=101,
                gold_cfg={
                    **gold_config(),
                    "scsi0": "source-storage:vm-101-disk-0,size=16G,backup=0",
                },
                root_disk="scsi0",
                requested_storage=None,
            )


class CreatePlanTests(unittest.TestCase):
    def _base_patches(self):
        return (
            patch.object(
                lifecycle,
                "cluster_vm_resource",
                side_effect=[
                    {"vmid": 101, "type": "qemu", "node": "example-node-1"},
                    None,
                ],
            ),
            patch.object(lifecycle, "qm_config_on_node", return_value=gold_config()),
            patch.object(lifecycle, "require_gold_tag"),
            patch.object(lifecycle, "get_workspace_authorized_keys", return_value=("key\n", "test")),
            patch.object(lifecycle, "parse_authorized_key_records", return_value=[{"label": "key"}]),
            patch.object(lifecycle, "stale_create_snippets", return_value=[]),
            patch.object(lifecycle, "resolve_homestack_storage", return_value="example-storage-a"),
        )

    def test_default_target_preserves_clone_plan_and_source_metadata(self) -> None:
        patches = self._base_patches()
        with patch.object(lifecycle, "check_remote_requirements"):
            for item in patches:
                item.start()
            self.addCleanup(lambda: [item.stop() for item in patches])
            plan = lifecycle.build_create_plan(
                FakeSession(), test_config(), 200, "test1", "20G"
            )
        self.assertEqual(plan["node"], "example-node-1")
        self.assertEqual(plan["source_node"], "example-node-1")
        self.assertEqual(plan["transfer_method"], "clone")

    def test_target_uses_stream_and_target_storage(self) -> None:
        patches = self._base_patches()
        with patch.object(
            lifecycle,
            "validate_cross_node_create",
            return_value={
                "target_storage": "example-storage-b",
                "source_metadata": {"root_disk": "scsi0"},
            },
        ) as validate:
            for item in patches:
                item.start()
            self.addCleanup(lambda: [item.stop() for item in patches])
            plan = lifecycle.build_create_plan(
                FakeSession(),
                test_config(),
                200,
                "test1",
                "20G",
                node="example-node-2",
            )
        validate.assert_called_once()
        self.assertEqual(plan["node"], "example-node-2")
        self.assertEqual(plan["source_node"], "example-node-1")
        self.assertEqual(plan["transfer_method"], "stream")
        self.assertEqual(plan["root_storage"], "example-storage-b")
        self.assertEqual(plan["home_storage"], "example-storage-b")


class RestoreVerificationTests(unittest.TestCase):
    def test_source_disk_change_requires_a_new_plan(self) -> None:
        cfg = test_config()
        source = gold_config()
        plan = {
            "source_node": cfg.node,
            "source_metadata": create_transfer.capture_gold_metadata(
                cfg.gold_vmid, source, cfg.root_disk
            ),
        }
        changed = {**source, "scsi0": "source-storage:vm-101-disk-1,size=16G"}
        with patch.object(lifecycle, "qm_config_on_node", return_value=changed):
            with self.assertRaisesRegex(models.AppError, "changed after create confirmation"):
                lifecycle._revalidate_stream_source(FakeSession(), cfg, plan)

    def test_unplanned_home_disk_is_rejected_before_allocation(self) -> None:
        restored = {
            "scsi0": "example-storage-b:vm-200-disk-0,cache=none,size=16G",
            "ide2": "example-storage-b:vm-200-cloudinit,media=cdrom",
            "scsi1": "example-storage-b:vm-200-disk-1,size=500G",
        }
        with (
            patch.object(lifecycle, "qm_status_on_node", return_value="stopped"),
            patch.object(lifecycle, "qm_config_on_node", return_value=restored),
            self.assertRaisesRegex(models.AppError, "unexpected scsi1"),
        ):
            lifecycle._verify_streamed_restore(FakeSession(), test_config(), self._plan())

    def _plan(self) -> dict[str, object]:
        return {
            "vmid": 200,
            "node": "example-node-2",
            "name": "test1",
            "root_storage": "example-storage-b",
            "source_metadata": {
                "root_config": (
                    "example-storage-a:vm-101-disk-0,size=16G,"
                    "format=qcow2,cache=none"
                ),
                "cloudinit_slots": ["ide2"],
                "bridges": {"net0": "vmbr0"},
                "volumes": [
                    {"slot": "scsi0", "volume": "example-storage-a:vm-101-disk-0"},
                    {"slot": "ide2", "volume": "example-storage-a:vm-101-cloudinit"},
                ],
            },
        }

    def test_accepts_target_owned_volumes_and_stopped_vm(self) -> None:
        with patch.object(
            lifecycle,
            "qm_status_on_node",
            return_value="stopped",
        ), patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={
                "name": "test1",
                "template": "0",
                "boot": "order=scsi0;ide2;net0",
                "scsi0": "example-storage-b:vm-200-disk-0,cache=none,size=16G",
                "ide2": "example-storage-b:vm-200-cloudinit,media=cdrom",
                "net0": "virtio=02:00:00:00:02:00,bridge=vmbr0",
            },
        ):
            result = lifecycle._verify_streamed_restore(
                FakeSession(), test_config(), self._plan()
            )
        self.assertEqual(result["name"], "test1")

    def test_rejects_source_volume_ownership(self) -> None:
        plan = self._plan()
        plan["root_storage"] = "example-storage-a"
        with patch.object(lifecycle, "qm_status_on_node", return_value="stopped"), patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={
                "name": "test1",
                "boot": "order=scsi0;ide2",
                "scsi0": "example-storage-a:vm-101-disk-0,size=16G,format=qcow2,cache=none",
                "ide2": "example-storage-b:vm-200-cloudinit,media=cdrom",
                "net0": "virtio=02:00:00:00:02:00,bridge=vmbr0",
            },
        ), self.assertRaisesRegex(models.AppError, "source volume"):
            lifecycle._verify_streamed_restore(
                FakeSession(), test_config(), plan
            )


class CreateRestoreFailureTests(unittest.TestCase):
    def test_stream_failure_does_not_allocate_home_or_start(self) -> None:
        cfg = test_config()
        plan = {
            "vmid": 200,
            "name": "test1",
            "node": "example-node-2",
            "source_node": "example-node-1",
            "transfer_method": "stream",
            "ip": "192.0.2.200",
            "home_label": "HS_HOME_200",
            "home_size_gib": 20,
            "root_storage": "example-storage-b",
            "home_storage": "example-storage-b",
            "stale_snippets": [],
            "source_metadata": {"cloudinit_slots": ["ide2"], "volumes": []},
        }
        with patch.object(
            lifecycle,
            "get_workspace_authorized_keys",
            return_value=("ssh-ed25519 AAAATEST test\n", "test"),
        ), patch.object(
            lifecycle,
            "cluster_vm_resource",
            return_value=None,
        ), patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={},
        ), patch.object(
            lifecycle,
            "_revalidate_stream_source",
            return_value=None,
        ), patch.object(
            lifecycle,
            "stream_gold_restore",
            side_effect=models.AppError("pipeline failed"),
        ), patch.object(
            lifecycle,
            "allocate_named_raw_volume",
            side_effect=AssertionError("home must not be allocated"),
        ), patch.object(
            lifecycle,
            "node_run",
            side_effect=AssertionError("target must not be configured"),
        ), self.assertRaisesRegex(models.AppError, "ownership is uncertain"):
            lifecycle.create_workspace(FakeSession(), cfg, plan, json_mode=True)
