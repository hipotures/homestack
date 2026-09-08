from __future__ import annotations

from io import StringIO
import unittest
from unittest.mock import patch

from homestack import cli, ui

class ParserTests(unittest.TestCase):
    def test_package_cli_main_is_callable(self) -> None:
        self.assertTrue(callable(cli.main))

    def test_error_panel_preserves_bracketed_config_section(self) -> None:
        output = StringIO()
        test_console = ui.Console(file=output, force_terminal=False, width=100)
        with patch.object(ui, 'console', test_console):
            ui.show_error('Missing configuration value [transport] type')
        self.assertIn('[transport]', output.getvalue())


    def test_create_refresh_destroy_status_and_migrate(self) -> None:
        parser = cli.build_parser()
        create = parser.parse_args(['create', '200', 'test1', '--home-size', '40G'])
        self.assertEqual((create.vmid, create.name, create.home_size), (200, 'test1', '40G'))
        self.assertEqual(parser.parse_args(['refresh', '200']).target, '200')
        self.assertEqual(parser.parse_args(['refresh', 'test1']).target, 'test1')
        self.assertEqual(parser.parse_args(['destroy', '200']).target, '200')
        self.assertEqual(parser.parse_args(['destroy', 'test1']).target, 'test1')
        migrate = parser.parse_args(['migrate', 'test1', 'example-node-2', '--target-storage', 'example-storage-b'])
        self.assertEqual(migrate.target, 'test1')
        self.assertEqual(migrate.target_node, 'example-node-2')
        self.assertEqual(migrate.target_storage, 'example-storage-b')
        self.assertIsNone(parser.parse_args(['status']).target)
        self.assertEqual(parser.parse_args(['status', '200']).target, '200')
        self.assertEqual(parser.parse_args(['status', 'test1']).target, 'test1')


class CreatePlanRenderingTests(unittest.TestCase):
    def test_create_plan_shows_target_node(self) -> None:
        plan = {
            'vmid': 210,
            'name': 'test210',
            'node': 'example-node-2',
            'gold_vmid': 101,
            'root_disk_gb': 16.0,
            'ip': '192.0.2.210',
            'cidr': 24,
            'gateway': '192.0.2.1',
            'root_storage': 'example-storage-b',
            'root_volume_name': 'vm-210-hs-root-default',
            'home_disk': 'scsi1',
            'home_storage': 'example-storage-b',
            'home_size': '20G',
            'home_label': 'HS_HOME_210',
            'home_volume_name': 'vm-210-hs-home-user',
            'stale_snippets': [],
            'ssh_public_keys': ['example hardware-backed key'],
            'ssh_key_source': 'trusted desktop',
            'user': 'user',
            'uid': 1000,
            'gid': 1000,
            'transport': 'herdr',
        }
        output = StringIO()
        test_console = ui.Console(file=output, force_terminal=False, width=120)
        with patch.object(ui, 'console', test_console):
            ui.show_create_plan(plan)
        rendered = output.getvalue()
        self.assertIn('Node', rendered)
        self.assertIn('example-node-2', rendered)
