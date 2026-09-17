import unittest
from unittest.mock import patch

from homestack import resize
from homestack.models import AppError
from support import test_config


class ResizeTests(unittest.TestCase):
    def setUp(self):
        self.cfg = test_config()
        self.vm = {
            'tags': 'homestack-ws;homestack-lock', 'digest': 'a' * 40,
            self.cfg.root_disk: 'local-lvm:root,size=16G',
            self.cfg.home_disk: 'local-lvm:home,size=500G,serial=HS_HOME_207',
        }
        self.info = dict(vm_config=self.vm, status='running', node='pve3', name='gpu', home_label='HS_HOME_207')
        self.guest = dict(device='/dev/sda1', parent='/dev/sda', partition=1, size_bytes=16 * 1024**3)
        self.resolve = self.enterContext(patch.object(resize, 'resolve_existing_workspace', return_value=self.info))
        self.inspect = self.enterContext(patch.object(resize, '_inspect_guest', return_value=self.guest))
        self.run = self.enterContext(patch.object(resize, 'node_run'))
        self.execute_guest = self.enterContext(patch.object(resize, 'guest_out_on_node'))
        self.config = self.enterContext(patch.object(resize, 'qm_config_on_node', return_value={**self.vm, self.cfg.root_disk: 'local-lvm:root,size=32G'}))

    def plan(self, size='32G'):
        return resize.build_resize_plan(object(), self.cfg, 207, root_size=size)

    def test_lock_allows_resize_and_routes_to_current_node(self):
        plan = self.plan()
        result = resize.resize_workspace(object(), self.cfg, plan)
        self.assertTrue(result['ok'])
        self.assertEqual(self.run.call_args.args[2], 'pve3')
        self.assertIn(f'qm disk resize 207 {self.cfg.root_disk} 32G --digest', self.run.call_args.args[3])
        self.assertIn('growpart /dev/sda 1', self.execute_guest.call_args.args[4])
        self.assertIn('resize2fs /dev/sda1', self.execute_guest.call_args.args[4])

    def test_shrink_rejected_before_mutations_and_guest_inspection(self):
        with self.assertRaisesRegex(AppError, 'Shrinking'):
            self.plan('8G')
        self.inspect.assert_not_called()
        self.run.assert_not_called()
        self.execute_guest.assert_not_called()

    def test_equal_size_retries_filesystem_without_disk_resize(self):
        self.config.return_value = self.vm
        resize.resize_workspace(object(), self.cfg, self.plan('16G'))
        self.run.assert_not_called()
        self.execute_guest.assert_called_once()

    def test_home_selects_home_disk_and_skips_partition_growth(self):
        self.guest.update(device='/dev/sdb', parent='/dev/sdb', partition=None, size_bytes=500 * 1024**3)
        self.config.return_value = {**self.vm, self.cfg.home_disk: 'local-lvm:home,size=1T'}
        plan = resize.build_resize_plan(object(), self.cfg, 207, home_size='1T')
        resize.resize_workspace(object(), self.cfg, plan)
        self.assertIn(f'qm disk resize 207 {self.cfg.home_disk} 1T', self.run.call_args.args[3])
        self.assertNotIn('growpart', self.execute_guest.call_args.args[4])

    def test_revalidation_rejects_volume_or_size_drift(self):
        for value in ('local-lvm:other,size=16G', 'local-lvm:root,size=64G'):
            with self.subTest(value=value):
                self.vm[self.cfg.root_disk] = 'local-lvm:root,size=16G'
                plan = self.plan()
                self.vm[self.cfg.root_disk] = value
                with self.assertRaises(AppError):
                    resize.resize_workspace(object(), self.cfg, plan)
                self.run.assert_not_called()

    def test_filesystem_failure_explains_same_size_retry(self):
        self.execute_guest.side_effect = AppError('resize2fs failed')
        with self.assertRaisesRegex(AppError, 'Re-run resize 207 --root-size 32G'):
            resize.resize_workspace(object(), self.cfg, self.plan())
        self.run.assert_called_once()

    def test_guest_preflight_failure_prevents_host_growth(self):
        self.inspect.side_effect = AppError('Unsupported filesystem')
        with self.assertRaisesRegex(AppError, 'Unsupported filesystem'):
            self.plan()
        self.run.assert_not_called()
