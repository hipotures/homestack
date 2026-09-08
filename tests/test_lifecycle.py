from __future__ import annotations

from contextlib import ExitStack
from dataclasses import replace
from io import StringIO
from pathlib import Path
import unittest
from unittest.mock import patch

from homestack import lifecycle, models, status, ui

from support import FakeSession, test_config


class CreateWorkspaceCompletionTests(unittest.TestCase):
    def run_verified_create(
        self,
        *,
        ssh_config_error: Exception | None = None,
        sync_paths: tuple[str, ...] = (),
    ) -> tuple[dict[str, object], FakeSession]:
        cfg = replace(test_config(), sync_paths=sync_paths)
        plan = {
            'vmid': 200,
            'name': 'test1',
            'ip': '192.0.2.200',
            'home_label': 'HS_HOME_200',
            'home_size_gib': 20,
            'root_storage': 'example-storage-a',
            'home_storage': 'example-storage-a',
            'stale_snippets': [],
        }
        vm_config = {
            'net0': 'virtio=02:00:00:00:02:00,bridge=vmbr0',
            'scsi1': 'example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200',
            'tags': 'homestack-ws',
        }

        def guest_value(_session: object, _vmid: int, command: str, **_: object) -> str:
            if command == 'hostname':
                return 'test1'
            if command.startswith('ip -4'):
                return '2: eth0 inet 192.0.2.200/24'
            if command.startswith('findmnt -n -o SOURCE,FSTYPE,TARGET'):
                return '/dev/sdb ext4 /home/user'
            if 'blkid -s LABEL' in command:
                return 'HS_HOME_200'
            if command.startswith('id '):
                return 'uid=1000(user) gid=1000(user)'
            if command.startswith('test -s '):
                return 'OK'
            if command.startswith('cat '):
                return 'ssh-ed25519 AAAATEST test'
            if 'getent passwd ubuntu' in command:
                return 'ABSENT'
            if 'cloud-id' in command:
                return ''
            raise AssertionError(command)

        session = FakeSession()
        self.last_session = session
        with ExitStack() as stack:
            stack.enter_context(
                patch.object(
                    lifecycle,
                    'get_workspace_authorized_keys',
                    return_value=('ssh-ed25519 AAAATEST test\n', 'test'),
                )
            )
            for name in (
                'clone_full',
                'rename_attached_disk_volume',
                'set_workspace_role_tags',
                'wait_for_qga',
                'guest_exec',
            ):
                stack.enter_context(patch.object(lifecycle, name))
            stack.enter_context(
                patch.object(
                    lifecycle,
                    'allocate_named_raw_volume',
                    return_value='example-storage-a:vm-200-hs-home-user',
                )
            )
            stack.enter_context(patch.object(lifecycle, 'qm_config', return_value=vm_config))
            stack.enter_context(
                patch.object(
                    lifecycle,
                    'write_snippets',
                    return_value={'user': Path('/tmp/user.yaml')},
                )
            )
            stack.enter_context(patch.object(lifecycle, 'guest_out', side_effect=guest_value))
            stack.enter_context(
                patch.object(lifecycle, 'forget_local_ssh_host', return_value=[])
            )
            stack.enter_context(patch.object(lifecycle, 'qm_status', return_value='running'))
            if ssh_config_error is None:
                stack.enter_context(
                    patch.object(
                        lifecycle,
                        'write_local_ssh_config',
                        return_value=Path('/tmp/vm200-test1.conf'),
                    )
                )
            else:
                stack.enter_context(
                    patch.object(
                        lifecycle,
                        'write_local_ssh_config',
                        side_effect=ssh_config_error,
                    )
                )
            result = lifecycle.create_workspace(session, cfg, plan, json_mode=True)
        return result, session

    def test_local_ssh_config_failure_reports_verified_vm_without_rollback(self) -> None:
        with self.assertRaisesRegex(
            models.AppError,
            r'Workspace VM 200 \(test1\) was created and verified successfully, '
            r'but local SSH config generation failed: disk full',
        ):
            self.run_verified_create(ssh_config_error=models.AppError('disk full'))
        self.assertFalse(any(command.startswith('qm destroy ') for command in self.last_session.commands))

    def test_generated_sync_command_uses_installed_cli_name(self) -> None:
        result, _session = self.run_verified_create(
            sync_paths=('~/.config/example/settings.json',)
        )
        self.assertEqual(result['sync_command'], 'homestack sync 200')

