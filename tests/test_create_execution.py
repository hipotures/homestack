from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from support import FakeSession, test_config

from homestack import create_transfer, lifecycle, models


class CreateExecutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.cfg = test_config()
        self.session = FakeSession()

    def _stream_plan(self, **overrides: object) -> dict[str, object]:
        plan: dict[str, object] = {
            "vmid": 200,
            "name": "test1",
            "node": "target-node",
            "source_node": self.cfg.node,
            "transfer_method": "stream",
            "ip": "192.0.2.200",
            "home_label": "HS_HOME_200",
            "home_size_gib": 500,
            "root_storage": "target-storage",
            "home_storage": "target-storage",
            "stale_snippets": [],
            "source_metadata": {
                "cloudinit_slots": ["ide0"],
                "volumes": [
                    {
                        "slot": "ide0",
                        "volume": "source-storage:vm-101-cloudinit",
                    },
                    {
                        "slot": "scsi0",
                        "volume": "source-storage:vm-101-disk-0",
                    },
                ],
            },
        }
        plan.update(overrides)
        return plan

    def test_same_node_default_plan_uses_clone_only(self) -> None:
        gold_cfg = {
            "scsi0": "example-storage-a:vm-101-disk-0,size=16G,backup=1",
            "tags": "homestack-gold",
        }
        cluster_resource = Mock(
            side_effect=[
                {"vmid": self.cfg.gold_vmid, "node": self.cfg.node},
                None,
            ]
        )
        with patch.object(lifecycle, "check_remote_requirements"), patch.object(
            lifecycle, "cluster_vm_resource", cluster_resource
        ), patch.object(lifecycle, "qm_config_on_node", return_value=gold_cfg), patch.object(
            lifecycle, "require_gold_tag"
        ), patch.object(
            lifecycle,
            "resolve_homestack_storage",
            return_value="example-storage-a",
        ), patch.object(
            lifecycle,
            "get_workspace_authorized_keys",
            return_value=("ssh-ed25519 AAAATEST test\n", "test"),
        ), patch.object(lifecycle, "stale_create_snippets", return_value=[]), patch.object(
            lifecycle, "validate_cross_node_create"
        ) as validate_cross_node:
            plan = lifecycle.build_create_plan(
                self.session,
                self.cfg,
                200,
                "test1",
                "20G",
            )

        self.assertEqual(plan["node"], self.cfg.node)
        self.assertEqual(plan["source_node"], self.cfg.node)
        self.assertEqual(plan["transfer_method"], "clone")
        validate_cross_node.assert_not_called()

    def test_stream_success_precedes_target_home_allocation_and_failure_destroys_target(self) -> None:
        plan = self._stream_plan()
        events: list[str] = []

        def stream(*_args: object, **kwargs: object) -> None:
            events.append("stream")
            self.assertEqual(kwargs["source_node"], self.cfg.node)
            self.assertEqual(kwargs["target_node"], "target-node")

        def verify(*_args: object, **_kwargs: object) -> dict[str, str]:
            events.append("verify")
            return {}

        def rename(*_args: object, **_kwargs: object) -> None:
            events.append("rename")

        def revalidate(*_args: object, **_kwargs: object) -> None:
            events.append("revalidate")

        allocate = Mock(side_effect=models.AppError("home allocation sentinel"))
        target_exists = Mock(return_value=True)
        node_run = Mock(return_value=models.RemoteResult(0, ""))
        with patch.object(
            lifecycle,
            "get_workspace_authorized_keys",
            return_value=("ssh-ed25519 AAAATEST test\n", "test"),
        ), patch.object(lifecycle, "cluster_vm_resource", return_value=None), patch.object(
            lifecycle, "_revalidate_stream_source", side_effect=revalidate
        ), patch.object(lifecycle, "stream_gold_restore", side_effect=stream), patch.object(
            lifecycle, "_verify_streamed_restore", side_effect=verify
        ), patch.object(lifecycle, "rename_attached_disk_volume", side_effect=rename), patch.object(
            lifecycle, "allocate_named_raw_volume", allocate
        ), patch.object(lifecycle, "qm_exists_on_node", target_exists), patch.object(
            lifecycle, "node_run", node_run
        ), patch.object(lifecycle, "write_snippets") as write_snippets, self.assertRaisesRegex(
            models.AppError, "home allocation sentinel"
        ):
            lifecycle.create_workspace(self.session, self.cfg, plan, json_mode=True)

        self.assertLess(events.index("revalidate"), events.index("stream"))
        self.assertLess(events.index("stream"), events.index("verify"))
        self.assertLess(events.index("verify"), events.index("rename"))
        allocate.assert_called_once_with(
            self.session,
            self.cfg,
            "target-node",
            "target-storage",
            200,
            "vm-200-hs-home-user",
            "500G",
        )
        self.assertNotIn(self.cfg.node, [allocate.call_args.args[2]])
        target_exists.assert_called_once_with(self.session, self.cfg, "target-node", 200)
        node_run.assert_called_once_with(
            self.session,
            self.cfg,
            "target-node",
            "qm destroy 200 --purge 1",
            check=False,
            timeout=600,
        )
        write_snippets.assert_not_called()

    def test_stream_failure_does_not_allocate_home_boot_or_destroy_unknown_restore(self) -> None:
        plan = self._stream_plan()
        stream = Mock(side_effect=models.AppError("stream failure sentinel"))
        allocate = Mock()
        target_exists = Mock()
        node_run = Mock()
        with patch.object(
            lifecycle,
            "get_workspace_authorized_keys",
            return_value=("ssh-ed25519 AAAATEST test\n", "test"),
        ), patch.object(lifecycle, "cluster_vm_resource", return_value=None), patch.object(
            lifecycle, "_revalidate_stream_source"
        ), patch.object(lifecycle, "stream_gold_restore", stream), patch.object(
            lifecycle, "allocate_named_raw_volume", allocate
        ), patch.object(lifecycle, "qm_exists_on_node", target_exists), patch.object(
            lifecycle, "node_run", node_run
        ), patch.object(lifecycle, "write_snippets") as write_snippets, self.assertRaisesRegex(
            models.AppError,
            r"stream failure sentinel.*ownership is uncertain",
        ):
            lifecycle.create_workspace(self.session, self.cfg, plan, json_mode=True)

        stream.assert_called_once()
        allocate.assert_not_called()
        target_exists.assert_not_called()
        node_run.assert_not_called()
        write_snippets.assert_not_called()

    def test_invalid_restored_disk_ownership_prevents_destroy(self) -> None:
        plan = self._stream_plan()
        restored_cfg = {
            "name": "test1",
            "template": "0",
            "boot": "order=scsi0;net0",
            "scsi0": "target-storage:vm-999-unrelated,size=16G",
            "ide0": "target-storage:vm-200-cloudinit,media=cdrom,size=4M",
        }
        cluster_resource = Mock(
            side_effect=[
                None,
                {"vmid": 200, "node": "target-node", "status": "stopped"},
            ]
        )
        target_config = Mock(return_value=restored_cfg)
        node_run = Mock()
        target_exists = Mock()
        with patch.object(
            lifecycle,
            "get_workspace_authorized_keys",
            return_value=("ssh-ed25519 AAAATEST test\n", "test"),
        ), patch.object(
            lifecycle, "cluster_vm_resource", cluster_resource
        ), patch.object(lifecycle, "_revalidate_stream_source"), patch.object(
            lifecycle, "stream_gold_restore"
        ), patch.object(lifecycle, "qm_status_on_node", return_value="stopped"), patch.object(
            lifecycle, "qm_config_on_node", target_config
        ), patch.object(lifecycle, "qm_exists_on_node", target_exists), patch.object(
            lifecycle, "node_run", node_run
        ), self.assertRaisesRegex(
            models.AppError,
            r"does not have target VM ownership.*cleanup skipped",
        ):
            lifecycle.create_workspace(self.session, self.cfg, plan, json_mode=True)

        target_config.assert_called_with(self.session, self.cfg, "target-node", 200)
        target_exists.assert_not_called()
        node_run.assert_not_called()

    def test_stream_command_preserves_binary_payload_and_pipefail_failures(self) -> None:
        payload = b"\x00Gold archive\n\xffpayload"
        with tempfile.TemporaryDirectory(prefix="homestack-stream-command-") as directory:
            root = Path(directory)
            vzdump_args = root / "vzdump-args.json"
            received_payload = root / "received-payload.bin"
            qm_args = root / "qm-args.json"

            def write_executable(name: str, source: str) -> None:
                path = root / name
                path.write_text(f"#!{sys.executable}\n{source}", encoding="utf-8")
                path.chmod(0o755)

            write_executable(
                "vzdump",
                """
import json
import os
import sys
from pathlib import Path

Path(os.environ["VZDUMP_ARGS"]).write_text(json.dumps(sys.argv[1:]), encoding="utf-8")
sys.stdout.buffer.write(bytes.fromhex(os.environ["VZDUMP_PAYLOAD_HEX"]))
sys.exit(int(os.environ.get("VZDUMP_EXIT", "0")))
""",
            )
            write_executable(
                "ssh",
                """
import subprocess
import sys

result = subprocess.run(["/bin/sh", "-c", sys.argv[-1]])
sys.exit(result.returncode)
""",
            )
            write_executable(
                "qm",
                """
import json
import os
import sys
from pathlib import Path

Path(os.environ["QM_ARGS"]).write_text(json.dumps(sys.argv[1:]), encoding="utf-8")
Path(os.environ["QM_PAYLOAD"]).write_bytes(sys.stdin.buffer.read())
sys.exit(int(os.environ.get("QM_EXIT", "0")))
""",
            )

            command = create_transfer.build_stream_restore_command(
                self.cfg,
                source_node=self.cfg.control_node,
                target_node="target-node",
                vmid=200,
                name="test workspace",
                target_storage="target-storage",
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{root}{os.pathsep}{environment['PATH']}",
                    "VZDUMP_ARGS": str(vzdump_args),
                    "VZDUMP_PAYLOAD_HEX": payload.hex(),
                    "QM_ARGS": str(qm_args),
                    "QM_PAYLOAD": str(received_payload),
                    "VZDUMP_EXIT": "0",
                    "QM_EXIT": "0",
                }
            )

            successful = subprocess.run(
                ["/bin/bash", "-c", command],
                env=environment,
                capture_output=True,
                check=False,
            )
            self.assertEqual(successful.returncode, 0, successful.stderr.decode())
            self.assertEqual(received_payload.read_bytes(), payload)
            source_args = json.loads(vzdump_args.read_text(encoding="utf-8"))
            mailto = source_args.index("--mailto")
            self.assertEqual(source_args[mailto + 1 : mailto + 3], ["", "--notification-mode"])
            target_args = json.loads(qm_args.read_text(encoding="utf-8"))
            self.assertEqual(target_args[target_args.index("--name") + 1], "test workspace")

            environment["VZDUMP_EXIT"] = "7"
            producer_failed = subprocess.run(
                ["/bin/bash", "-c", command],
                env=environment,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(producer_failed.returncode, 0)

            environment["VZDUMP_EXIT"] = "0"
            environment["QM_EXIT"] = "9"
            consumer_failed = subprocess.run(
                ["/bin/bash", "-c", command],
                env=environment,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(consumer_failed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
