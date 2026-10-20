"""Offline tests of the exact remote probe, with no real commands or files."""
import contextlib
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

SOURCE = compile((Path(__file__).parent.parent / 'src/tui/pangolin_discover.py').read_text(),
                 'pangolin_discover.py', 'exec')
SENTINEL = 'synthetic-secret-never-export'


class ProbeTests(unittest.TestCase):
    def probe(self, *, compose=True, enabled=True, running=True, publication='127.0.0.1',
              containers=1, docker_code=0, readable=True, config=None, native=None,
              mounts=None, inspect_code=0, sockets=True, image='fosrl/pangolin:synthetic',
              listing=None):
        calls = []
        if config is None:
            config = ('server:\n  integration_port: 4567\n  secret: ' + SENTINEL +
                      '\nflags:\n  enable_integration_api: ' + str(enabled).lower() + '\n')
        if mounts is None:
            mounts = [{'Source': '/srv/synthetic config', 'Destination': '/app/config'}]
        def run(args, **kwargs):
            calls.append(args)
            if args[:2] == ['docker', 'ps']:
                output = listing if listing is not None else '\n'.join(json.dumps({'ID': str(i + 1) * 64, 'Image': image}) for i in range(containers))
                return subprocess.CompletedProcess(args, docker_code, output.encode(), ('Permission denied ' + SENTINEL).encode())
            if args[:2] == ['docker', 'inspect']:
                values = [running, image, mounts, {'4567/tcp': [{'HostIp': publication, 'HostPort': '14567'}]} if publication else {},
                          'synthetic-project' if compose else None, '/srv/synthetic config/compose.yml' if compose else None, 12345]
                return subprocess.CompletedProcess(args, inspect_code, '\n'.join(map(json.dumps, values)).encode(), ('Permission denied ' + SENTINEL).encode())
            if args[:2] == ['systemctl', 'list-unit-files']:
                return subprocess.CompletedProcess(args, 0, b'pangolin.service enabled' if native else b'', b'')
            if args[:2] == ['systemctl', 'show']:
                return subprocess.CompletedProcess(args, 0, ('ExecStart={ path=' + native + ' ; }\nActiveState=active\nFragmentPath=/etc/systemd/system/pangolin.service').encode(), b'')
            raise AssertionError('Unexpected command: ' + repr(args))
        def open_file(path, **kwargs):
            if path.startswith('/proc/'):
                if not sockets:
                    raise PermissionError(SENTINEL)
                return io.StringIO('header\n0: 00000000:11D7 00000000:0000 0A\n')
            if path in ('/srv/synthetic config/config.yml', "/srv/quote'$(false)/config.yml"):
                if not readable:
                    raise PermissionError(SENTINEL)
                return io.StringIO(config)
            raise AssertionError('Unexpected file: ' + path)
        output = io.StringIO()
        with patch('subprocess.run', run), patch('builtins.open', open_file), patch('os.path.isfile', lambda p: p.endswith('/config.yml')), contextlib.redirect_stdout(output):
            exec(SOURCE, {'__name__': '__main__'})
        self.assertNotIn(SENTINEL, output.getvalue())
        self.assertTrue(all(c[0] in ('docker', 'systemctl') for c in calls))
        self.assertFalse(any(c[1] in ('exec', 'restart', 'stop', 'start') for c in calls))
        return json.loads(output.getvalue()), calls

    def test_compose_running_enabled_nondefault_port_and_loopback(self):
        result, _ = self.probe()
        self.assertEqual(result['outcome'], 'Discovered')
        self.assertEqual(result['facts']['installation'], 'Docker Compose')
        self.assertEqual(result['facts']['api_port'], '4567')
        self.assertEqual(result['facts']['api_publication'], 'Loopback only')
        self.assertEqual(result['facts']['api_listening'], 'Yes')
        self.assertNotIn('version', result['facts'])

    def test_plain_docker(self):
        result, _ = self.probe(compose=False)
        self.assertEqual(result['facts']['installation'], 'Docker')

    def test_disabled_api_is_complete_even_without_explicit_default_port(self):
        result, _ = self.probe(config='flags:\n  enable_integration_api: false\n')
        self.assertEqual(result['outcome'], 'Discovered')
        self.assertNotIn('api_port', result['facts'])

    def test_stopped(self):
        result, _ = self.probe(running=False)
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertEqual(result['facts']['state'], 'Stopped')
        self.assertIn('stopped', result['issues'])

    def test_missing_installation(self):
        result, _ = self.probe(containers=0)
        self.assertEqual(result['outcome'], 'Not found')

    def test_docker_presence_does_not_prove_pangolin(self):
        result, _ = self.probe(image='unrelated/web:synthetic')
        self.assertEqual(result['outcome'], 'Not found')

    def test_multiple_installations(self):
        result, calls = self.probe(containers=2)
        self.assertEqual(result['outcome'], 'Unsupported installation')
        self.assertFalse(any(c[1] == 'inspect' for c in calls))

    def test_docker_permissions(self):
        result, _ = self.probe(docker_code=1)
        self.assertEqual(result['outcome'], 'Permission denied')
        self.assertIn('docker_permission', result['issues'])

    def test_inspect_permission_failure(self):
        result, _ = self.probe(inspect_code=1)
        self.assertEqual(result['outcome'], 'Permission denied')

    def test_unreadable_config(self):
        result, _ = self.probe(readable=False)
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertIn('config_unreadable', result['issues'])

    def test_missing_mount(self):
        result, _ = self.probe(mounts=[])
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertIn('config_unknown', result['issues'])

    def test_unsupported_yaml_does_not_infer_api(self):
        result, _ = self.probe(config='flags: {enable_integration_api: true}\n')
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertNotIn('api_enabled', result['facts'])

    def test_ipv6_loopback(self):
        result, _ = self.probe(publication='::1')
        self.assertEqual(result['facts']['api_publication'], 'Loopback only')

    def test_external_publication(self):
        result, _ = self.probe(publication='0.0.0.0')
        self.assertEqual(result['facts']['api_publication'], 'Wider interface')

    def test_unpublished_port(self):
        result, _ = self.probe(publication='')
        self.assertEqual(result['facts']['api_publication'], 'Not published')

    def test_socket_permission_failure(self):
        result, _ = self.probe(sockets=False)
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertIn('sockets_unreadable', result['issues'])

    def test_native_systemd_identity_partial(self):
        result, _ = self.probe(containers=0, native='/usr/local/bin/pangolin')
        self.assertEqual(result['facts']['installation'], 'Native/systemd')
        self.assertEqual(result['outcome'], 'Partially discovered')

    def test_unknown_service_executable(self):
        result, _ = self.probe(containers=0, native='/usr/bin/node')
        self.assertEqual(result['outcome'], 'Unsupported installation')

    def test_remote_paths_are_data_not_commands(self):
        result, calls = self.probe(mounts=[{'Source': "/srv/quote'$(false)", 'Destination': '/app/config'}])
        self.assertEqual(result['facts']['config'], "/srv/quote'$(false)/config.yml")
        self.assertFalse(any('$(false)' in arg for c in calls for arg in c))

    def test_unexpected_listing(self):
        result, _ = self.probe(listing='malformed')
        self.assertEqual(result['outcome'], 'Not found')
        self.assertIn('docker_listing_invalid', result['issues'])

    def test_duplicate_sensitive_settings_cannot_complete(self):
        result, _ = self.probe(config='flags:\n  enable_integration_api: true\n  enable_integration_api: false\n')
        self.assertEqual(result['outcome'], 'Partially discovered')

    def test_duplicate_root_sections_are_not_guessed(self):
        result, _ = self.probe(config='flags:\n  enable_integration_api: false\nflags: {enable_integration_api: true}\n')
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertNotIn('api_enabled', result['facts'])

    def test_yaml_anchors_are_not_interpreted(self):
        result, _ = self.probe(config='flags:\n  <<: *defaults\n  enable_integration_api: false\n')
        self.assertEqual(result['outcome'], 'Partially discovered')

    def test_block_scalar_cannot_spoof_configuration_sections(self):
        result, _ = self.probe(config='message: |\n flags:\n  enable_integration_api: false\n')
        self.assertEqual(result['outcome'], 'Partially discovered')
        self.assertNotIn('api_enabled', result['facts'])


if __name__ == '__main__':
    unittest.main()