class WorkspaceTargetTests(unittest.TestCase):

    def test_numeric_target_is_vmid(self) -> None:

        class Session:
            pass
        self.assertEqual(lifecycle.resolve_workspace_target(Session(), test_config(), '200'), 200)

    def test_workspace_name_resolves_to_tagged_vmid(self) -> None:

        class Session:

            def run_json_value(self, command: str, **_: object):
                if command.startswith('pvesh get /cluster/resources'):
                    return [{'type': 'qemu', 'vmid': 200, 'node': 'example-node-1', 'name': 'test1'}]
                if command.startswith('pvesh get /nodes/example-node-1/qemu/200/config'):
                    return {'name': 'test1', 'tags': 'homestack-ws'}
                raise AssertionError(command)
        self.assertEqual(lifecycle.resolve_workspace_target(Session(), test_config(), 'test1'), 200)

    def test_create_name_cannot_start_with_digit(self) -> None:
        models.validate_name('test1')
        with self.assertRaises(models.AppError):
            models.validate_name('200test')

class MigrationProgressTests(unittest.TestCase):

    def test_tracker_reports_current_disk_copy_progress(self) -> None:
        volumes = [{'slot': 'ide0', 'role': 'cloud-init', 'volume': 'example-storage-b:vm-200-cloudinit', 'name': 'vm-200-cloudinit', 'size_bytes': 4 * 1024 ** 2}, {'slot': 'scsi1', 'role': 'home', 'volume': 'example-storage-b:vm-200-hs-home-user', 'name': 'vm-200-hs-home-user', 'size_bytes': 20 * 1024 ** 3}, {'slot': 'scsi0', 'role': 'root', 'volume': 'example-storage-b:vm-200-hs-root-default', 'name': 'vm-200-hs-root-default', 'size_bytes': 16 * 1024 ** 3}]
        tracker = lifecycle.MigrationProgressTracker(volumes)
        first = tracker.update('successfully imported \'example-storage-b:vm-200-cloudinit\'\nLogical volume "vm-200-hs-home-user" created.\n8711372800 bytes (8.7 GB, 8.1 GiB) copied, 30 s, 290 MB/s\n')
        self.assertIsNotNone(first)
        assert first is not None
        self.assertEqual(first['role'], 'home')
        self.assertEqual(first['copied_bytes'], 8711372800)
        self.assertGreater(float(first['percent']), 40.0)
        self.assertLess(float(first['percent']), 41.0)
        self.assertIn('290 MB/s', str(first['description']))
        second = tracker.update('successfully imported \'example-storage-b:vm-200-hs-home-user\'\nLogical volume "vm-200-hs-root-default" created.\n4294967296 bytes (4.3 GB, 4.0 GiB) copied, 15 s, 286 MB/s\n')
        self.assertIsNotNone(second)
        assert second is not None
        self.assertEqual(second['role'], 'root')
        self.assertGreater(float(second['percent']), 24.0)
        self.assertLess(float(second['percent']), 26.0)

    def test_migration_volume_plan_ignores_unused_refs(self) -> None:
        cfg = test_config()
        vm_cfg = {'ide0': 'example-storage-a:vm-200-cloudinit,media=cdrom,size=4M', 'scsi0': 'example-storage-a:vm-200-hs-root-default,size=16G', 'scsi1': 'example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200,size=20G', 'unused0': 'example-storage-a:vm-200-disk-0'}
        plan = lifecycle.migration_volume_plan(vm_cfg, cfg)
        self.assertEqual([(item['role'], item['name']) for item in plan], [('cloud-init', 'vm-200-cloudinit'), ('root', 'vm-200-hs-root-default'), ('home', 'vm-200-hs-home-user')])
        self.assertEqual(sum((int(item['size_bytes']) for item in plan)), 36 * 1024 ** 3 + 4 * 1024 ** 2)

