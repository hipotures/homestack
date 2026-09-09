from __future__ import annotations

from io import StringIO
import unittest
from unittest.mock import patch

from homestack import models, proxmox, status, ui

from support import test_config

class MigrationPlanDisplayTests(unittest.TestCase):

    def test_format_volume_lines_accepts_migration_size_bytes(self) -> None:
        lines = ui.format_volume_lines([{'slot': 'scsi0', 'role': 'root', 'volume': 'example-storage-b:vm-200-hs-root-default', 'size_bytes': 16 * 1024 ** 3}, {'slot': 'scsi1', 'role': 'home', 'volume': 'example-storage-b:vm-200-hs-home-user', 'size_bytes': 20 * 1024 ** 3}])
        self.assertEqual(len(lines), 3)
        self.assertIn('16.0 GiB', lines[1])
        self.assertIn('20.0 GiB', lines[2])

class VmVolumeInventoryTests(unittest.TestCase):

    def test_lists_root_home_cloudinit_and_unused_volume_refs(self) -> None:
        cfg = test_config()
        vm_cfg = {'ide0': 'example-storage-a:vm-200-cloudinit,media=cdrom,size=4M', 'ide2': 'none,media=cdrom', 'scsi0': 'example-storage-a:vm-200-hs-root-default,iothread=1,size=16G', 'scsi1': 'example-storage-a:vm-200-hs-home-user,discard=on,iothread=1,serial=HS_HOME_200,size=20G,ssd=1', 'unused0': 'example-storage-a:vm-200-disk-0', 'name': 'test1'}
        self.assertEqual(status.vm_volume_inventory(vm_cfg, cfg), [{'slot': 'ide0', 'role': 'cloud-init', 'volume': 'example-storage-a:vm-200-cloudinit', 'storage': 'example-storage-a', 'size': '4M'}, {'slot': 'scsi0', 'role': 'root', 'volume': 'example-storage-a:vm-200-hs-root-default', 'storage': 'example-storage-a', 'size': '16G'}, {'slot': 'scsi1', 'role': 'home', 'volume': 'example-storage-a:vm-200-hs-home-user', 'storage': 'example-storage-a', 'size': '20G'}, {'slot': 'unused0', 'role': 'unused', 'volume': 'example-storage-a:vm-200-disk-0', 'storage': 'example-storage-a', 'size': None}])

    def test_workspace_rejects_root_and_home_pointing_to_same_volume(self) -> None:
        shared = 'example-storage-a:vm-200-hs-home-user'
        vm_cfg = {
            'name': 'test1',
            'tags': 'homestack-ws',
            'scsi0': f'{shared},size=20G',
            'scsi1': f'{shared},serial=HS_HOME_200,size=20G',
        }
        with patch.object(
            status,
            'cluster_vm_resource',
            return_value={'node': 'example-node-1', 'status': 'stopped'},
        ), patch.object(
            status, 'qm_config_on_node', return_value=vm_cfg
        ), patch.object(
            status, 'workspace_home_usage'
        ) as usage:
            with self.assertRaisesRegex(models.AppError, 'same volume'):
                status.resolve_existing_workspace(
                    object(), test_config(), 200, require_network=False
                )
        usage.assert_not_called()

