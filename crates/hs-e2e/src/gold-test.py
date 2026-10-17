"""The provisioning script must reject unrelated guests before any mutation."""
import importlib.util
import io
import json
import pathlib
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('gold_create', pathlib.Path(sys.argv.pop(1)))
gold = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gold)


class Ownership(unittest.TestCase):
    def exercise(self, marker, container=False, vm=False):
        calls = []
        def run(*args):
            calls.append(args)
            if args == ('hostname', '-s'):
                return 'pve1'
            if args[:2] == ('pvesh', 'get') and args[2].endswith('/config'):
                return json.dumps({'description': 'another-run'})
            raise AssertionError('Unrelated guest reached a mutation: ' + repr(args))
        with patch('builtins.open', return_value=io.StringIO(json.dumps({'run_id': marker}))), \
             patch.object(gold, 'run', side_effect=run), patch.object(gold.pathlib, 'Path') as paths:
            def path(name):
                from unittest.mock import Mock
                result = Mock()
                result.is_dir.return_value = True
                result.exists.return_value = (container and name == '/etc/pve/lxc/100.conf') or (vm and name == '/etc/pve/qemu-server/100.conf')
                return result
            paths.side_effect = path
            with self.assertRaises((AssertionError, RuntimeError)):
                gold.create({'run_id': 'this-run', 'owner': 'this-run:gold'})
        return calls

    def test_wrong_run_never_calls_proxmox(self):
        self.assertEqual(self.exercise('another-run'), [])

    def test_existing_container_is_never_replaced(self):
        self.assertEqual(self.exercise('this-run', container=True), [])

    def test_existing_unowned_vm_is_never_modified(self):
        calls = self.exercise('this-run', vm=True)
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], ('hostname', '-s'))
        self.assertEqual(calls[1][0:2], ('pvesh', 'get'))


if __name__ == '__main__':
    unittest.main()
