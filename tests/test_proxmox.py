from __future__ import annotations

import time
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import cloudinit, guest, lifecycle, models, proxmox

from support import FakeSession, test_config

class ControlNodeTests(unittest.TestCase):

    def test_node_shell_command_runs_directly_on_control_node(self) -> None:
        cfg = replace(test_config(), node='example-node-1', control_node='example-node-2')
        self.assertEqual(proxmox.node_shell_command(cfg, 'example-node-2', 'hostname'), 'hostname')
        self.assertEqual(proxmox.node_shell_command(cfg, 'example-node-1', 'hostname'), 'ssh -o BatchMode=yes root@example-node-1 hostname')

    def test_cluster_node_statuses_preserve_online_state(self) -> None:
        class Session:
            def run_json_value(self, command: str, **_: object):
                self.command = command
                return [
                    {'node': 'example-node-2', 'status': 'offline'},
                    {'node': 'example-node-1', 'status': 'online'},
                ]

        session = Session()
        self.assertEqual(
            proxmox.cluster_node_statuses(session),
            [
                {'node': 'example-node-1', 'status': 'online', 'online': True},
                {'node': 'example-node-2', 'status': 'offline', 'online': False},
            ],
        )
        self.assertEqual(session.command, 'pvesh get /nodes --output-format json')

class NetworkInventoryTests(unittest.TestCase):
    def test_node_network_and_dns_use_read_only_pvesh_get(self) -> None:
        class Session:
            def __init__(self) -> None:
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                if command.endswith('/network --output-format json'):
                    return [{'iface': 'vmbr0', 'type': 'bridge'}]
                if command.endswith('/dns --output-format json'):
                    return {'dns1': '192.0.2.1'}
                raise AssertionError(command)

        session = Session()
        self.assertEqual(
            proxmox.node_network_inventory(session, 'example-node-1')[0]['iface'],
            'vmbr0',
        )
        self.assertEqual(
            proxmox.node_dns_config(session, 'example-node-1')['dns1'],
            '192.0.2.1',
        )
        self.assertTrue(all(command.startswith('pvesh get ') for command in session.commands))


class VmStatusTests(unittest.TestCase):

    def test_qm_status_uses_proxmox_json_api(self) -> None:

        class JsonSession:

            def __init__(self) -> None:
                self.cfg = test_config()
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                return {'status': 'running'}
        session = JsonSession()
        self.assertEqual(proxmox.qm_status(session, 200), 'running')
        self.assertEqual(session.commands, ['pvesh get /nodes/example-node-1/qemu/200/status/current --output-format json'])

class VmConfigTests(unittest.TestCase):

    def test_qm_config_uses_proxmox_json_api_not_terminal_text(self) -> None:

        class JsonSession:

            def __init__(self) -> None:
                self.cfg = test_config()
                self.commands: list[str] = []

            def run_json_value(self, command: str, **_: object):
                self.commands.append(command)
                return {'name': 'test1', 'tags': 'homestack-ws', 'scsi1': 'example-storage-a:vm-200-disk-1,discard=on,iothread=1,serial=HS_HOME_200,size=20G,ssd=1', 'memory': 4096}
        session = JsonSession()
        config = proxmox.qm_config(session, 200)
        self.assertEqual(config['name'], 'test1')
        self.assertEqual(config['tags'], 'homestack-ws')
        self.assertEqual(config['memory'], '4096')
        self.assertEqual(session.commands, ['pvesh get /nodes/example-node-1/qemu/200/config --output-format json'])

class TagTests(unittest.TestCase):

    def test_exact_semicolon_separated_tags(self) -> None:
        self.assertEqual(proxmox.parse_tags(' homestack-gold ; backup ; homestack-ws '), frozenset({'homestack-gold', 'backup', 'homestack-ws'}))
        self.assertTrue(proxmox.has_tag('backup;homestack-gold', models.GOLD_TAG))
        self.assertTrue(proxmox.has_tag('backup;homestack-ws', models.WORKSPACE_TAG))
        self.assertFalse(proxmox.has_tag('prefix-homestack-ws-suffix', models.WORKSPACE_TAG))

