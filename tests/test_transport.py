from __future__ import annotations

import subprocess
import unittest

from homestack import models, proxmox
from homestack.transports import base as transports_base
from homestack.transports import herdr as transports_herdr

from support import test_config

class TransportTests(unittest.TestCase):
    def test_transport_protocol_accepts_required_operations(self) -> None:
        class CompleteTransport:
            cfg = object()

            def run(self, command: str, **kwargs: object) -> models.RemoteResult:
                return models.RemoteResult(0, '')

            def run_with_progress(self, command: str, on_output: object, **kwargs: object) -> models.RemoteResult:
                return models.RemoteResult(0, '')

            def run_json_value(self, command: str, **kwargs: object) -> object:
                return {}

            def run_json(self, command: str, **kwargs: object) -> dict[str, object]:
                return {}

            def execution_info(self) -> dict[str, object]:
                return {'type': 'example'}

        self.assertIsInstance(CompleteTransport(), transports_base.Transport)

    def test_remote_wrapper_does_not_echo_completion_marker(self) -> None:
        token = 'abc123'
        wrapped = transports_herdr.wrap_remote_command('command -v base64 >/dev/null 2>&1', token)
        self.assertIn('hs_t=abc123', wrapped)
        self.assertNotIn('__HS_END__abc123:', wrapped)
        self.assertNotIn('__HS_BEGIN__abc123', wrapped)
        self.assertIn("printf '__HS_BEGIN__%s", wrapped)
        self.assertIn("printf '\\n__HS_END__%s:%d", wrapped)

    def test_remote_envelope_parser_tolerates_indented_end_marker(self) -> None:
        token = 'abc123'
        text = 'noise before\n__HS_BEGIN__abc123\n{"name":"gold","tags":"homestack-gold"}\n                                                __HS_END__abc123:0\nroot@example-node-1:~# '
        result = transports_herdr.parse_remote_envelope(text, token)
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.output, '{"name":"gold","tags":"homestack-gold"}')

    def test_remote_envelope_parser_uses_latest_begin_marker(self) -> None:
        token = 'abc123'
        text = '__HS_BEGIN__abc123\nold\n__HS_END__abc123:0\n__HS_BEGIN__abc123\nnew\n__HS_END__abc123:7\n'
        result = transports_herdr.parse_remote_envelope(text, token)
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.output, 'new')

    def test_remote_exit_is_isolated_and_still_emits_completion_marker(self) -> None:
        token = 'exit-isolated'
        wrapped = transports_herdr.wrap_remote_command('exit 81', token, timeout=5)
        proc = subprocess.run(
            ['/bin/sh', '-c', wrapped],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(proc.returncode, 0)
        result = transports_herdr.parse_remote_envelope(proc.stdout, token)
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.returncode, 81)

    def test_remote_timeout_is_applied_to_the_child_command(self) -> None:
        wrapped = transports_herdr.wrap_remote_command('sleep 30', 'bounded', timeout=2)
        self.assertIn('timeout --foreground -k 5s 2s /bin/sh -c', wrapped)

    def test_node_shell_command(self) -> None:
        cfg = test_config()
        self.assertEqual(proxmox.node_shell_command(cfg, 'example-node-1', 'qm status 200'), 'qm status 200')
        self.assertEqual(proxmox.node_shell_command(cfg, 'example-node-2', 'qm status 200'), "ssh -o BatchMode=yes root@example-node-2 'qm status 200'")
        quoted = proxmox.node_shell_command(cfg, 'example-node-2', "qm guest exec 200 -- /bin/sh -lc 'echo ok && id -u'")
        self.assertNotIn('-- sh -lc', quoted)
        self.assertIn('qm guest exec 200', quoted)
