from __future__ import annotations

from dataclasses import replace
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from textual.widgets import Button, Input, Static
from homestack import setup_catalog as catalog, setup_config as definitions
from homestack.setup_tui import SetupApp, SetupTree, Review
from support import test_config

TARGET = {'name': 'workspace', 'vmid': 200, 'ip': '192.0.2.200'}


class TestApp(SetupApp):
    def action_refresh_catalog(self):
        pass

    def save_displayed_catalog(self, catalog):
        pass


class SetupTUITests(unittest.IsolatedAsyncioTestCase):
    async def test_keyboard_parent_states_filter_scope_review_back_and_cancel(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(100, 40)) as pilot:
            tree = app.query_one(SetupTree)
            self.assertFalse(app.selected)
            tree.select_node(app.nodes['bash'])
            await pilot.press('space')
            self.assertEqual(app.selected, {'bash'})
            self.assertEqual(app.checkbox('env'), '[-]')
            tree.select_node(app.nodes['env'])
            await pilot.press('0')
            self.assertEqual(app.checkbox('env'), '[x]')
            await pilot.press('space')
            self.assertFalse(app.selected)
            app.query_one(Input).value = 'bash'
            await pilot.pause()
            tree.select_node(app.nodes['env'])
            tree.focus()
            await pilot.press('space')
            self.assertEqual(app.selected, {'bash'})
            app.query_one(Input).value = 'codex'
            await pilot.pause()
            tree.select_node(app.nodes['codex'])
            await pilot.press('space')
            self.assertEqual(app.selected, {'bash', 'codex'})
            self.assertIn('Hidden selected: 1', str(app.query_one('#counts', Static).render()))
            app.review()
            await pilot.pause()
            self.assertIsInstance(app.screen, Review)
            self.assertIn('[hidden by filter]', app.screen.text)
            await pilot.press('escape')
            self.assertEqual(app.selected, {'bash', 'codex'})
            self.assertFalse(app.busy)
            app.query_one(Input).value = ''
            await pilot.pause()
            tree.select_node(app.nodes['env'])
            tree.focus()
            await pilot.press('left')
            self.assertFalse(app.nodes['env'].is_expanded)
            await pilot.press('right')
            self.assertTrue(app.nodes['env'].is_expanded)
            await pilot.press('enter')
            self.assertIsInstance(app.screen, Review)
            self.assertFalse(app.screen.apply)
            await pilot.press('escape')
            self.assertFalse(app.busy)
            await pilot.press('escape')

    async def test_refresh_preserves_removed_selected_identity_and_allows_deselection(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(100, 40)) as pilot:
            app.selected = {'bash'}
            refreshed = catalog.load_catalog(test_config())
            refreshed.entries = tuple(e for e in refreshed.entries if e.id != 'bash')
            app.replace_catalog(refreshed)
            self.assertIn('bash', app.selected)
            self.assertEqual(app.catalog.availability['bash'], 'removed')
            app.toggle_node(app.nodes['bash'])
            self.assertNotIn('bash', app.selected)

    async def test_unavailable_files_visible_dont_disable_apps_and_no_execution_on_enter(self):
        cfg = replace(test_config(), sync_paths=('~/missing-setup-fixture',))
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            app = TestApp(cfg, TARGET)
        async with app.run_test(size=(100, 40)) as pilot:
            file = next(e for e in app.catalog.entries if e.handler == 'file')
            self.assertIn(file.id, app.nodes)
            app.toggle_node(app.nodes[file.id])
            self.assertNotIn(file.id, app.selected)
            app.toggle_node(app.nodes['codex'])
            self.assertIn('codex', app.selected)
            tree = app.query_one(SetupTree)
            tree.select_node(app.nodes['codex'])
            tree.focus()
            await pilot.press('enter')
            self.assertFalse(app.busy)

    def test_suspend_restored_even_if_body_raises(self):
        from contextlib import contextmanager
        events = []
        @contextmanager
        def suspended():
            events.append('suspend')
            yield
            events.append('restore')
        app = TestApp(test_config(), TARGET)
        with patch.object(app, 'suspend', suspended):
            from homestack.models import AppError
            with self.assertRaisesRegex(AppError, "Cancelled"):
                app.terminal_on_main(lambda: (_ for _ in ()).throw(KeyboardInterrupt()))
        self.assertEqual(events, ['suspend', 'restore'])

    async def test_worker_terminal_handoff_runs_driver_on_main_thread(self):
        import asyncio
        import threading
        from contextlib import contextmanager
        events = []
        main = threading.get_ident()
        @contextmanager
        def suspended():
            events.append(("suspend", threading.get_ident()))
            yield
            events.append(("restore", threading.get_ident()))
        app = TestApp(test_config(), TARGET)
        async with app.run_test():
            with patch.object(app, "suspend", suspended):
                value = await asyncio.to_thread(app.terminal, lambda: threading.get_ident())
        self.assertEqual(value, main)
        self.assertEqual(events, [("suspend", main), ("restore", main)])