class VolumeRenameCleanupTests(unittest.TestCase):

    def test_cleanup_removes_only_missing_renamed_source_refs(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.deleted: set[str] = set()
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                if command == 'pvesh get /nodes/example-node-1/qemu/200/config --output-format json':
                    cfg = {'scsi0': 'example-storage-a:vm-200-hs-root-default,iothread=1,size=16G', 'unused0': 'example-storage-a:vm-200-disk-0', 'unused1': 'example-storage-a:vm-200-old-data'}
                    return {key: value for key, value in cfg.items() if key not in self.deleted}
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return [{'volid': 'example-storage-a:vm-200-hs-root-default'}, {'volid': 'example-storage-a:vm-200-old-data'}]
                raise AssertionError(command)

            def run(self, command: str, **_: object):
                self.commands.append(command)
                match = __import__('re').fullmatch('qm set 200 --delete (unused[0-9]+)', command)
                if match is None:
                    raise AssertionError(command)
                self.deleted.add(match.group(1))
                return models.RemoteResult(0, '')
        session = Session()
        removed = proxmox.cleanup_renamed_volume_unused_refs(
            session,
            test_config(),
            'example-node-1',
            200,
            'example-storage-a:vm-200-disk-0',
        )
        self.assertEqual(removed, ['unused0'])
        self.assertEqual(session.commands, ['qm set 200 --delete unused0'])
        self.assertNotIn('unused1', session.deleted)

    def test_cleanup_refuses_when_old_source_volume_still_exists(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                if command == 'pvesh get /nodes/example-node-1/qemu/200/config --output-format json':
                    return {'scsi0': 'example-storage-a:vm-200-hs-root-default,iothread=1,size=16G', 'unused0': 'example-storage-a:vm-200-disk-0'}
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return [{'volid': 'example-storage-a:vm-200-disk-0'}]
                raise AssertionError(command)

            def run(self, command: str, **_: object):
                self.commands.append(command)
                return models.RemoteResult(0, '')
        session = Session()
        with self.assertRaisesRegex(models.AppError, 'source volume still exists'):
            proxmox.cleanup_renamed_volume_unused_refs(
                session,
                test_config(),
                'example-node-1',
                200,
                'example-storage-a:vm-200-disk-0',
            )
        self.assertEqual(session.commands, [])

class OrphanedVolumeTests(unittest.TestCase):

    def test_homestack_volume_identity(self) -> None:
        self.assertEqual(proxmox.homestack_volume_identity('example-storage-a:vm-200-hs-home-user'), {'volid': 'example-storage-a:vm-200-hs-home-user', 'vmid': 200, 'role': 'home', 'name': 'user'})
        self.assertEqual(proxmox.homestack_volume_identity('local:200/vm-200-hs-root-default.raw'), {'volid': 'local:200/vm-200-hs-root-default.raw', 'vmid': 200, 'role': 'root', 'name': 'default'})
        self.assertIsNone(proxmox.homestack_volume_identity('example-storage-a:vm-200-disk-1'))
        self.assertIsNone(proxmox.homestack_volume_identity('example-storage-a:vm-200-home-user'))

    def test_attached_disk_volumes_reads_vm_disks_only(self) -> None:
        cfg = {'scsi0': 'example-storage-a:vm-200-hs-root-default,iothread=1,size=16G', 'scsi1': 'example-storage-a:vm-200-hs-home-user,size=20G', 'cicustom': 'user=local:snippets/test.yaml'}
        self.assertEqual(proxmox.attached_disk_volumes(cfg), {'example-storage-a:vm-200-hs-root-default', 'example-storage-a:vm-200-hs-home-user'})

    def test_storage_layout_maps_node_to_assigned_pve_storage_ids(self) -> None:
        cfg = test_config()
        self.assertEqual(proxmox.homestack_storage_layout_name('example-node-1'), 'homestack-storage-1')
        self.assertEqual(proxmox.homestack_storage_ids_for_node(cfg, 'example-node-1'), ('example-storage-a',))
        self.assertEqual(proxmox.homestack_storage_ids_for_node(cfg, 'example-node-2'), ('example-storage-b',))
        self.assertEqual(proxmox.homestack_storage_ids_for_node(cfg, 'other'), ())

    def test_orphan_scan_reports_only_unattached_managed_volumes(self) -> None:

        class Session:

            def run_json_value(self, command: str, **_: object):
                if command == 'pvesh get /nodes --output-format json':
                    return [{'node': 'example-node-1'}]
                if command == 'pvesh get /nodes/example-node-1/storage --output-format json':
                    return [{'storage': 'example-storage-a', 'content': 'images', 'enabled': 1, 'active': 1}, {'storage': 'example-storage-b', 'content': 'images', 'enabled': 1, 'active': 1}, {'storage': 'local', 'content': 'iso,vztmpl', 'enabled': 1, 'active': 1}]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return [{'volid': 'example-storage-a:vm-200-hs-root-default', 'size': 16}, {'volid': 'example-storage-a:vm-200-hs-home-user', 'size': 20}, {'volid': 'example-storage-a:vm-201-hs-home-user', 'size': 20}, {'volid': 'example-storage-a:vm-201-disk-0', 'size': 16}]
                raise AssertionError(command)
        progress_updates: list[tuple[str, float]] = []
        orphaned, warnings, complete = proxmox.orphaned_homestack_volumes(Session(), test_config(), {'example-storage-a:vm-200-hs-root-default', 'example-storage-a:vm-200-hs-home-user'}, progress=lambda description, fraction: progress_updates.append((description, fraction)))
        self.assertEqual(warnings, [])
        self.assertTrue(complete)
        self.assertEqual([item['volid'] for item in orphaned], ['example-storage-a:vm-201-hs-home-user'])
        self.assertTrue(progress_updates)
        self.assertIn('Scan example-storage-a on example-node-1', [description for description, _ in progress_updates])
        self.assertEqual(progress_updates[-1], ('Detached-volume scan complete', 1.0))

    def test_orphan_scan_ignores_unassigned_image_storages(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command == 'pvesh get /nodes --output-format json':
                    return [{'node': 'example-node-1'}]
                if command == 'pvesh get /nodes/example-node-1/storage --output-format json':
                    return [{'storage': 'example-storage-b', 'content': 'images', 'enabled': 1, 'active': 1}, {'storage': 'example-storage-a', 'content': 'images', 'enabled': 1, 'active': 1}]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return []
                raise AssertionError(command)
        session = Session()
        orphaned, warnings, complete = proxmox.orphaned_homestack_volumes(session, test_config(), set())
        self.assertEqual(orphaned, [])
        self.assertEqual(warnings, [])
        self.assertTrue(complete)
        self.assertNotIn('pvesh get /nodes/example-node-1/storage/example-storage-b/content --content images --output-format json', session.commands)

    def test_orphan_scan_warns_when_assigned_storage_is_missing(self) -> None:

        class Session:

            def run_json_value(self, command: str, **_: object):
                if command == 'pvesh get /nodes --output-format json':
                    return [{'node': 'example-node-1'}]
                if command == 'pvesh get /nodes/example-node-1/storage --output-format json':
                    return [{'storage': 'example-storage-b', 'content': 'images', 'enabled': 1, 'active': 1}]
                raise AssertionError(command)
        orphaned, warnings, complete = proxmox.orphaned_homestack_volumes(Session(), test_config(), set())
        self.assertEqual(orphaned, [])
        self.assertEqual(warnings, ["Assigned HomeStack storage 'example-storage-a' is unavailable for image scanning on example-node-1"])
        self.assertFalse(complete)

    def test_orphan_scan_skips_offline_nodes_without_warning(self) -> None:
        class Session:
            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command == 'pvesh get /nodes --output-format json':
                    return [
                        {'node': 'example-node-1', 'status': 'online'},
                        {'node': 'example-node-3', 'status': 'offline'},
                    ]
                if command == 'pvesh get /nodes/example-node-1/storage --output-format json':
                    return [{'storage': 'example-storage-a', 'content': 'images', 'enabled': 1, 'active': 1}]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return []
                raise AssertionError(command)

        session = Session()
        orphaned, warnings, complete = proxmox.orphaned_homestack_volumes(
            session, test_config(), set()
        )
        self.assertEqual(orphaned, [])
        self.assertEqual(warnings, [])
        self.assertFalse(complete)
        self.assertFalse(
            any('/nodes/example-node-3/storage' in command for command in session.commands)
        )

class ManagedStatusResourceTests(unittest.TestCase):

    def test_global_status_inspects_only_gold_and_tagged_workspaces(self) -> None:
        cfg = test_config()
        resources = [{'vmid': 100, 'type': 'qemu', 'name': 'desktop', 'tags': ''}, {'vmid': 101, 'type': 'qemu', 'name': 'gold', 'tags': 'homestack-gold'}, {'vmid': 102, 'type': 'qemu', 'name': 'aideml', 'tags': 'other'}, {'vmid': 200, 'type': 'qemu', 'name': 'test1', 'tags': 'homestack-ws'}, {'vmid': 201, 'type': 'qemu', 'name': 'test2', 'tags': 'backup;homestack-ws'}]
        managed = status.homestack_status_resources(resources, cfg)
        self.assertEqual([item['vmid'] for item in managed], [101, 200, 201])

    def test_global_status_marks_orphan_scan_incomplete_for_offline_storage_node(self) -> None:
        class Session:
            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command == 'pvesh get /cluster/resources --type vm --output-format json':
                    return []
                if command == 'pvesh get /nodes --output-format json':
                    return [
                        {'node': 'example-node-1', 'status': 'online'},
                        {'node': 'example-node-3', 'status': 'offline'},
                    ]
                if command == 'pvesh get /nodes/example-node-1/storage --output-format json':
                    return [{'storage': 'example-storage-a', 'content': 'images', 'enabled': 1, 'active': 1}]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/content --content images --output-format json':
                    return []
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/status --output-format json':
                    return {'total': 1000, 'used': 250, 'avail': 750}
                raise AssertionError(command)

            def execution_info(self) -> dict[str, object]:
                return {'type': 'fake'}

        session = Session()
        result = status.global_status(session, test_config())
        self.assertFalse(result['summary']['orphan_scan_complete'])
        self.assertFalse(
            any('/nodes/example-node-3/storage' in command for command in session.commands)
        )
        self.assertFalse(
            any('orphan scan incomplete' in warning.lower() for warning in result['warnings'])
        )

class StorageCapacityStatusTests(unittest.TestCase):

    def test_reads_capacity_for_only_assigned_homestack_storages(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command == 'pvesh get /nodes --output-format json':
                    return [{'node': 'example-node-1'}, {'node': 'example-node-2'}, {'node': 'example-node-3'}]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/status --output-format json':
                    return {'total': 1000, 'used': 250, 'avail': 750}
                if command == 'pvesh get /nodes/example-node-2/storage/example-storage-b/status --output-format json':
                    return {'total': 2000, 'used': 500, 'avail': 1500}
                if command == 'pvesh get /nodes/example-node-3/storage/example-storage-b/status --output-format json':
                    return {'total': 3000, 'used': 600, 'avail': 2400}
                raise AssertionError(command)
        session = Session()
        entries, warnings = status.homestack_storage_capacities(session, test_config())
        self.assertEqual(warnings, [])
        self.assertEqual([(item['layout'], item['node'], item['storage']) for item in entries], [('homestack-storage-1', 'example-node-1', 'example-storage-a'), ('homestack-storage-2', 'example-node-2', 'example-storage-b'), ('homestack-storage-3', 'example-node-3', 'example-storage-b')])
        self.assertEqual(entries[1]['used_bytes'], 500)
        self.assertEqual(entries[1]['available_bytes'], 1500)

    def test_offline_node_capacity_is_reported_without_remote_call(self) -> None:
        class Session:
            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command == 'pvesh get /nodes --output-format json':
                    return [
                        {'node': 'example-node-1', 'status': 'online'},
                        {'node': 'example-node-3', 'status': 'offline'},
                    ]
                if command == 'pvesh get /nodes/example-node-1/storage/example-storage-a/status --output-format json':
                    return {'total': 1000, 'used': 250, 'avail': 750}
                raise AssertionError(command)

        session = Session()
        entries, warnings = status.homestack_storage_capacities(session, test_config())
        offline = next(item for item in entries if item['node'] == 'example-node-3')
        self.assertEqual(warnings, [])
        self.assertEqual(offline['node_status'], 'offline')
        self.assertFalse(offline['ok'])
        self.assertIsNone(offline['total_bytes'])
        self.assertFalse(
            any('/nodes/example-node-3/storage' in command for command in session.commands)
        )

class FreePercentDisplayTests(unittest.TestCase):

    def test_free_percent_uses_capacity_risk_thresholds(self) -> None:
        self.assertEqual(status.free_percent(100, 40), 40.0)
        self.assertEqual(ui.free_percent_color(30.0), 'green')
        self.assertEqual(ui.free_percent_color(29.9), 'yellow')
        self.assertEqual(ui.free_percent_color(10.0), 'yellow')
        self.assertEqual(ui.free_percent_color(9.9), 'red')

    def test_free_percent_text_formats_value(self) -> None:
        self.assertEqual(ui.free_percent_text(99.0).plain, '99.0%')
        self.assertEqual(ui.free_percent_text(None).plain, '—')

class StorageDisplayFormatTests(unittest.TestCase):

    def test_fixed_storage_unit_and_precision(self) -> None:
        value = int(1.75 * 1024 ** 4)
        self.assertEqual(ui.byte_value_in_unit(value, 'GiB', 0), '1792')
        self.assertEqual(ui.byte_value_in_unit(value, 'TiB', 2), '1.75')
        self.assertEqual(ui.byte_value_in_unit(value, 'TiB', 3), '1.750')

class GlobalStorageTableTests(unittest.TestCase):

    def test_storage_table_replaces_total_with_free_percent(self) -> None:
        result = {'summary': {'storage_unit': 'GiB', 'storage_decimals': 0, 'storage_layouts': [{'layout': 'homestack-storage-2', 'node': 'example-node-2', 'storage': 'example-storage-b', 'total_bytes': 100 * 1024 ** 3, 'used_bytes': 75 * 1024 ** 3, 'available_bytes': 25 * 1024 ** 3}]}}
        table = ui.build_global_storage_table(result, narrow=False)
        self.assertIsNotNone(table)
        assert table is not None
        self.assertEqual([column.header for column in table.columns], ['LAYOUT', 'NODE', 'STATE', 'STORAGE', 'USED GiB', 'FREE GiB', 'FREE %'])

    def test_offline_node_state_is_visible(self) -> None:
        result = {'summary': {'storage_unit': 'GiB', 'storage_decimals': 0, 'storage_layouts': [{'layout': 'homestack-storage-3', 'node': 'example-node-3', 'node_status': 'offline', 'storage': 'example-storage-b', 'total_bytes': None, 'used_bytes': None, 'available_bytes': None}]}}
        table = ui.build_global_storage_table(result, narrow=False)
        self.assertIsNotNone(table)
        output = StringIO()
        console = ui.Console(file=output, force_terminal=False, width=120)
        console.print(table)
        self.assertIn('offline', output.getvalue())

class GlobalSummaryTests(unittest.TestCase):

    def test_cluster_node_states_are_summarized(self) -> None:
        result = {'summary': {'workspace_count': 0, 'running': 0, 'stopped': 0, 'nodes': [{'node': 'example-node-1', 'status': 'online'}, {'node': 'example-node-3', 'status': 'offline'}], 'homes': {'used_bytes': None, 'quota_bytes': 0, 'free_percent': None, 'missing_count': 0}}}
        text = '\n'.join(line.plain for line in ui.global_summary_lines(result, narrow=False))
        self.assertIn('Cluster nodes', text)
        self.assertIn('example-node-3 offline', text)

    def test_zero_workspaces_keeps_storage_in_separate_table_data(self) -> None:
        result = {'summary': {'workspace_count': 0, 'running': 0, 'stopped': 0, 'storage_layouts': [{'layout': 'homestack-storage-2', 'node': 'example-node-2', 'storage': 'example-storage-b', 'total_bytes': 2 * 1024 ** 4, 'used_bytes': 512 * 1024 ** 3, 'available_bytes': int(1.5 * 1024 ** 4)}], 'homes': {'used_bytes': None, 'quota_bytes': 0, 'free_percent': None, 'missing_count': 0}}}
        lines = ui.global_summary_lines(result, narrow=False)
        text = '\n'.join((line.plain for line in lines))
        rows = ui.global_storage_rows(result)
        self.assertNotIn('homestack-storage-2', text)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['layout'], 'homestack-storage-2')
        self.assertEqual(rows[0]['node'], 'example-node-2')
        self.assertEqual(rows[0]['storage'], 'example-storage-b')
        self.assertEqual(rows[0]['used_bytes'], 512 * 1024 ** 3)
        self.assertEqual(rows[0]['available_bytes'], int(1.5 * 1024 ** 4))

    def test_zero_workspaces_hides_home_metrics(self) -> None:
        result = {'summary': {'workspace_count': 0, 'running': 0, 'stopped': 0, 'homes': {'used_bytes': None, 'quota_bytes': 0, 'free_percent': None, 'missing_count': 0}}}
        lines = ui.global_summary_lines(result, narrow=False)
        text = '\n'.join((line.plain for line in lines))
        self.assertIn('Workspaces', text)
        self.assertIn('0 (0 running / 0 stopped)', text)
        self.assertNotIn('Persistent homes', text)
        self.assertNotIn('Home filesystem free', text)

    def test_zero_workspaces_shows_detached_volume_count_when_present(self) -> None:
        result = {'summary': {'workspace_count': 0, 'running': 0, 'stopped': 0, 'homes': {'used_bytes': None, 'quota_bytes': 0, 'free_percent': None, 'missing_count': 0}, 'orphaned_volumes': [{'volid': 'example-storage-a:vm-200-hs-home-user'}]}}
        lines = ui.global_summary_lines(result, narrow=False)
        text = '\n'.join((line.plain for line in lines))
        self.assertIn('Workspaces', text)
        self.assertIn('Detached HS volumes', text)
        self.assertIn('1', text)
