from __future__ import annotations

import unittest
from unittest.mock import patch

from homestack import cli
from support import test_config


class ResizeCommandTests(unittest.TestCase):
    def test_parser_requires_exactly_one_disk_size(self) -> None:
        parser = cli.build_parser()
        args = parser.parse_args(['resize', 'gpu', '--root-size', '32G'])
        self.assertEqual((args.target, args.root_size, args.home_size), ('gpu', '32G', None))
        for arguments in (['resize', 'gpu'], ['resize', 'gpu', '--root-size', '32G', '--home-size', '500G']):
            with self.subTest(arguments=arguments), patch('sys.stderr'), self.assertRaises(SystemExit):
                parser.parse_args(arguments)

    def test_json_plan_and_execution_require_confirmation(self) -> None:
        cfg = test_config()
        for confirmed in (False, True):
            with (
                self.subTest(confirmed=confirmed),
                patch('sys.argv', ['homestack', 'resize', 'gpu', '--home-size', '500G', '--json'] + (['--yes'] if confirmed else [])),
                patch.object(cli, 'load_config', return_value=cfg),
                patch.object(cli, 'open_transport') as transport,
                patch.object(cli, 'resolve_workspace_target', return_value=207),
                patch('homestack.resize.build_resize_plan', return_value={'vmid': 207}) as build,
                patch('homestack.resize.resize_workspace', return_value={'ok': True}) as execute,
                patch.object(cli, 'emit_json') as emit,
            ):
                self.assertEqual(cli.main(), 0 if confirmed else 3)
                build.assert_called_once_with(
                    transport.return_value.__enter__.return_value,
                    cfg, 207, root_size=None, home_size='500G',
                )
                if confirmed:
                    execute.assert_called_once()
                    self.assertEqual(emit.call_args.args[0], {'ok': True})
                else:
                    execute.assert_not_called()
                    self.assertTrue(emit.call_args.args[0]['confirmation_required'])


if __name__ == '__main__':
    unittest.main()
