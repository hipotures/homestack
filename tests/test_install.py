from __future__ import annotations

from contextlib import contextmanager
from dataclasses import replace
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from homestack import cli, config, install, models
from support import test_config


class ReadOnlySession:
    def __init__(self, responses: dict[str, object] | None = None) -> None:
        self.responses = responses or {}
        self.commands: list[str] = []
        self.cfg = test_config()

    def run_json_value(self, command: str, **_: object) -> object:
        self.commands.append(command)
        if command not in self.responses:
            raise AssertionError(f'unexpected command: {command}')
        return self.responses[command]


class InstallCliTests(unittest.TestCase):
    def test_install_parser_and_help_do_not_require_config(self) -> None:
        parser = cli.build_parser()
        self.assertEqual(parser.parse_args(['install']).command, 'install')
        with patch('sys.argv', ['homestack', 'install', '--help']), patch.object(
            cli, 'load_config', side_effect=AssertionError('must not load config')
        ):
            self.assertEqual(cli.main(), 0)

    def test_install_dispatches_before_config_loading_or_transport(self) -> None:
        missing = Path('/tmp/definitely-missing-homestack-config.toml')
        with patch('sys.argv', ['homestack', '--config', str(missing), 'install']), patch(
            'homestack.install.run_installer', return_value=0
        ) as run, patch.object(
            cli, 'load_config', side_effect=AssertionError('must not load config')
        ), patch.object(cli, 'open_transport', side_effect=AssertionError('must not open')):
            self.assertEqual(cli.main(), 0)
        run.assert_called_once_with(missing)

    def test_normal_command_still_requires_config(self) -> None:
        missing = Path('/tmp/definitely-missing-homestack-config.toml')
        with patch('sys.argv', ['homestack', '--config', str(missing), 'transport']), patch.object(
            cli, 'open_transport', side_effect=AssertionError('must not open')
        ):
            self.assertEqual(cli.main(), 1)

    def test_parser_default_observes_current_xdg_location(self) -> None:
        with patch.dict('os.environ', {'XDG_CONFIG_HOME': '/tmp/install-xdg'}):
            parsed = cli.build_parser().parse_args(['install'])
        self.assertEqual(parsed.config, Path('/tmp/install-xdg/homestack/config.toml'))