class RefreshPowerStateTests(unittest.TestCase):

    @staticmethod
    def _plan(status: str) -> dict[str, object]:
        return {'command': 'refresh', 'vmid': 200, 'name': 'test1', 'node': 'example-node-1', 'status': status, 'ip': '192.0.2.200', 'cidr': 24, 'gateway': '192.0.2.1', 'gold_vmid': 101, 'root_storage': 'example-storage-a', 'root_disk': 'scsi0', 'root_disk_gb': 16.0, 'gold_root_disk_config': 'example-storage-a:vm-101-disk-0,size=16G', 'gold_root_volume': 'example-storage-a:vm-101-disk-0', 'root_volume_name': 'vm-200-hs-root-default', 'home_disk': 'scsi1', 'home_storage': 'example-storage-a', 'home_label': 'HS_HOME_200', 'home_volume': 'example-storage-a:vm-200-hs-home-user'}

    @staticmethod
    def _workspace_info(status: str) -> dict[str, object]:
        return {'vmid': 200, 'name': 'test1', 'node': 'example-node-1', 'status': status, 'home_label': 'HS_HOME_200', 'vm_config': {'name': 'test1', 'tags': 'homestack-ws', 'boot': 'order=scsi0;net0', 'net0': 'virtio=BC:24:11:00:00:01,bridge=vmbr0', 'scsi0': 'example-storage-a:vm-200-hs-root-default,size=16G', 'scsi1': 'example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200,size=20G'}}

    @staticmethod
    def _qm_configs() -> list[dict[str, str]]:
        without_root = {'name': 'test1', 'tags': 'homestack-ws', 'boot': 'order=scsi0;net0', 'net0': 'virtio=BC:24:11:00:00:01,bridge=vmbr0', 'scsi1': 'example-storage-a:vm-200-hs-home-user,serial=HS_HOME_200,size=20G'}
        with_root = {**without_root, 'scsi0': 'example-storage-a:vm-200-hs-root-default,size=16G'}
        return [without_root, with_root, with_root]

    def test_stopped_workspace_stays_stopped_after_refresh(self) -> None:
        cfg = test_config()
        commands: list[str] = []

        class Session:

            def run(self, command: str, **_: object):
                commands.append(command)
                return models.RemoteResult(0, '')
        session = Session()
        with patch.object(lifecycle, 'resolve_existing_workspace', return_value=self._workspace_info('stopped')), patch.object(lifecycle, 'shutdown_vm') as shutdown, patch.object(lifecycle, 'qm_config', side_effect=self._qm_configs()), patch.object(lifecycle, 'root_import_spec', return_value='example-storage-a:0,import-from=gold'), patch.object(lifecycle, 'run_transfer_with_progress'), patch.object(lifecycle, 'rename_attached_disk_volume'), patch.object(lifecycle, 'verify_workspace_role_tags'), patch.object(lifecycle, 'write_snippets'), patch.object(lifecycle, 'qm_status', return_value='stopped'), patch.object(lifecycle, 'wait_for_qga') as wait_qga, patch.object(lifecycle, 'guest_out') as guest_out, patch.object(lifecycle, 'guest_exec') as guest_exec, patch.object(lifecycle, 'forget_local_ssh_host', return_value=[]):
            result = lifecycle.refresh_workspace(session, cfg, self._plan('stopped'), json_mode=True)
        shutdown.assert_not_called()
        wait_qga.assert_not_called()
        guest_out.assert_not_called()
        guest_exec.assert_not_called()
        self.assertFalse(any((command == 'qm start 200' for command in commands)))
        self.assertEqual(result['status'], 'stopped')
        self.assertFalse(result['guest_verified'])
        self.assertTrue(result['power_state_preserved'])

    def test_running_workspace_is_restarted_after_refresh(self) -> None:
        cfg = test_config()
        commands: list[str] = []

        class Session:

            def run(self, command: str, **_: object):
                commands.append(command)
                return models.RemoteResult(0, '')
        session = Session()
        guest_values = ['test1', '2: eth0    inet 192.0.2.200/24 brd 192.0.2.255', '/dev/sdb ext4 /home/user', 'HS_HOME_200', 'uid=1000(user) gid=1000(user) groups=1000(user)', 'OK', 'OK', 'ABSENT', 'nocloud']
        with patch.object(lifecycle, 'resolve_existing_workspace', return_value=self._workspace_info('running')), patch.object(lifecycle, 'shutdown_vm') as shutdown, patch.object(lifecycle, 'qm_config', side_effect=self._qm_configs()), patch.object(lifecycle, 'root_import_spec', return_value='example-storage-a:0,import-from=gold'), patch.object(lifecycle, 'run_transfer_with_progress'), patch.object(lifecycle, 'rename_attached_disk_volume'), patch.object(lifecycle, 'verify_workspace_role_tags'), patch.object(lifecycle, 'write_snippets'), patch.object(lifecycle, 'qm_status', return_value='running'), patch.object(lifecycle, 'wait_for_qga') as wait_qga, patch.object(lifecycle, 'guest_out', side_effect=guest_values), patch.object(lifecycle, 'guest_exec'), patch.object(lifecycle, 'forget_local_ssh_host', return_value=[]):
            result = lifecycle.refresh_workspace(session, cfg, self._plan('running'), json_mode=True)
        shutdown.assert_called_once_with(session, 200)
        wait_qga.assert_called_once_with(session, 200, timeout=600)
        self.assertIn('qm start 200', commands)
        self.assertEqual(result['status'], 'running')
        self.assertTrue(result['guest_verified'])
        self.assertTrue(result['power_state_preserved'])

