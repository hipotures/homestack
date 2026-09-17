from contextlib import redirect_stdout
from io import StringIO
import json
import unittest
from unittest.mock import patch

from homestack.resize import _INSPECT


class GuestInspectionTests(unittest.TestCase):
    def inspect(self, *, role='root', filesystem='ext4', root_type='disk'):
        rows = [
            dict(name='/dev/sda', type=root_type, pkname=None, serial=None, size=16 * 1024**3),
            dict(name='/dev/sdb', type='disk', pkname=None, serial='HS_HOME_207', size=500 * 1024**3),
            dict(name='/dev/zram0', type='disk', pkname=None, serial=None, size=1024**3),
        ]
        mount = '/' if role == 'root' else '/home/user'
        def output(args, **kwargs):
            if args[0] == 'findmnt':
                return json.dumps({'filesystems': [dict(source='/dev/sda' if role == 'root' else '/dev/sdb', fstype=filesystem, target=mount, options='rw,relatime')]})
            if args[0] == 'lsblk':
                return json.dumps({'blockdevices': rows})
            if args[0] == 'blkid':
                return 'HS_HOME_207'
            raise AssertionError(args)
        stdout = StringIO()
        with patch('sys.argv', ['inspect', mount, role, 'HS_HOME_207']), patch('shutil.which', return_value='/bin/tool'), patch('subprocess.check_output', side_effect=output), redirect_stdout(stdout):
            exec(compile(_INSPECT, '<guest-inspect>', 'exec'), {})
        return json.loads(stdout.getvalue())

    def test_root_and_home_are_identified_with_zram_present(self):
        self.assertEqual(self.inspect()['device'], '/dev/sda')
        self.assertEqual(self.inspect(role='home')['device'], '/dev/sdb')

    def test_unsupported_filesystem_and_lvm_are_rejected(self):
        with self.assertRaisesRegex(SystemExit, 'ext4'):
            self.inspect(filesystem='xfs')
        with self.assertRaisesRegex(SystemExit, 'LVM'):
            self.inspect(root_type='lvm')
