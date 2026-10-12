from __future__ import annotations

from io import StringIO
import unittest
from unittest.mock import patch

from homestack import cli, ui
from support import test_config

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
        self.assertIsNone(create.node)
        self.assertEqual(parser.parse_args(['refresh', '200']).target, '200')
        self.assertEqual(parser.parse_args(['refresh', 'test1']).target, 'test1')
        self.assertEqual(parser.parse_args(['destroy', '200']).target, '200')
        self.assertEqual(parser.parse_args(['destroy', 'test1']).target, 'test1')
        migrate = parser.parse_args(['migrate', 'test1', 'example-node-2', '--target-storage', 'example-storage-b'])
        self.assertEqual(migrate.target, 'test1')
        self.assertEqual(migrate.target_node, 'example-node-2')
        self.assertEqual(migrate.target_storage, 'example-storage-b')
        repo_default = parser.parse_args(['repo', 'test1'])
        self.assertEqual(repo_default.target, 'test1')
        self.assertIsNone(repo_default.repository)
        repo_explicit = parser.parse_args(
            ['repo', 'test1', 'hipotures/tklivetracker']
        )
        self.assertEqual(repo_explicit.repository, 'hipotures/tklivetracker')
        self.assertIsNone(parser.parse_args(['status']).target)
        self.assertEqual(parser.parse_args(['status', '200']).target, '200')
        self.assertEqual(parser.parse_args(['status', 'test1']).target, 'test1')

    def test_sync_command_is_rejected(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            cli.build_parser().parse_args(['sync', '200'])
        self.assertEqual(raised.exception.code, 2)

    def test_create_accepts_target_node_with_large_home(self) -> None:
        args = cli.build_parser().parse_args([
            'create', '200', 'test1', '--node', 'example-node-2',
            '--storage', 'example-storage-b', '--home-size', '500G',
        ])
        self.assertEqual(args.node, 'example-node-2')
        self.assertEqual(args.storage, 'example-storage-b')
        self.assertEqual(args.home_size, '500G')


class CreateCommandTests(unittest.TestCase):
    def test_json_plan_forwards_destination_without_creating_vm(self) -> None:
        cfg = test_config()
        plan = {
            'node': 'example-node-2',
            'source_node': cfg.node,
            'transfer_method': 'stream',
        }
        with (
            patch('sys.argv', [
                'homestack', 'create', '200', 'test1', '--node', 'example-node-2',
                '--home-size', '500G', '--storage', 'example-storage-b', '--json',
            ]),
            patch.object(cli, 'load_config', return_value=cfg),
            patch.object(cli, 'open_transport') as transport,
            patch.object(cli, 'build_create_plan', return_value=plan) as build_plan,
            patch.object(cli, 'create_workspace') as create,
            patch.object(cli, 'emit_json') as emit,
        ):
            self.assertEqual(cli.main(), 3)
        build_plan.assert_called_once_with(
            transport.return_value.__enter__.return_value,
            cfg, 200, 'test1', '500G', storage='example-storage-b', node='example-node-2',
        )
        create.assert_not_called()
        self.assertEqual(emit.call_args.args[0]['plan'], plan)
        self.assertTrue(emit.call_args.args[0]['confirmation_required'])


class CreatePlanRenderingTests(unittest.TestCase):
    def test_create_plan_shows_target_node(self) -> None:
        plan = {
            'vmid': 210,
            'name': 'test210',
            'node': 'example-node-2',
            'source_node': 'example-node-1',
            'transfer_method': 'stream',
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
        self.assertIn('example-node-1', rendered)
        self.assertIn('Stream to target', rendered)
