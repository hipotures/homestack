from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

from homestack import lifecycle, models, proxmox, repo, resize, setup, setup_config, setup_guest, status
from support import test_config


ALIEN_TAG = models.ALIEN_WORKSPACE_TAG
NATIVE_TAG = models.WORKSPACE_TAG
TARGET = {
    "vmid": 200,
    "name": "alien-workspace",
    "home": "/home/user",
    "user": "user",
    "alien": True,
}


class RoleTagTests(unittest.TestCase):
    def test_setup_accepts_standalone_alien_but_native_lifecycle_does_not(self) -> None:
        self.assertEqual(ALIEN_TAG, "homestack-alien")
        proxmox.require_setup_workspace_tag(200, {"tags": ALIEN_TAG})
        proxmox.require_setup_workspace_tag(200, {"tags": NATIVE_TAG})

        with self.assertRaisesRegex(models.AppError, "native"):
            proxmox.require_workspace_tag(200, {"tags": ALIEN_TAG})
        with self.assertRaisesRegex(models.AppError, "native"):
            proxmox.require_workspace_tag(200, {"tags": f"{NATIVE_TAG};{ALIEN_TAG}"})
        with self.assertRaisesRegex(models.AppError, "conflicting"):
            proxmox.require_setup_workspace_tag(200, {"tags": f"{NATIVE_TAG};{ALIEN_TAG}"})
        with self.assertRaises(models.AppError):
            proxmox.require_setup_workspace_tag(200, {"tags": models.GOLD_TAG})
        with self.assertRaisesRegex(models.AppError, "template"):
            proxmox.require_setup_workspace_tag(200, {"tags": ALIEN_TAG, "template": "1"})

    def test_name_resolution_discovers_alien_workspace(self) -> None:
        class Session:
            def run_json_value(self, command: str, **_: object):
                if command.startswith("pvesh get /cluster/resources"):
                    return [{"type": "qemu", "vmid": 200, "node": "example-node-1", "name": "alien-workspace"}]
                raise AssertionError(command)

        with patch.object(
            lifecycle,
            "qm_config_on_node",
            return_value={"name": "alien-workspace", "tags": ALIEN_TAG},
        ):
            self.assertEqual(
                lifecycle.resolve_workspace_target(Session(), test_config(), "alien-workspace"),
                200,
            )