class MigrationPowerStateTests(unittest.TestCase):

    def test_stopped_workspace_stays_stopped_after_migration(self) -> None:
        plan = {'vmid': 200, 'name': 'test1', 'source_node': 'example-node-3', 'target_node': 'example-node-2', 'target_storage': 'example-storage-b', 'status': 'stopped', 'home_label': 'HS_HOME_200', 'volumes': []}
        commands: list[str] = []

        def fake_node_run(_session: object, _cfg: object, _node: str, command: str, **_: object):
            commands.append(command)
            return models.RemoteResult(0, '')
        with patch.object(lifecycle, 'sync_snippets_to_node', return_value=[]), patch.object(lifecycle, 'node_run', side_effect=fake_node_run), patch.object(lifecycle, 'cluster_vm_resource', return_value={'node': 'example-node-2', 'status': 'stopped'}), patch.object(lifecycle, 'qm_config_on_node', return_value={'scsi1': 'example-storage-b:vm-200-hs-home-user,serial=HS_HOME_200,size=20G'}), patch.object(lifecycle, 'qm_status_on_node', return_value='stopped'), patch.object(lifecycle, 'shutdown_vm_on_node') as shutdown:
            result = lifecycle.migrate_workspace(object(), test_config(), plan, json_mode=True)
        shutdown.assert_not_called()
        self.assertFalse(any((command == 'qm start 200' for command in commands)))
        self.assertEqual(result['status'], 'stopped')
        self.assertTrue(result['power_state_preserved'])

    def test_running_workspace_is_started_after_migration(self) -> None:
        plan = {'vmid': 200, 'name': 'test1', 'source_node': 'example-node-3', 'target_node': 'example-node-2', 'target_storage': 'example-storage-b', 'status': 'running', 'home_label': 'HS_HOME_200', 'volumes': []}
        commands: list[str] = []

        def fake_node_run(_session: object, _cfg: object, _node: str, command: str, **_: object):
            commands.append(command)
            return models.RemoteResult(0, '')
        with patch.object(lifecycle, 'sync_snippets_to_node', return_value=[]), patch.object(lifecycle, 'node_run', side_effect=fake_node_run), patch.object(lifecycle, 'cluster_vm_resource', return_value={'node': 'example-node-2', 'status': 'running'}), patch.object(lifecycle, 'qm_config_on_node', return_value={'scsi1': 'example-storage-b:vm-200-hs-home-user,serial=HS_HOME_200,size=20G'}), patch.object(lifecycle, 'qm_status_on_node', return_value='running'), patch.object(lifecycle, 'shutdown_vm_on_node') as shutdown, patch.object(lifecycle, 'wait_for_qga_on_node'), patch.object(lifecycle, 'guest_out_on_node', side_effect=['/dev/sdb ext4 /home/user', 'HS_HOME_200']):
            result = lifecycle.migrate_workspace(object(), test_config(), plan, json_mode=True)
        shutdown.assert_called_once()
        self.assertIn('qm start 200', commands)
        self.assertEqual(result['status'], 'running')
        self.assertTrue(result['power_state_preserved'])

