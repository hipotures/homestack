"""Authenticate encrypted resize without placing credentials in command arguments."""
from contextlib import contextmanager
from dataclasses import replace
import getpass
import shlex
import sys
import warnings

from .guest import guest_exec_on_node, guest_out_on_node
from .models import AppError
from .workspace_ssh import WorkspaceSSH


def _qga_command(session, cfg, node, vmid, argv):
    result = guest_exec_on_node(session, cfg, node, vmid, shlex.join(argv), check=False)
    if not result.get('exited') or result.get('exitcode') != 0:
        raise AppError('LUKS authentication or resize failed; verify the configured key file in the VM')


@contextmanager
def crypt_resize_access(session, cfg, plan, *, interactive=False):
    """Validate credentials before disk growth; yield the authenticated resize action."""
    crypt = plan['guest'].get('crypt')
    if not crypt:
        yield None
        return
    base = ['cryptsetup', '--batch-mode']
    test = ['open', '--type', 'luks', '--test-passphrase', crypt['device']]
    grow = ['resize', crypt['name']]
    if crypt['auth'] == 'keyfile':
        args = base + ['--key-file', crypt['key_file']]
        _qga_command(session, cfg, plan['node'], plan['vmid'], args + test)
        yield lambda: _qga_command(session, cfg, plan['node'], plan['vmid'], args + grow)
        return
    if not interactive or not sys.stdin.isatty():
        raise AppError('LUKS requires a passphrase. Run resize interactively without --json; no disk changes made')
    boot_id = guest_out_on_node(session, cfg, plan['node'], plan['vmid'], 'cat /proc/sys/kernel/random/boot_id').strip()
    target = {'vmid': plan['vmid'], 'ip': plan['ip']}
    with WorkspaceSSH.configured(replace(cfg, user_name='root'), target) as ssh:
        actual = ssh.run('cat /proc/sys/kernel/random/boot_id').stdout.strip()
        if not boot_id or actual != boot_id:
            raise AppError('SSH target is not the VM inspected through QEMU Guest Agent; no disk changes made')
        try:
            with warnings.catch_warnings():
                warnings.simplefilter('error', getpass.GetPassWarning)
                password = getpass.getpass(f'LUKS passphrase for VM {plan["vmid"]}: ', stream=sys.stderr)
        except (EOFError, getpass.GetPassWarning) as exc:
            raise AppError('Cannot read a hidden LUKS passphrase; no disk changes made') from exc
        if not password:
            raise AppError('Empty LUKS passphrase; no disk changes made')
        args = base + ['--key-file', '-']
        try:
            result = ssh.run(shlex.join(args + test), input_text=password, check=False)
            if result.returncode:
                raise AppError('LUKS passphrase verification failed; no disk changes made')
            def grow_with_password():
                result = ssh.run(shlex.join(args + grow), input_text=password, check=False)
                if result.returncode:
                    raise AppError('LUKS resize failed; guest command output withheld')
            yield grow_with_password
        finally:
            password = None