class StorageLayoutTests(unittest.TestCase):

    def test_first_storage_is_default_and_override_must_be_assigned(self) -> None:
        cfg = test_config()
        self.assertEqual(proxmox.resolve_homestack_storage(cfg, 'example-node-1'), 'example-storage-a')
        self.assertEqual(proxmox.resolve_homestack_storage(cfg, 'example-node-2'), 'example-storage-b')
        self.assertEqual(proxmox.resolve_homestack_storage(cfg, 'example-node-2', 'example-storage-b'), 'example-storage-b')
        with self.assertRaises(models.AppError):
            proxmox.resolve_homestack_storage(cfg, 'example-node-2', 'disallowed-storage')

class HomeDiskTests(unittest.TestCase):

    def test_home_label_and_size(self) -> None:
        self.assertEqual(proxmox.home_label(200), 'HS_HOME_200')
        self.assertEqual(proxmox.parse_home_size('20G'), ('20G', 20))
        self.assertEqual(proxmox.parse_home_size('1t'), ('1T', 1024))
        with self.assertRaises(models.AppError):
            proxmox.parse_home_size('500M')

    def test_logical_volume_names(self) -> None:
        self.assertEqual(proxmox.root_volume_name(200), 'vm-200-hs-root-default')
        self.assertEqual(proxmox.home_volume_name(200, 'user'), 'vm-200-hs-home-user')
        self.assertEqual(proxmox.named_volume_id('example-storage-a', proxmox.root_volume_name(200)), 'example-storage-a:vm-200-hs-root-default')

    def test_named_raw_volume_allocation_uses_requested_name(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run(self, command: str, **_: object):
                self.commands.append(command)
                return models.RemoteResult(0, '')
        session = Session()
        volume = proxmox.allocate_named_raw_volume(session, 'example-storage-a', 200, 'vm-200-hs-home-user', '20G')
        self.assertEqual(volume, 'example-storage-a:vm-200-hs-home-user')
        self.assertEqual(session.commands, ['pvesm alloc example-storage-a 200 vm-200-hs-home-user 20G --format raw'])

    def test_replace_disk_volume_preserves_options(self) -> None:
        source = 'local-zfs:vm-101-disk-0,iothread=1,size=16G,ssd=1'
        self.assertEqual(proxmox.replace_disk_volume(source, 'example-storage-a:vm-200-hs-root-default'), 'example-storage-a:vm-200-hs-root-default,iothread=1,size=16G,ssd=1')

    def test_pve_rename_volume_uses_proxmox_storage_layer(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run(self, command: str, **_: object):
                self.commands.append(command)
                return models.RemoteResult(0, 'example-storage-a:vm-200-hs-root-default\n')
        session = Session()
        result = proxmox.pve_rename_volume(session, 'example-storage-a:vm-200-disk-0', 200, 'vm-200-hs-root-default')
        self.assertEqual(result, 'example-storage-a:vm-200-hs-root-default')
        self.assertEqual(len(session.commands), 1)
        self.assertIn('PVE::Storage::rename_volume', session.commands[0])
        self.assertIn('vm-200-hs-root-default', session.commands[0])

    def test_disk_option(self) -> None:
        disk = 'example-storage-a:vm-200-disk-1,discard=on,iothread=1,size=20G,serial=HS_HOME_200,ssd=1'
        self.assertEqual(proxmox.disk_option(disk, 'serial'), 'HS_HOME_200')
        self.assertEqual(proxmox.disk_storage(disk), 'example-storage-a')
        self.assertEqual(guest.parse_disk_size_gb(disk), 20.0)

    def test_root_import_spec_clones_gold_root_into_existing_workspace(self) -> None:
        cfg = test_config()
        gold = 'local-zfs:vm-101-disk-0,iothread=1,size=16G'
        self.assertEqual(proxmox.root_import_spec('example-storage-a', gold), 'example-storage-a:0,import-from=local-zfs:vm-101-disk-0,iothread=1')

    def test_boot_order_contains_root_disk(self) -> None:
        self.assertTrue(proxmox.boot_order_contains_disk('order=ide2;scsi0;net0', 'scsi0'))
        self.assertFalse(proxmox.boot_order_contains_disk('order=ide2;net0', 'scsi0'))
        self.assertFalse(proxmox.boot_order_contains_disk('', 'scsi0'))

    def test_create_snippet_formats_only_blank_new_home(self) -> None:
        captured: dict[str, str] = {}

        def capture(_session: object, path: Path, content: str, _mode: int) -> None:
            captured[path.name] = content
        with patch.object(cloudinit, 'remote_write_text', side_effect=capture):
            cloudinit.write_snippets(FakeSession(), test_config(), 'test1', 200, 'BC:24:11:00:00:01', '192.0.2.200', 'HS_HOME_200', authorized_keys='ssh-ed25519 AAAATEST test\n')
        vendor = captured['homestack-test1-vendor.yaml']
        self.assertIn('allow_format=1', vendor)
        self.assertIn('mkfs.ext4 -m 0 -L', vendor)
        self.assertIn('rm -rf -- "$home_path/lost+found"', vendor)
        self.assertIn('expected_serial=HS_HOME_200', vendor)
        self.assertIn('LABEL=%s %s ext4 defaults 0 2', vendor)
        self.assertNotIn('virtiofs', vendor.lower())

    def test_refresh_snippet_refuses_blank_home(self) -> None:
        captured: dict[str, str] = {}

        def capture(_session: object, path: Path, content: str, _mode: int) -> None:
            captured[path.name] = content
        with patch.object(cloudinit, 'remote_write_text', side_effect=capture):
            cloudinit.write_snippets(FakeSession(), test_config(), 'test1', 200, 'BC:24:11:00:00:01', '192.0.2.200', 'HS_HOME_200', replace=True, preserve_home=True)
        vendor = captured['homestack-test1-vendor.yaml']
        self.assertIn('allow_format=0', vendor)
        self.assertIn('persistent home is blank during refresh; refusing mkfs', vendor)

class QgaWaitTests(unittest.TestCase):

    def test_qga_ping_command_is_bounded(self) -> None:
        self.assertEqual(guest.qga_ping_command(200), 'timeout -k 2s 5s qm guest cmd 200 ping')

    def test_wait_for_qga_retries_transient_transport_error(self) -> None:

        class Session:

            def __init__(self) -> None:
                self.calls = 0

            def run(self, command: str, **_: object):
                self.calls += 1
                if self.calls == 1:
                    raise models.AppError('transient Herdr timeout')
                return models.RemoteResult(0, '')
        session = Session()
        with patch.object(time, 'sleep', return_value=None):
            guest.wait_for_qga(session, 200, timeout=30)
        self.assertEqual(session.calls, 2)

class SnippetMigrationTests(unittest.TestCase):

    def test_snippet_copy_relays_through_control_node(self) -> None:
        cfg = replace(test_config(), control_node='example-node-2', node='example-node-2')

        class Session:

            def __init__(self) -> None:
                self.commands: list[str] = []

            def run(self, command: str, **_: object):
                self.commands.append(command)
                return models.RemoteResult(0, '')
        session = Session()
        copied = cloudinit.sync_snippets_to_node(session, cfg, 'example-node-3', 'example-node-2', 'test1')
        self.assertEqual(len(copied), 4)
        self.assertEqual(len(session.commands), 1)
        command = session.commands[0]
        self.assertIn('root@example-node-3:/var/lib/vz/snippets/homestack-test1-user.yaml', command)
        self.assertNotIn('root@example-node-2:/var/lib/vz/snippets', command)
        self.assertIn('cp -p "$hs_tmp/homestack-test1-user.yaml"', command)

    def test_snippet_failure_happens_before_workspace_shutdown(self) -> None:
        plan = {'vmid': 200, 'name': 'test1', 'source_node': 'example-node-3', 'target_node': 'example-node-2', 'target_storage': 'example-storage-b', 'status': 'running', 'home_label': 'HS_HOME_200', 'volumes': []}
        with patch.object(lifecycle, 'sync_snippets_to_node', side_effect=models.AppError('snippet copy failed')), patch.object(lifecycle, 'shutdown_vm_on_node') as shutdown:
            with self.assertRaisesRegex(models.AppError, 'snippet copy failed'):
                lifecycle.migrate_workspace(object(), test_config(), plan, json_mode=True)
        shutdown.assert_not_called()
