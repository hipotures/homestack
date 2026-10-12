from contextlib import redirect_stdout
from io import StringIO
import json
import stat
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from homestack.resize_guest import INSPECT


class LayerInspectionTests(unittest.TestCase):
    def inspect(self, *, filesystem='ext4', key='none', extra_lv=False, multi_btrfs=False):
        crypt = '/dev/mapper/dm_crypt-0'
        lv = '/dev/mapper/ubuntu--vg-ubuntu--lv'
        rows = []
        def row(name, kind, parent, number, serial=None):
            rows.append(dict(name=name, type=kind, pkname=parent, serial=serial, size=16 * 1024**3, **{'maj:min': number}))
        row('/dev/sda', 'disk', None, '8:0')
        row('/dev/sda3', 'part', '/dev/sda', '8:3')
        row(crypt, 'crypt', '/dev/sda3', '253:0')
        if filesystem == 'ext4':
            row(lv, 'lvm', crypt, '253:1')
        row('/dev/sdb', 'disk', None, '8:16', 'HS_HOME_207')
        aliases = {crypt: '/dev/dm-0', lv: '/dev/dm-1', '/dev/ubuntu-vg/ubuntu-lv': '/dev/dm-1'}
        def realpath(path, **kwargs):
            return aliases.get(path, path)
        def output(args, **kwargs):
            if args[0] == 'findmnt':
                return json.dumps({'filesystems': [{'source': lv if filesystem == 'ext4' else crypt + '[/@]', 'fstype': filesystem, 'target': '/', 'options': 'rw', 'maj:min': '253:1' if filesystem == 'ext4' else '0:35'}]})
            if args[0] == 'lsblk':
                return json.dumps({'blockdevices': rows})
            if args[0] == 'blkid':
                return {'UUID': 'uuid-root', 'PARTUUID': 'part-id', 'LABEL': '', 'TYPE': 'crypto_LUKS'}[args[2]]
            if args[0] == 'btrfs':
                return f'Label: none uuid: btrfs-uuid\n\tTotal devices {2 if multi_btrfs else 1} FS bytes used 123\n\tdevid 3 size 12345 used 100 path {crypt}\n'
            if args[0] == 'lvs':
                item = dict(lv_path='/dev/ubuntu-vg/ubuntu-lv', lv_name='ubuntu-lv', vg_name='ubuntu-vg', lv_size=str(10*1024**3), lv_attr='-wi-ao----', segtype='linear')
                return json.dumps({'report': [{'lv': [item, dict(item, lv_path='/dev/ubuntu-vg/other')] if extra_lv else [item]}]})
            if args[0] == 'vgs':
                return json.dumps({'report': [{'vg': [dict(vg_name='ubuntu-vg', vg_extent_size='4194304')]}]})
            if args[0] == 'pvs':
                return json.dumps({'report': [{'pv': [dict(vg_name='ubuntu-vg', pv_name=crypt)]}]})
            raise AssertionError(args)
        files = {'/etc/crypttab': f'dm_crypt-0 UUID=uuid-root {key} luks\n', '/sys/class/block/sda3/partition': '3', '/sys/class/block/sda3/start': '3774873', '/sys/class/block/dm-0/dm/name': 'dm_crypt-0'}
        def read(path, *args, **kwargs):
            if path not in files:
                raise FileNotFoundError(path)
            return StringIO(files[path])
        stdout = StringIO()
        with patch('sys.argv', ['inspect', '/', 'root', 'HS_HOME_207']), patch('shutil.which', return_value='/bin/tool'), patch('subprocess.check_output', side_effect=output), patch('os.path.realpath', side_effect=realpath), patch('os.listdir', return_value=[]), patch('builtins.open', side_effect=read), patch('os.stat', return_value=SimpleNamespace(st_mode=stat.S_IFREG | 0o600)), redirect_stdout(stdout):
            exec(compile(INSPECT, '<inspect>', 'exec'), {})
        return json.loads(stdout.getvalue())

    def test_gold_luks_lvm_ext4_detects_file_key(self):
        result = self.inspect(key='/etc/cryptsetup-keys.d/dm_crypt-0.key')
        self.assertEqual(result['crypt']['auth'], 'keyfile')
        self.assertEqual(result['crypt']['device'], '/dev/sda3')
        self.assertEqual(result['lvm']['lv_path'], '/dev/ubuntu-vg/ubuntu-lv')
        self.assertEqual(result['partition'], 3)
        self.assertEqual(result['parent'], '/dev/sda')

    def test_no_key_requires_passphrase(self):
        result = self.inspect()
        self.assertEqual(result['crypt']['auth'], 'passphrase')
        self.assertIsNone(result['crypt']['key_file'])

    def test_escaped_keyfile_path_is_decoded(self):
        result = self.inspect(key=r'/etc/cryptsetup-keys.d/root\040key')
        self.assertEqual(result['crypt']['key_file'], '/etc/cryptsetup-keys.d/root key')

    def test_btrfs_subvolume_with_synthetic_device_number(self):
        result = self.inspect(filesystem='btrfs')
        self.assertEqual(result['filesystem'], 'btrfs')
        self.assertEqual(result['btrfs_devid'], 3)
        self.assertIsNone(result['lvm'])
        self.assertEqual(result['device'], '/dev/mapper/dm_crypt-0')

    def test_shared_vg_and_multidevice_btrfs_are_rejected(self):
        with self.assertRaisesRegex(SystemExit, 'one logical volume'):
            self.inspect(extra_lv=True)
        with self.assertRaisesRegex(SystemExit, 'single-device'):
            self.inspect(filesystem='btrfs', multi_btrfs=True)
