"""Detached lifecycle tests: local fake API, fake ELF CLI, synthetic credentials."""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import test_harness as base
from connector import PersistentConnector

h = base.h

FAKE_CLI = r'''
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 5 || strcmp(argv[1], "up") || strcmp(argv[2], "site") || strcmp(argv[3], "--config-file")) return 2;
    signal(SIGXFSZ, SIG_IGN);
    char path[4096];
    snprintf(path, sizeof(path), "%s.write-probe", argv[4]);
    int fd = open(path, O_WRONLY | O_CREAT, 0600);
    if (fd < 0 || write(fd, "x", 1) >= 0 || errno != EFBIG) return 3;
    close(fd);
    while (1) pause();
}
'''


class LifecycleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        base.HarnessTests.setUpClass()
        cls.compiler_dir = tempfile.TemporaryDirectory(prefix='pangolin-fake-cli-')
        cls.binary = Path(cls.compiler_dir.name) / 'pangolin'
        compiler = shutil.which('cc')
        if compiler is None:
            raise RuntimeError('A C compiler is required for the fake ELF process ownership tests.')
        subprocess.run([compiler, '-x', 'c', '-', '-o', str(cls.binary)], input=FAKE_CLI,
                       text=True, capture_output=True, check=True)

    @classmethod
    def tearDownClass(cls):
        cls.compiler_dir.cleanup()
        base.HarnessTests.tearDownClass()

    def setUp(self):
        self.fixture = base.HarnessTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.run_experiment()
        self.work = self.fixture.work
        self.experiment = self.fixture.experiment
        self.fake = self.fixture.fake
        self.fake.calls.clear()
        self.runtime = PersistentConnector(self.work, self.experiment.state['run'])
        self.addCleanup(self.stop_owned)

    def stop_owned(self):
        if self.work.fd is not None:
            self.runtime.stop()

    def detached_start(self):
        self.work.__exit__(None, None, None)
        code = '''import sys,json
sys.path.insert(0,sys.argv[1])
from harness import Workspace
from connector import PersistentConnector
with Workspace(sys.argv[2]) as work:
    runtime=PersistentConnector(work,work.load("state.json")["run"])
    print(json.dumps(runtime.start(sys.argv[3],work.load("site-credentials.json"))))
'''
        try:
            result = subprocess.run([sys.executable, '-c', code, str(Path(__file__).parent),
                str(self.work.path), str(self.binary)], text=True, capture_output=True, timeout=10)
        finally:
            self.work.__enter__()
        diagnostic = result.stderr
        for value in self.work.load('site-credentials.json').values():
            diagnostic = diagnostic.replace(value, '<redacted>')
        self.assertEqual(result.returncode, 0, diagnostic)
        return json.loads(result.stdout)

    def test_connector_survives_start_command_and_status_is_current(self):
        started = self.detached_start()
        self.assertEqual(self.runtime.running()['pid'], started['pid'])
        report = self.experiment.status_connector(probe=self.fixture.probe, interval=0)
        self.assertTrue(report['start_command_returned'])
        self.assertTrue(report['healthy_after_start_return'])
        self.assertTrue(report['public_tls']['verified'])
        self.fake.sites[report['site_id']]['online'] = False
        report = self.experiment.status_connector(probe=self.fixture.probe, interval=0)
        self.assertFalse(report['site_online'])
        self.assertTrue(report['target_healthy'])
        self.assertFalse(report['resource_healthy'])
        self.assertFalse(report['healthy_after_start_return'])
        self.assertTrue(all(method == 'GET' for method, _, _ in self.fake.calls))
        self.assertFalse(any('/domains' in path or path.startswith('/org/') for _, path, _ in self.fake.calls))

    def test_repeated_start_reuses_process_without_discovery_or_download(self):
        started = self.detached_start()
        with patch.object(h, 'discover_backend', side_effect=AssertionError('No discovery')):
            report = self.experiment.start_connector(locate_cli=lambda _: self.fail('No download'),
                probe=self.fixture.probe, wait_seconds=0, interval=0)
        self.assertEqual(report['pid'], started['pid'])
        self.assertTrue(report['reused'])
        self.assertFalse(report['healthy_after_start_return'])
        self.assertTrue(all(method == 'GET' for method, _, _ in self.fake.calls))

    def test_online_site_without_owned_process_refuses_start(self):
        with self.assertRaisesRegex(h.Failure, 'already Online'):
            self.experiment.start_connector(locate_cli=lambda _: self.fail('No launch/download'),
                probe=self.fixture.probe, wait_seconds=0, interval=0)
        self.assertIsNone(self.runtime.running())

    def test_unsafe_lock_permissions_refuse_start(self):
        lock = self.work.path / 'connector.lock'
        lock.touch(mode=0o600)
        lock.chmod(0o644)
        try:
            with self.assertRaisesRegex(h.DnsFailure, 'private regular file'):
                self.runtime.start(self.binary, self.work.load('site-credentials.json'))
        finally:
            lock.chmod(0o600)

    def test_stop_only_owned_process_is_idempotent_and_preserves_credentials(self):
        started = self.detached_start()
        foreign = subprocess.Popen(['sleep', '30'])
        try:
            self.assertEqual(self.runtime.stop()['pid'], started['pid'])
            self.assertIsNone(self.runtime.running())
            self.assertTrue(self.runtime.stop()['already_stopped'])
            self.assertIsNone(foreign.poll())
            self.assertTrue((self.work.path / 'connector-site.json').exists())
            self.assertTrue((self.work.path / 'site-credentials.json').exists())
            self.assertFalse(self.fake.calls)
        finally:
            foreign.terminate()
            foreign.wait()

    def test_reused_pid_or_changed_executable_never_receives_a_signal(self):
        self.detached_start()
        original = self.work.load('connector.json')
        try:
            for changed in ({'start_ticks': str(int(original['start_ticks']) + 1)}, {'binary': '/bin/sleep'}):
                self.work.save('connector.json', {**original, **changed})
                with patch('connector.signal.pidfd_send_signal') as send:
                    with self.assertRaises(h.DnsFailure):
                        self.runtime.stop()
                    send.assert_not_called()
        finally:
            self.work.save('connector.json', original)
        self.assertIsNotNone(self.runtime.running())

    def test_missing_ownership_record_with_held_lock_refuses_duplicate(self):
        self.detached_start()
        original = self.work.load('connector.json')
        (self.work.path / 'connector.json').unlink()
        try:
            with self.assertRaisesRegex(h.DnsFailure, 'without verifiable ownership'):
                self.runtime.start(self.binary, self.work.load('site-credentials.json'))
        finally:
            self.work.save('connector.json', original)

    def test_secure_config_no_secret_arguments_and_no_binary_replacement(self):
        started = self.detached_start()
        credentials = self.work.load('site-credentials.json')
        config = self.work.path / 'connector-site.json'
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE((self.work.path / 'connector.json').stat().st_mode), 0o600)
        self.assertEqual(json.loads(config.read_text())['secret'], credentials['secret'])
        argv = Path(f"/proc/{started['pid']}/cmdline").read_bytes()
        env = Path(f"/proc/{started['pid']}/environ").read_bytes()
        for value in credentials.values():
            self.assertNotIn(value.encode(), argv + env)
            self.assertNotIn(value, json.dumps(self.work.load('connector.json')))
        probe = Path(str(config) + '.write-probe')
        self.assertEqual(probe.stat().st_size, 0)
        self.assertIsNotNone(self.runtime.running())

    def test_status_requires_health_in_both_observations(self):
        self.detached_start()
        target = self.fake.targets[self.experiment.state['target_id']]
        with patch.object(h.time, 'sleep', side_effect=lambda _: target.update(hcHealth='unhealthy')):
            result = self.experiment.status_connector(probe=self.fixture.probe, interval=0)
        self.assertFalse(result['target_healthy'])
        self.assertFalse(result['healthy_after_start_return'])

    def test_public_tls_failure_is_separate_from_site_health(self):
        self.detached_start()
        def failed(*_):
            raise h.Failure('Health/routing probe failed: TLS handshake failed.')
        result = self.experiment.status_connector(probe=failed, interval=0)
        self.assertTrue(result['resource_healthy'])
        self.assertTrue(result['healthy_after_start_return'])
        self.assertFalse(result['public_tls']['verified'])

    def test_start_timeout_preserves_owned_process_for_status_and_stop(self):
        self.fake.site_online = False
        self.fake.sites[self.experiment.state['site_id']]['online'] = False
        with self.assertRaisesRegex(h.Failure, 'background'):
            self.experiment.start_connector(locate_cli=lambda _: self.binary,
                probe=self.fixture.probe, wait_seconds=0, interval=0)
        self.assertIsNotNone(self.runtime.running())
        result = self.experiment.status_connector(probe=self.fixture.probe, interval=0)
        self.assertFalse(result['healthy_after_start_return'])

    def test_provisioning_and_cleanup_refuse_a_running_connector(self):
        self.detached_start()
        with self.assertRaisesRegex(h.Failure, 'persistent connector'):
            self.fixture.run_experiment()
        with self.assertRaisesRegex(h.Failure, 'Stop the recorded'):
            self.experiment.cleanup()
        self.assertFalse(self.fake.calls)

    def test_stop_does_not_read_api_credentials_or_contact_api(self):
        self.detached_start()
        settings = self.work.path.parent / 'settings.toml'
        settings.write_text('api_url=' + json.dumps(self.fixture.endpoint) + '\norganization="example-org"\n'
            'server_url="https://dashboard.example.invalid"\nbackend_instance="backend"\n')
        settings.chmod(0o600)
        self.work.__exit__(None, None, None)
        try:
            with patch.object(h, 'Client', side_effect=AssertionError('No API client')):
                with contextlib.redirect_stdout(io.StringIO()):
                    code = h.main(['stop', '--settings-file', str(settings), '--state-dir', str(self.work.path),
                                   '--credential-file', '/nonexistent/synthetic-credential'])
            self.assertEqual(code, 0)
        finally:
            self.work.__enter__()
        self.assertIsNone(self.runtime.running())


if __name__ == '__main__':
    unittest.main()