class SetupRolePropagationTests(unittest.TestCase):
    def test_repository_and_setup_targets_preserve_alien_role(self) -> None:
        cfg = test_config()
        resource = {"vmid": 200, "type": "qemu", "node": "example-node-1", "status": "running"}
        vm_cfg = {"name": "alien-workspace", "tags": ALIEN_TAG}
        with patch.object(repo, "cluster_vm_resource", return_value=resource), patch.object(
            repo, "qm_config_on_node", return_value=vm_cfg
        ):
            info = repo.repository_workspace_info(object(), cfg, 200)
        self.assertTrue(info["alien"])

        with patch.object(setup, "resolve_workspace_target", return_value=200), patch.object(
            setup.repo, "repository_workspace_info", return_value=info
        ):
            target = setup.resolve_target(object(), cfg, "alien-workspace")
        self.assertTrue(target["alien"])
        self.assertEqual(target["name"], "alien-workspace")

    def test_inspection_and_execution_send_alien_identity_to_guest(self) -> None:
        cfg = test_config()
        calls: list[tuple[str, dict[str, object]]] = []

        def fake_guest(_ws: object, _cfg: object, operation: str, **values: object) -> dict:
            calls.append((operation, values))
            if operation == "state-read":
                return {"registry": {}}
            return {"ok": True}

        with patch.object(setup, "require_tool"), patch.object(setup, "guest", side_effect=fake_guest):
            state = setup.inspect_workspace_state(object(), cfg, TARGET, ())
        self.assertTrue(state["ok"])
        inspect_identity = next(values for operation, values in calls if operation == "identity")
        self.assertTrue(inspect_identity["alien"])

        entry = setup_config.Entry(
            "bash",
            "env",
            "environment",
            "Bash",
            "shell setup",
            setup_config.EnvironmentParams("bash"),
        )
        plan = setup.Plan(TARGET, (entry,))
        workspace = Mock()
        calls.clear()
        with patch.object(setup, "require_tool"), patch.object(setup, "guest", side_effect=fake_guest), patch.object(
            setup, "preflight_entry", return_value={"changed": []}
        ), patch.object(setup, "apply_entry", return_value=("already-ready", "already ready")), patch.object(
            setup, "record_entry_state", return_value={"ok": True}
        ):
            result = setup.execute_plan(cfg, plan, workspace=workspace)
        self.assertTrue(result["ok"], result)
        execute_identity = next(values for operation, values in calls if operation == "identity")
        self.assertTrue(execute_identity["alien"])

    def test_setup_state_record_marks_alien_without_home_label(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            setup_guest.run(
                {
                    "operation": "state-record",
                    "home": tmp,
                    "vmid": 200,
                    "name": "alien-workspace",
                    "id": "bash",
                    "handler": "environment",
                    "paths": [],
                    "snapshot": None,
                    "alien": True,
                }
            )
            registry_path = Path(tmp) / ".local/state/homestack/setup.json"
            registry = json.loads(registry_path.read_text())
        self.assertEqual(registry["workspace"]["kind"], "alien")
        self.assertNotIn("home_label", registry["workspace"])


class GuestIdentityTests(unittest.TestCase):
    def _identity(self, home: Path, *, alien: bool, **updates: object) -> dict[str, object]:
        data: dict[str, object] = {
            "home": str(home),
            "user": "workspace-user",
            "uid": os.getuid(),
            "gid": os.getgid(),
            "name": "alien-workspace",
            "vmid": 200,
            "alien": alien,
        }
        data.update(updates)
        return data

    def test_alien_identity_allows_home_on_root_filesystem(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            data = self._identity(home, alien=True)
            hostname = subprocess.CompletedProcess([], 0, "alien-workspace\n", "")
            with patch.dict(os.environ, {"HOME": str(home)}), patch(
                "pwd.getpwuid",
                return_value=SimpleNamespace(pw_name="workspace-user", pw_dir=str(home)),
            ), patch.object(setup_guest.subprocess, "run", return_value=hostname) as run:
                setup_guest.verify_identity(data)
            run.assert_called_once_with(
                ["hostname", "-s"], capture_output=True, text=True, check=True
            )

    def test_alien_identity_still_rejects_account_and_hostname_mismatches(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            data = self._identity(home, alien=True, uid=os.getuid() + 1)
            with patch.dict(os.environ, {"HOME": str(home)}), self.assertRaisesRegex(
                setup_guest.GuestError, "UID/GID"
            ):
                setup_guest.verify_identity(data)

            data = self._identity(home, alien=True)
            with patch.dict(os.environ, {"HOME": str(home)}), patch(
                "pwd.getpwuid",
                return_value=SimpleNamespace(pw_name="workspace-user", pw_dir=str(home)),
            ), patch.object(
                setup_guest.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, "wrong-name\n", ""),
            ), self.assertRaisesRegex(setup_guest.GuestError, "hostname"):
                setup_guest.verify_identity(data)

    def test_native_identity_still_rejects_root_filesystem_mount(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            data = self._identity(home, alien=False)
            outputs = [
                subprocess.CompletedProcess([], 0, "alien-workspace\n", ""),
                subprocess.CompletedProcess([], 0, "/ ext4\n", ""),
            ]
            with patch.dict(os.environ, {"HOME": str(home)}), patch(
                "pwd.getpwuid",
                return_value=SimpleNamespace(pw_name="workspace-user", pw_dir=str(home)),
            ), patch.object(setup_guest.subprocess, "run", side_effect=outputs):
                with self.assertRaisesRegex(setup_guest.GuestError, "mount"):
                    setup_guest.verify_identity(data)


class LifecycleAlienGuardsTests(unittest.TestCase):
    @staticmethod
    def _alien_session() -> object:
        class Session:
            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command.startswith("pvesh get /cluster/resources"):
                    return [{"type": "qemu", "vmid": 200, "node": "example-node-1", "status": "running"}]
                if "qemu/200/config" in command:
                    return {
                        "name": "alien-workspace",
                        "tags": ALIEN_TAG,
                        "scsi0": "example-storage-a:vm-200-root,size=16G",
                        "scsi1": "example-storage-a:vm-200-home,size=20G",
                    }
                raise AssertionError(command)

            def run(self, command: str, **_: object):
                self.commands.append(command)
                raise AssertionError(f"unexpected remote mutation: {command}")

        return Session()

    def test_resize_destroy_and_migrate_plans_reject_alien_before_guest_or_disk_work(self) -> None:
        cfg = test_config()
        for operation in ("resize", "destroy", "migrate"):
            with self.subTest(operation=operation):
                session = self._alien_session()
                if operation == "resize":
                    with self.assertRaisesRegex(models.AppError, "forbidden workspace role tag"):
                        resize.build_resize_plan(session, cfg, 200, root_size="32G")
                elif operation == "destroy":
                    with self.assertRaisesRegex(models.AppError, "forbidden workspace role tag"):
                        lifecycle.build_destroy_plan(session, cfg, 200)
                else:
                    with self.assertRaisesRegex(models.AppError, "forbidden workspace role tag"):
                        lifecycle.build_migrate_plan(session, cfg, 200, "example-node-2", "example-storage-b")
                self.assertEqual(
                    [command for command in session.commands if command.startswith("qm ")],
                    [],
                )

    def test_destroy_execution_rejects_alien_before_shutdown(self) -> None:
        session = self._alien_session()
        with patch.object(lifecycle, "shutdown_vm_on_node") as shutdown:
            with self.assertRaisesRegex(models.AppError, "forbidden workspace role tag"):
                lifecycle.destroy_workspace(
                    session,
                    test_config(),
                    {"vmid": 200, "node": "example-node-1", "name": "alien-workspace", "home_label": "HS_HOME_200"},
                )
        shutdown.assert_not_called()

    def test_migrate_execution_rejects_alien_before_snippets_or_shutdown(self) -> None:
        session = self._alien_session()
        with patch.object(lifecycle, "sync_snippets_to_node") as snippets, patch.object(
            lifecycle, "shutdown_vm_on_node"
        ) as shutdown:
            with self.assertRaisesRegex(models.AppError, "forbidden workspace role tag"):
                lifecycle.migrate_workspace(
                    session,
                    test_config(),
                    {
                        "vmid": 200,
                        "source_node": "example-node-1",
                        "target_node": "example-node-2",
                        "target_storage": "example-storage-b",
                        "status": "running",
                        "name": "alien-workspace",
                    },
                    json_mode=True,
                )
        snippets.assert_not_called()
        shutdown.assert_not_called()

    def test_refresh_recovery_rejects_alien_before_recovery(self) -> None:
        session = self._alien_session()
        with patch.object(lifecycle, "_recover_refresh") as recover:
            with self.assertRaisesRegex(models.AppError, "alien workspace role tag"):
                lifecycle.refresh_workspace(
                    session,
                    test_config(),
                    {
                        "mode": "recover",
                        "vmid": 200,
                        "node": "example-node-1",
                        "journal": {"node": "example-node-1"},
                    },
                    json_mode=True,
                )
        recover.assert_not_called()

    def test_refresh_plan_rejects_alien_before_reading_recovery_journal(self) -> None:
        with patch.object(lifecycle, "_read_refresh_journal") as journal:
            with self.assertRaisesRegex(models.AppError, "alien workspace role tag"):
                lifecycle.build_refresh_plan(self._alien_session(), test_config(), 200)
        journal.assert_not_called()


class StatusAlienTests(unittest.TestCase):
    def test_global_status_counts_only_native_homes_and_skips_alien_disk_warnings(self) -> None:
        cfg = test_config()
        resources = [
            {"vmid": 101, "type": "qemu", "node": "example-node-1", "status": "stopped", "name": "gold"},
            {"vmid": 200, "type": "qemu", "node": "example-node-1", "status": "running", "name": "native", "tags": NATIVE_TAG},
            {"vmid": 201, "type": "qemu", "node": "example-node-1", "status": "running", "name": "alien", "tags": ALIEN_TAG},
        ]
        configs = {
            101: {"name": "gold", "tags": models.GOLD_TAG, "scsi0": "example-storage-a:vm-101-root,size=16G"},
            200: {
                "name": "native",
                "tags": NATIVE_TAG,
                "scsi0": "example-storage-a:vm-200-root,size=16G",
                "scsi1": "example-storage-a:vm-200-home,serial=HS_HOME_200,size=20G",
            },
            201: {
                "name": "alien",
                "tags": ALIEN_TAG,
                "scsi0": "example-storage-a:vm-201-root,size=16G",
                "scsi1": "example-storage-a:vm-201-home,size=20G",
            },
        }

        class Session:
            def run_json_value(self, command: str, **_: object):
                if command.startswith("pvesh get /cluster/resources"):
                    return resources
                raise AssertionError(command)

            def execution_info(self) -> dict[str, str]:
                return {"type": "fake"}

        with patch.object(status, "cluster_node_statuses", return_value=[{"node": "example-node-1", "status": "online", "online": True}]), patch.object(
            status, "qm_config_on_node", side_effect=lambda _s, _c, _n, vmid: configs[vmid]
        ), patch.object(
            status,
            "workspace_home_usage",
            return_value={"size_bytes": 100, "used_bytes": 40, "free_bytes": 60},
        ) as usage, patch.object(
            status, "orphaned_homestack_volumes", return_value=([], [], True)
        ), patch.object(status, "homestack_storage_capacities", return_value=([], [])):
            result = status.global_status(Session(), cfg)

        self.assertEqual([item["role"] for item in result["workspaces"]], ["WS", "ALIEN"])
        alien = result["workspaces"][1]
        self.assertIsNone(alien["home"])
        self.assertEqual(result["summary"]["homes"]["quota_bytes"], 100)
        self.assertEqual(result["summary"]["homes"]["used_bytes"], 40)
        self.assertEqual(result["summary"]["homes"]["free_bytes"], 60)
        self.assertEqual(result["summary"]["homes"]["free_percent"], 60.0)
        self.assertEqual(result["summary"]["homes"]["missing_count"], 0)
        self.assertEqual(usage.call_count, 1)
        self.assertFalse(any("scsi1" in warning for warning in result["warnings"]))

    def test_per_vm_alien_status_has_no_home_identity_or_home_volume_role(self) -> None:
        cfg = test_config()
        vm_cfg = {
            "name": "alien",
            "tags": ALIEN_TAG,
            "scsi0": "example-storage-a:vm-201-root,size=16G",
            "scsi1": "example-storage-a:vm-201-home,size=20G",
        }
        with patch.object(
            status, "cluster_vm_resource", return_value={"node": "example-node-1", "status": "running"}
        ), patch.object(status, "qm_config_on_node", return_value=vm_cfg), patch.object(
            status, "node_run", return_value=models.RemoteResult(1, "")
        ):
            session = Mock()
            session.execution_info.return_value = {"type": "fake"}
            result = status.workspace_status(session, cfg, 201)

        self.assertEqual(result["role"], "ALIEN")
        self.assertIsNone(result["home_label"])
        self.assertIsNone(result["home_identity_ok"])
        self.assertIsNone(result["home_mount_ok"])
        self.assertIsNone(result["home_disk"])
        self.assertTrue(all(item["role"] == "disk" for item in result["volumes"]))


if __name__ == "__main__":
    unittest.main()
