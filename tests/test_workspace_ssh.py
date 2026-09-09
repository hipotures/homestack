from __future__ import annotations

import shutil
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from homestack import config, lifecycle, models, workspace_ssh

from support import FakeSession, test_config

class LocalSSHConfigTests(unittest.TestCase):

    def test_writes_exact_config_for_vm_200_and_vm_201_with_mode_0600(self) -> None:
        cases = ((200, 'test1', '192.0.2.200'), (201, 'workspace-two', '192.0.2.201'))
        for vmid, name, ip in cases:
            with self.subTest(vmid=vmid, name=name), tempfile.TemporaryDirectory() as tmp:
                home = Path(tmp)
                with patch.object(Path, 'home', return_value=home):
                    path = workspace_ssh.write_local_ssh_config(test_config(), vmid, name, ip)
                self.assertEqual(path, home / '.ssh/config.d/homestack' / f'vm{vmid}-{name}.conf')
                self.assertEqual(path.read_text(encoding='utf-8'), f'Host {name}\n    HostName {ip}\n    User user\n    IdentityFile ~/.ssh/example-hardware-key\n    IdentitiesOnly yes\n    LogLevel FATAL\n')
                self.assertEqual(path.stat().st_mode & 511, 384)

    def test_uses_workspace_ssh_config_instead_of_hardcoded_keys(self) -> None:
        cfg = replace(
            test_config(),
            workspace_ssh=config.WorkspaceSSHConfig(
                user='user',
                identity_files=('~/.ssh/custom-key-a', '/keys/custom-key-b'),
                identities_only=False,
                log_level='ERROR',
            ),
        )
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            with patch.object(Path, 'home', return_value=home):
                path = workspace_ssh.write_local_ssh_config(
                    cfg, 202, 'custom', '192.0.2.202'
                )
            self.assertEqual(
                path.read_text(encoding='utf-8'),
                'Host custom\n'
                '    HostName 192.0.2.202\n'
                '    User user\n'
                '    IdentityFile ~/.ssh/custom-key-a\n'
                '    IdentityFile /keys/custom-key-b\n'
                '    IdentitiesOnly no\n'
                '    LogLevel ERROR\n',
            )

    def test_replaces_only_stale_config_for_same_vmid(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            directory = home / '.ssh/config.d/homestack'
            directory.mkdir(parents=True)
            stale = directory / 'vm200-old-name.conf'
            other_vm = directory / 'vm201-keep.conf'
            stale.write_text('old', encoding='utf-8')
            other_vm.write_text('keep', encoding='utf-8')
            with patch.object(Path, 'home', return_value=home):
                created = workspace_ssh.write_local_ssh_config(test_config(), 200, 'new-name', '192.0.2.200')
            self.assertFalse(stale.exists())
            self.assertTrue(created.exists())
            self.assertEqual(other_vm.read_text(encoding='utf-8'), 'keep')

    def test_atomic_replace_failure_keeps_previous_entry(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            directory = home / '.ssh/config.d/homestack'
            directory.mkdir(parents=True)
            stale = directory / 'vm200-old-name.conf'
            target = directory / 'vm200-new-name.conf'
            stale.write_text('old', encoding='utf-8')

            with patch.object(Path, 'home', return_value=home), patch.object(
                workspace_ssh.os, 'replace', side_effect=OSError('replace failed')
            ):
                with self.assertRaisesRegex(models.AppError, 'replace failed'):
                    workspace_ssh.write_local_ssh_config(
                        test_config(), 200, 'new-name', '192.0.2.200'
                    )

            self.assertTrue(stale.exists())
            self.assertFalse(target.exists())
            self.assertFalse(
                any(path.name.startswith(f'.{target.name}.') for path in directory.iterdir())
            )

    def test_remove_local_ssh_config_removes_only_matching_vmid(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            directory = home / '.ssh/config.d/homestack'
            directory.mkdir(parents=True)
            current = directory / 'vm210-test210.conf'
            stale = directory / 'vm210-oldname.conf'
            other_vm = directory / 'vm201-keep.conf'
            main_config = home / '.ssh/config'
            for path in (current, stale, other_vm, main_config):
                path.write_text(path.name, encoding='utf-8')

            with patch.object(Path, 'home', return_value=home):
                removed = workspace_ssh.remove_local_ssh_config(210)

            self.assertEqual(removed, [stale, current])
            self.assertFalse(current.exists())
            self.assertFalse(stale.exists())
            self.assertTrue(other_vm.exists())
            self.assertTrue(main_config.exists())

    def test_create_failure_does_not_generate_ssh_config(self) -> None:
        plan = {'vmid': 200, 'name': 'test1', 'node': 'example-node-1', 'ip': '192.0.2.200', 'home_label': 'HS_HOME_200', 'home_size_gib': 20, 'root_storage': 'example-storage-a', 'stale_snippets': []}
        with patch.object(lifecycle, 'get_workspace_authorized_keys', return_value=('ssh-ed25519 AAAATEST test\n', 'test')), patch.object(lifecycle, 'clone_full', side_effect=models.AppError('clone failed')), patch.object(lifecycle, 'write_local_ssh_config') as write_config:
            with self.assertRaisesRegex(models.AppError, 'clone failed'):
                lifecycle.create_workspace(FakeSession(), test_config(), plan, json_mode=True)
        write_config.assert_not_called()

class KnownHostsTests(unittest.TestCase):

    def test_forget_local_ssh_host_removes_plain_and_bracketed_entries(self) -> None:
        calls: list[list[str]] = []

        def fake_run(cmd: list[str], *, check: bool=True):
            calls.append(cmd)
            if cmd[:2] == ['ssh-keygen', '-F']:
                return __import__('subprocess').CompletedProcess(cmd, 0, '# found\n', '')
            return __import__('subprocess').CompletedProcess(cmd, 0, '', '')
        with patch.object(shutil, 'which', return_value='/usr/bin/ssh-keygen'), patch.object(workspace_ssh, 'run_local', side_effect=fake_run):
            removed = workspace_ssh.forget_local_ssh_host('192.0.2.200')
        self.assertEqual(removed, ['192.0.2.200', '[192.0.2.200]:22'])
        self.assertIn(['ssh-keygen', '-R', '192.0.2.200'], calls)
        self.assertIn(['ssh-keygen', '-R', '[192.0.2.200]:22'], calls)

    def test_forget_local_ssh_host_is_noop_when_not_found(self) -> None:
        calls: list[list[str]] = []

        def fake_run(cmd: list[str], *, check: bool=True):
            calls.append(cmd)
            return __import__('subprocess').CompletedProcess(cmd, 1, '', '')
        with patch.object(shutil, 'which', return_value='/usr/bin/ssh-keygen'), patch.object(workspace_ssh, 'run_local', side_effect=fake_run):
            removed = workspace_ssh.forget_local_ssh_host('192.0.2.200')
        self.assertEqual(removed, [])
        self.assertFalse(any((cmd[1] == '-R' for cmd in calls)))


class GoldKeyRoutingTests(unittest.TestCase):

    def test_guest_key_fallback_runs_on_gold_owner_node(self) -> None:
        cfg = replace(test_config(), control_node='example-node-1')
        guest_result = {
            'exited': 1,
            'exitcode': 0,
            'out-data': 'ssh-ed25519 AAAATEST routed\n',
        }
        with patch.object(
            workspace_ssh,
            'cluster_vm_resource',
            return_value={'node': 'example-node-2', 'status': 'running'},
        ), patch.object(
            workspace_ssh, 'qm_config_on_node', return_value={}
        ) as qm_config, patch.object(
            workspace_ssh, 'qm_status_on_node', return_value='running'
        ) as qm_status, patch.object(
            workspace_ssh,
            'node_run',
            return_value=models.RemoteResult(0, ''),
        ) as node_run, patch.object(
            workspace_ssh, 'guest_exec_on_node', return_value=guest_result
        ) as guest_exec:
            keys, source = workspace_ssh.get_workspace_authorized_keys(object(), cfg)

        self.assertIn('AAAATEST', keys)
        self.assertIn('/home/user/.ssh/authorized_keys', source)
        self.assertEqual(qm_config.call_args.args[2], 'example-node-2')
        self.assertEqual(qm_status.call_args.args[2], 'example-node-2')
        self.assertEqual(node_run.call_args.args[2], 'example-node-2')
        self.assertEqual(guest_exec.call_args.args[2], 'example-node-2')