class DestroyPlanPresentationTests(unittest.TestCase):

    def test_gib_value_is_compact(self) -> None:
        self.assertEqual(ui.gib_value(16.0), '16 GiB')
        self.assertEqual(ui.gib_value(20.5), '20.5 GiB')
        self.assertEqual(ui.gib_value(None), '—')

    def test_destroy_removes_local_ssh_artifacts_only_after_vm_is_absent(self) -> None:
        events: list[str] = []
        plan = {
            'vmid': 210,
            'name': 'test210',
            'node': 'example-node-2',
            'ip': '192.0.2.210',
            'home_label': 'HS_HOME_210',
        }

        def verify_absent(*_: object, **__: object):
            events.append('vm absent')
            return None

        def remove_config(_vmid: int):
            events.append('ssh config')
            return [Path('/tmp/vm210-test210.conf')]

        def remove_known_hosts(_host: str):
            events.append('known hosts')
            return ['192.0.2.210']

        with patch.object(lifecycle, 'shutdown_vm_on_node'), patch.object(
            lifecycle,
            'resolve_existing_workspace',
            return_value={'home_label': 'HS_HOME_210'},
        ), patch.object(lifecycle, 'node_run'), patch.object(
            lifecycle, 'cluster_vm_resource', side_effect=verify_absent
        ), patch.object(lifecycle, 'cluster_nodes', return_value=[]), patch.object(
            lifecycle, 'remove_local_ssh_config', side_effect=remove_config
        ), patch.object(
            lifecycle, 'forget_local_ssh_host', side_effect=remove_known_hosts
        ):
            result = lifecycle.destroy_workspace(object(), test_config(), plan)

        self.assertEqual(events, ['vm absent', 'ssh config', 'known hosts'])
        self.assertEqual(result['ssh_config_removed'], ['/tmp/vm210-test210.conf'])
        self.assertEqual(result['ssh_known_hosts_removed'], ['192.0.2.210'])

    def test_destroy_reports_local_cleanup_failure_after_verified_deletion(self) -> None:
        plan = {
            'vmid': 210,
            'name': 'test210',
            'node': 'example-node-2',
            'ip': '192.0.2.210',
            'home_label': 'HS_HOME_210',
        }
        with patch.object(lifecycle, 'shutdown_vm_on_node'), patch.object(
            lifecycle,
            'resolve_existing_workspace',
            return_value={'home_label': 'HS_HOME_210'},
        ), patch.object(lifecycle, 'node_run'), patch.object(
            lifecycle, 'cluster_vm_resource', return_value=None
        ), patch.object(lifecycle, 'cluster_nodes', return_value=[]), patch.object(
            lifecycle,
            'remove_local_ssh_config',
            side_effect=models.AppError('permission denied'),
        ):
            with self.assertRaisesRegex(
                models.AppError,
                'was destroyed successfully, but local SSH cleanup failed: permission denied',
            ):
                lifecycle.destroy_workspace(object(), test_config(), plan)

    def test_destroy_result_shows_local_ssh_config_and_known_hosts_cleanup(self) -> None:
        result = {
            'vmid': 210,
            'name': 'test210',
            'vm_deleted': True,
            'home_deleted': True,
            'ssh_config_removed': [
                Path('/tmp/vm210-test210.conf'),
                Path('/tmp/vm210-oldname.conf'),
            ],
            'ssh_known_hosts_removed': ['192.0.2.210'],
        }
        output = StringIO()
        test_console = ui.Console(file=output, force_terminal=False, width=100)
        with patch.object(ui, 'console', test_console):
            ui.show_destroy_result(result)
        rendered = output.getvalue()
        self.assertIn('Local SSH config', rendered)
        self.assertIn('2 removed', rendered)
        self.assertIn('Local known_hosts', rendered)
        self.assertIn('1 removed', rendered)

class ProgressTests(unittest.TestCase):

    def test_transfer_progress_uses_latest_proxmox_mirror_percentage(self) -> None:

        class FakeProgress:

            def __init__(self) -> None:
                self.updates: list[dict[str, object]] = []
                self.removed: list[int] = []

            def add_task(self, _description: str, *, total: int) -> int:
                self.assert_total = total
                return 7

            def update(self, task_id: int, **kwargs: object) -> None:
                self.updates.append({'task_id': task_id, **kwargs})

            def remove_task(self, task_id: int) -> None:
                self.removed.append(task_id)

        class TransferSession:

            def run_with_progress(self, _command: str, callback, **_: object):
                callback('mirror-scsi0: transferred 3.1 GiB of 16.0 GiB (31.88%) in 13s\nmirror-scsi0: transferred 5.8 GiB of 16.0 GiB (36.14%) in 18s\n')
                return models.RemoteResult(0, '')
        progress = FakeProgress()
        lifecycle.run_transfer_with_progress(TransferSession(), 'qm set 200 --scsi0 ...', progress=progress, description='  ↳ Gold root')
        self.assertEqual(progress.removed, [7])
        self.assertTrue(progress.updates)
        self.assertEqual(progress.updates[-1]['completed'], 36.14)
        self.assertIn('5.8 / 16.0 GiB', str(progress.updates[-1]['description']))
