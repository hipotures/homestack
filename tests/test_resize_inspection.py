from contextlib import redirect_stdout
from io import StringIO
import json
import unittest
from unittest.mock import patch

from homestack.resize import _INSPECT


class GuestInspectionTests(unittest.TestCase):
    def inspect(self, *, role='root', filesystem='ext4', root_type='disk', source=None,
                maj_min=None, partition=False):
        rows = [
            dict(name='/dev/sda', type=root_type, pkname=None, serial=None, size=16 * 1024**3, **{'maj:min': '8:0'}),
            dict(name='/dev/sdb', type='disk', pkname=None, serial='HS_HOME_207', size=500 * 1024**3, **{'maj:min': '8:16'}),
            dict(name='/dev/zram0', type='disk', pkname=None, serial=None, size=1024**3, **{'maj:min': '252:0'}),
        ]
        if partition:
            rows.insert(1, dict(name='/dev/sda1', type='part', pkname='/dev/sda', serial=None,
                                size=16 * 1024**3, **{'maj:min': '8:1'}))
        mount = '/' if role == 'root' else '/home/user'
        def output(args, **kwargs):
            if args[0] == 'findmnt':
                return json.dumps({'filesystems': [dict(
                    source=source or ('/dev/sda' if role == 'root' else '/dev/sdb'),
                    fstype=filesystem,
                    target=mount,
                    options='rw,relatime',
                    **{'maj:min': maj_min or ('8:0' if role == 'root' else '8:16')},
                )]})
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

    def test_root_source_alias_maps_to_disk_by_major_minor(self):
        self.assertEqual(self.inspect(source='/dev/root')['device'], '/dev/sda')

    def test_root_partition_source_alias_maps_by_major_minor(self):
        values = {
            '/sys/class/block/sda1/partition': '1',
            '/sys/class/block/sda1/start': '2048',
        }
        def sysfs(path, *args, **kwargs):
            return StringIO(values[path])
        with patch('builtins.open', side_effect=sysfs):
            result = self.inspect(source='/dev/root', maj_min='8:1', partition=True)
        self.assertEqual(result['device'], '/dev/sda1')
        self.assertEqual(result['parent'], '/dev/sda')
        self.assertEqual(result['partition'], 1)

    def test_unknown_major_minor_is_rejected(self):
        with self.assertRaisesRegex(SystemExit, 'Cannot identify mounted block device'):
            self.inspect(maj_min='8:99')

    def test_unsupported_filesystem_and_unresolved_lvm_are_rejected(self):
        with self.assertRaisesRegex(SystemExit, 'ext4'):
            self.inspect(filesystem='xfs')
        with self.assertRaisesRegex(SystemExit, 'Cannot identify parent'):
            self.inspect(root_type='lvm')