class ExistingConfigSafetyTests(unittest.TestCase):
    def test_noninteractive_fresh_install_writes_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            with patch.object(install.sys.stdin, 'isatty', return_value=False):
                with self.assertRaisesRegex(models.AppError, 'requires a TTY'):
                    install.run_installer(path)
            self.assertFalse(path.exists())

    def test_existing_default_action_aborts_without_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            config.publish_config(replace(test_config(), path=path))
            original = path.read_bytes()
            with patch.object(install.sys.stdin, 'isatty', return_value=True), patch.object(
                install, '_existing_action', return_value='1'
            ), patch.object(install, 'publish_config') as publish:
                self.assertEqual(install.run_installer(path), 0)
            publish.assert_not_called()
            self.assertEqual(path.read_bytes(), original)

    def test_reconfigure_uses_loaded_values_as_defaults(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            existing = replace(
                test_config(), path=path, herdr_workspace='current-workspace', gold_vmid=222
            )
            config.publish_config(existing)
            seen: list[config.Config] = []

            def capture(cfg: config.Config) -> config.Config:
                seen.append(cfg)
                raise models.AppError('stop after defaults')

            with patch.object(install.sys.stdin, 'isatty', return_value=True), patch.object(
                install, '_existing_action', return_value='2'
            ), patch.object(install, '_configure_transport', side_effect=capture):
                with self.assertRaisesRegex(models.AppError, 'stop after defaults'):
                    install.run_installer(path)
            self.assertEqual(seen[0].herdr_workspace, 'current-workspace')
            self.assertEqual(seen[0].gold_vmid, 222)

    def test_replace_from_scratch_does_not_reuse_existing_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config.toml'
            config.publish_config(replace(
                test_config(), path=path, herdr_workspace='stale-workspace', gold_vmid=222
            ))
            seen: list[config.Config] = []

            def capture(cfg: config.Config) -> config.Config:
                seen.append(cfg)
                raise models.AppError('stop after defaults')

            with patch.object(install.sys.stdin, 'isatty', return_value=True), patch.object(
                install, '_existing_action', return_value='3'
            ), patch.object(install, '_configure_transport', side_effect=capture):
                with self.assertRaisesRegex(models.AppError, 'stop after defaults'):
                    install.run_installer(path)
            self.assertNotEqual(seen[0].herdr_workspace, 'stale-workspace')
            self.assertNotEqual(seen[0].gold_vmid, 222)


class DiscoveryTests(unittest.TestCase):
    def test_single_and_multi_node_storage_discovery_respects_restrictions(self) -> None:
        definitions = [
            {'storage': 'only-pve1', 'content': 'images', 'nodes': 'pve1'},
            {'storage': 'shared', 'content': 'images,snippets'},
        ]
        statuses = [
            {'node': 'pve1', 'status': 'online', 'online': True},
            {'node': 'pve2', 'status': 'offline', 'online': False},
        ]
        session = ReadOnlySession()
        with patch.object(install, 'node_storage_inventory', return_value=[
            {'storage': 'only-pve1', 'content': 'images', 'enabled': 1, 'active': 1},
            {'storage': 'shared', 'content': 'images,snippets', 'enabled': 1, 'active': 1},
        ]):
            options, unverified = install._storage_options(session, statuses, definitions)
        self.assertEqual(options['pve1'], ['only-pve1', 'shared'])
        self.assertEqual(options['pve2'], ['shared'])
        self.assertEqual(unverified, {'pve2'})

    def test_first_selected_storage_is_preserved_as_layout_default(self) -> None:
        with patch.object(install.Prompt, 'ask', return_value='2,1'):
            selected = install._select_numbered('select', ['slow', 'fast'])
        self.assertEqual(selected, ('fast', 'slow'))

    def test_exactly_one_valid_tagged_gold_is_confirmed(self) -> None:
        resources = [{'type': 'qemu', 'vmid': 101, 'name': 'gold', 'node': 'pve1', 'tags': 'homestack-gold'}]
        session = ReadOnlySession()
        with patch.object(
            install, '_validated_gold', return_value=(resources[0], {'scsi0': 'store:disk', 'tags': 'homestack-gold'})
        ), patch.object(install.Confirm, 'ask', return_value=True):
            self.assertEqual(install._choose_gold(session, test_config(), resources), (101, 'pve1'))

    def test_zero_or_ambiguous_gold_requires_explicit_vmid(self) -> None:
        zero = [{'type': 'qemu', 'vmid': 102, 'name': 'candidate', 'node': 'pve1'}]
        ambiguous = [
            {'type': 'qemu', 'vmid': 101, 'name': 'gold1', 'node': 'pve1', 'tags': 'homestack-gold'},
            {'type': 'qemu', 'vmid': 102, 'name': 'gold2', 'node': 'pve2', 'tags': 'homestack-gold'},
        ]
        session = ReadOnlySession()

        def validated(_session: object, _cfg: object, resources: list[dict[str, object]], vmid: int):
            item = next(item for item in resources if item['vmid'] == vmid)
            return item, {'scsi0': 'store:disk', 'tags': 'homestack-gold'}

        with patch.object(install, '_validated_gold', side_effect=validated), patch.object(
            install.IntPrompt, 'ask', return_value=102
        ) as prompt:
            self.assertEqual(install._choose_gold(session, test_config(), zero), (102, 'pve1'))
            self.assertEqual(install._choose_gold(session, test_config(), ambiguous), (102, 'pve2'))
        self.assertEqual(prompt.call_count, 2)

    def test_discovery_helpers_issue_only_read_only_queries(self) -> None:
        responses = {
            'pvesh get /nodes --output-format json': [{'node': 'pve1', 'status': 'online'}],
            'pvesh get /cluster/resources --type vm --output-format json': [],
            'pvesh get /storage --output-format json': [],
        }
        session = ReadOnlySession(responses)
        from homestack import proxmox
        proxmox.cluster_node_statuses(session)
        proxmox.cluster_vm_resources(session)
        proxmox.cluster_storage_definitions(session)
        self.assertTrue(all(command.startswith('pvesh get ') for command in session.commands))


class InstallerValueTests(unittest.TestCase):
    def test_network_validation_preserves_vmid_mapping(self) -> None:
        answers = iter(['10.20.30.0/24', '10.20.30.1', '10.20.30.53, 1.1.1.1'])
        with patch.object(install.Prompt, 'ask', side_effect=lambda *args, **kwargs: next(answers)):
            self.assertEqual(
                install._configure_network(test_config()),
                ('10.20.30', 24, '10.20.30.1', ('10.20.30.53', '1.1.1.1')),
            )

    def test_selected_ssh_identities_are_paths_only_and_ordered(self) -> None:
        with patch.object(
            install.Prompt, 'ask', return_value='~/.ssh/key_a_sk, ~/.ssh/key_b_sk'
        ), patch.object(install, '_discover_hardware_identities', return_value=[]):
            identities = install._configure_identities(test_config())
        self.assertEqual(identities, ('~/.ssh/key_a_sk', '~/.ssh/key_b_sk'))

    def test_declining_sync_returns_valid_empty_current_schema_values(self) -> None:
        with patch.object(install.Confirm, 'ask', return_value=False):
            self.assertEqual(install._configure_sync(test_config()), ((), (), False))

    def test_sync_paths_and_commands_preserve_input_order(self) -> None:
        prompts = iter(['~/first', '~/second/', '', 'command one', 'command two', ''])
        with patch.object(install.Confirm, 'ask', side_effect=[True, False]), patch.object(
            install.Prompt, 'ask', side_effect=lambda *args, **kwargs: next(prompts)
        ):
            paths, commands, verbose = install._configure_sync(test_config())
        self.assertEqual(paths, ('~/first', '~/second/'))
        self.assertEqual(commands, ('command one', 'command two'))
        self.assertFalse(verbose)


if __name__ == '__main__':
    unittest.main()
