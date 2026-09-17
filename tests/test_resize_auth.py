import unittest
from types import SimpleNamespace
from unittest.mock import patch

from homestack import resize_auth as auth
from homestack.models import AppError
from support import test_config


class ResizeAuthenticationTests(unittest.TestCase):
    def setUp(self):
        self.plan = {'vmid': 207, 'node': 'pve3', 'ip': '192.0.2.207', 'guest': {
            'crypt': {'name': 'dm_crypt-0', 'device': '/dev/sda3', 'auth': 'passphrase', 'key_file': None},
        }}
        self.cfg = test_config()
        self.enterContext(patch.object(auth.sys.stdin, 'isatty', return_value=True))
        self.prompt = self.enterContext(patch.object(auth.getpass, 'getpass', return_value='Secret-Do-Not-Log'))
        self.boot = self.enterContext(patch.object(auth, 'guest_out_on_node', return_value='boot-id'))
        self.qga = self.enterContext(patch.object(auth, 'guest_exec_on_node', return_value={'exited': 1, 'exitcode': 0}))
        self.factory = self.enterContext(patch.object(auth.WorkspaceSSH, 'configured'))
        self.ssh = self.factory.return_value.__enter__.return_value
        self.ssh.run.return_value = SimpleNamespace(returncode=0, stdout='boot-id')

    def test_password_is_only_sent_on_stdin_after_identity_check(self):
        with auth.crypt_resize_access(object(), self.cfg, self.plan, interactive=True) as grow:
            grow()
        self.assertEqual(self.factory.call_args.args[0].user_name, 'root')
        calls = self.ssh.run.call_args_list
        self.assertEqual(len(calls), 3)
        self.assertEqual(calls[0].args[0], 'cat /proc/sys/kernel/random/boot_id')
        for call in calls[1:]:
            self.assertNotIn('Secret-Do-Not-Log', call.args[0])
            self.assertEqual(call.kwargs['input_text'], 'Secret-Do-Not-Log')
            self.assertIn('--key-file -', call.args[0])
        self.assertIn('--test-passphrase', calls[1].args[0])
        self.assertIn('resize dm_crypt-0', calls[2].args[0])
        self.qga.assert_not_called()

    def test_noninteractive_password_refuses_before_prompt_or_connection(self):
        with self.assertRaisesRegex(AppError, 'interactively'):
            with auth.crypt_resize_access(object(), self.cfg, self.plan):
                self.fail('must not yield')
        self.prompt.assert_not_called()
        self.factory.assert_not_called()

    def test_wrong_ssh_vm_does_not_receive_password(self):
        self.ssh.run.return_value.stdout = 'old-gpu'
        with self.assertRaisesRegex(AppError, 'not the VM'):
            with auth.crypt_resize_access(object(), self.cfg, self.plan, interactive=True):
                self.fail('must not yield')
        self.prompt.assert_not_called()
        self.ssh.run.assert_called_once()

    def test_wrong_password_aborts_before_resize(self):
        self.ssh.run.side_effect = [SimpleNamespace(stdout='boot-id'), SimpleNamespace(returncode=2)]
        with self.assertRaisesRegex(AppError, 'verification failed'):
            with auth.crypt_resize_access(object(), self.cfg, self.plan, interactive=True):
                self.fail('must not yield')
        self.assertEqual(self.ssh.run.call_count, 2)

    def test_keyfile_stays_in_guest_and_is_checked_before_resize(self):
        self.plan['guest']['crypt'].update(auth='keyfile', key_file='/etc/cryptsetup-keys.d/dm_crypt-0.key')
        with auth.crypt_resize_access(object(), self.cfg, self.plan) as grow:
            self.assertEqual(self.qga.call_count, 1)
            self.assertIn('--test-passphrase', self.qga.call_args.args[4])
            grow()
        self.assertEqual(self.qga.call_count, 2)
        self.assertIn('resize dm_crypt-0', self.qga.call_args.args[4])
        self.prompt.assert_not_called()
        self.factory.assert_not_called()

    def test_wrong_keyfile_aborts_before_yield(self):
        self.plan['guest']['crypt'].update(auth='keyfile', key_file='/key')
        self.qga.return_value = {'exited': 1, 'exitcode': 2}
        with self.assertRaisesRegex(AppError, 'authentication'):
            with auth.crypt_resize_access(object(), self.cfg, self.plan):
                self.fail('must not yield')
