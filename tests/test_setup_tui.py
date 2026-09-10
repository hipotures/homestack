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


class SetupInteractionTests(unittest.IsolatedAsyncioTestCase):
    async def click_part(self, pilot, app, identity, part, *, button=1):
        """Click rendered cells, including after scrolling; exercise real events."""
        from rich.cells import cell_len

        await pilot.pause()
        tree = app.query_one(SetupTree)
        node = app.nodes[identity]
        tree.scroll_to_node(node, animate=False)
        await pilot.pause()
        y = node.line - tree.scroll_offset.y
        x = 0
        for segment in tree.render_line(y):
            metadata = segment.style.meta if segment.style else {}
            hit = (
                part == 'checkbox' and metadata.get('setup_checkbox') == identity
                or part == 'arrow' and metadata.get('toggle')
                or part == 'label' and metadata.get('node') == node.id
                and not metadata.get('toggle') and 'setup_checkbox' not in metadata
            )
            if hit and segment.text.strip():
                offset = (x + tree.content_region.x - tree.region.x,
                          y + tree.content_region.y - tree.region.y)
                self.assertTrue(await pilot.click(tree, offset=offset, button=button))
                await pilot.pause()
                return
            x += cell_len(segment.text)
        self.fail(f'No visible {part} cells for {identity}')

    async def test_mouse_checkbox_label_arrow_and_right_click_are_distinct(self):
        from unittest.mock import Mock
        executor = Mock()
        app = TestApp(test_config(), TARGET, executor=executor)
        async with app.run_test(size=(140, 40)) as pilot:
            await self.click_part(pilot, app, 'bash', 'checkbox')
            self.assertEqual(app.selected, {'bash'})
            self.assertEqual(app.query_one(SetupTree).cursor_node.data, 'bash')
            self.assertEqual(app.checkbox('env'), '[-]')
            await self.click_part(pilot, app, 'zsh', 'label')
            self.assertEqual(app.selected, {'bash'})
            self.assertEqual(app.query_one(SetupTree).cursor_node.data, 'zsh')
            self.assertIn('Zsh', str(app.query_one('#details', Static).render()))
            await self.click_part(pilot, app, 'bash', 'checkbox', button=3)
            self.assertEqual(app.selected, {'bash'})
            await self.click_part(pilot, app, 'bash', 'checkbox')
            self.assertFalse(app.selected)
            await self.click_part(pilot, app, 'env', 'checkbox')
            self.assertEqual(app.selected, {'bash', 'zsh', 'fish', 'nu'})
            self.assertTrue(app.nodes['env'].is_expanded)
            await self.click_part(pilot, app, 'env', 'label')
            self.assertTrue(app.nodes['env'].is_expanded)
            await self.click_part(pilot, app, 'env', 'arrow')
            self.assertFalse(app.nodes['env'].is_expanded)
            self.assertEqual(app.selected, {'bash', 'zsh', 'fish', 'nu'})
            await self.click_part(pilot, app, 'env', 'checkbox')
            self.assertFalse(app.selected)
            self.assertFalse(app.nodes['env'].is_expanded)
            await self.click_part(pilot, app, 'env', 'arrow')
            self.assertTrue(app.nodes['env'].is_expanded)
            executor.assert_not_called()
            self.assertFalse(app.busy)

    async def test_mouse_respects_filter_unavailable_items_and_busy_guard(self):
        cfg = replace(test_config(), sync_paths=('~/missing-setup-fixture',))
        with tempfile.TemporaryDirectory() as tmp, patch.object(Path, 'home', return_value=Path(tmp)):
            app = TestApp(cfg, TARGET)
        async with app.run_test(size=(120, 40)) as pilot:
            file = next(e for e in app.catalog.entries if e.handler == 'file')
            await self.click_part(pilot, app, file.id, 'checkbox')
            self.assertFalse(app.selected)
            app.selected = {'codex'}
            app.query_one(Input).value = 'bash'
            await pilot.pause()
            await self.click_part(pilot, app, 'env', 'checkbox')
            self.assertEqual(app.selected, {'bash', 'codex'})
            self.assertIn('Hidden selected: 1', str(app.query_one('#counts', Static).render()))
            app.busy = True
            await self.click_part(pilot, app, 'bash', 'checkbox')
            await pilot.press('space')
            self.assertEqual(app.selected, {'bash', 'codex'})
            app.busy = False
            await self.click_part(pilot, app, 'env', 'checkbox')
            self.assertEqual(app.selected, {'codex'})

    async def test_details_tab_page_keys_and_wheel_do_not_change_selection(self):
        from textual.containers import VerticalScroll
        from textual.events import MouseScrollDown

        app = TestApp(test_config(), TARGET)
        app.catalog.entries = tuple(
            replace(e, description='\n'.join(f'Detail line {i}' for i in range(100)))
            if e.id == 'bash' else e for e in app.catalog.entries
        )
        async with app.run_test(size=(120, 32)) as pilot:
            await pilot.pause()
            tree = app.query_one(SetupTree)
            tree.select_node(app.nodes['bash'])
            await pilot.pause()
            pane = app.query_one('#details-pane', VerticalScroll)
            self.assertTrue(pane.can_focus)
            await pilot.press('tab')
            self.assertIs(app.focused, pane)
            await pilot.press('pagedown')
            await pilot.wait_for_scheduled_animations()
            self.assertGreater(pane.scroll_y, 0)
            self.assertEqual(tree.cursor_node.data, 'bash')
            self.assertFalse(app.selected)
            await pilot.press('shift+tab')
            self.assertIs(app.focused, tree)
            before = pane.scroll_y
            tree_scroll = tree.scroll_y
            await pilot._post_mouse_events([MouseScrollDown], widget=pane, offset=(3, 3))
            await pilot.pause()
            self.assertGreater(pane.scroll_y, before)
            self.assertEqual(tree.scroll_y, tree_scroll)
            self.assertFalse(app.selected)
            # Changing the highlighted item resets details to the top. A short
            # panel is skipped in the tab order without trapping keyboard focus.
            tree.select_node(app.nodes['codex'])
            await pilot.pause()
            self.assertEqual(pane.scroll_y, 0)
            self.assertFalse(pane.can_focus)
            await pilot.press('tab')
            self.assertEqual(app.focused.id, 'review')

    async def test_layout_resizes_without_losing_selection_or_fixed_controls(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(140, 42)) as pilot:
            tree = app.query_one(SetupTree)
            pane = app.query_one('#details-pane')
            self.assertGreaterEqual(pane.region.x, tree.region.right)
            await self.click_part(pilot, app, 'bash', 'checkbox')
            await pilot.resize_terminal(80, 24)
            await pilot.pause()
            self.assertGreaterEqual(pane.region.y, tree.region.bottom)
            self.assertGreater(tree.size.height, 0)
            self.assertEqual(app.selected, {'bash'})
            self.assertEqual(tree.cursor_node.data, 'bash')
            for name in ('counts', 'controls', 'keys'):
                widget = app.query_one('#' + name)
                self.assertLessEqual(widget.region.bottom, 24)
                self.assertGreater(widget.size.height, 0)
            await pilot.resize_terminal(140, 42)
            await pilot.pause()
            self.assertGreaterEqual(pane.region.x, tree.region.right)
            self.assertEqual(app.selected, {'bash'})
            self.assertIn('192.0.2.200', str(app.query_one('#target', Static).render()))

    async def test_repository_initial_collapse_short_labels_and_full_identity(self):
        cfg = replace(test_config(), repo_owner='example-owner')
        app = TestApp(cfg, TARGET)
        entry = definitions.Entry('example-owner/project', 'repo', 'repository',
                                  'example-owner/project', 'Repository description',
                                  definitions.RepositoryParams('example-owner/project'))
        app.catalog.entries += (entry,)
        app.catalog.availability[entry.id] = 'available'
        app.catalog.timestamps[entry.id] = {'created_at': '2026-09-01T00:00:00Z', 'pushed_at': '2026-09-10T00:00:00Z'}
        async with app.run_test(size=(140, 40)) as pilot:
            self.assertFalse(app.nodes['repo'].is_expanded)
            await self.click_part(pilot, app, 'repo', 'arrow')
            self.assertTrue(app.nodes['repo'].is_expanded)
            self.assertIn('project', app.nodes[entry.id].label.plain)
            self.assertNotIn('example-owner/', app.nodes[entry.id].label.plain)
            await self.click_part(pilot, app, entry.id, 'checkbox')
            self.assertEqual(app.selected, {entry.id})
            self.assertIn('~/DEV/project', app.details(entry.id))
            self.assertIn('example-owner/project', app.details(entry.id, full=True))
            self.assertIn('Last push: 2026-09-10T00:00:00Z', app.details(entry.id))
            self.assertIn('Recent first', app.details('repo'))
            # A click on the root checkbox does not re-open a collapsed root.
            await self.click_part(pilot, app, 'root', 'arrow')
            self.assertFalse(app.nodes['root'].is_expanded)
            await self.click_part(pilot, app, 'root', 'checkbox')
            self.assertFalse(app.nodes['root'].is_expanded)
            self.assertEqual(app.query_one(SetupTree).cursor_node.data, 'root')

    async def test_mouse_hit_regions_survive_horizontal_and_vertical_scrolling(self):
        from textual.events import MouseScrollDown

        app = TestApp(test_config(), TARGET)
        entries = tuple(definitions.Entry(
            f'extra-{i}', 'app', 'application', f'Extra {i} ' + 'wide-label-' * 15,
            'Test-only installer', definitions.ApplicationParams('true')) for i in range(40))
        app.catalog.entries += entries
        async with app.run_test(size=(120, 28)) as pilot:
            tree = app.query_one(SetupTree)
            await self.click_part(pilot, app, entries[-1].id, 'label')
            self.assertGreater(tree.scroll_y, 0)
            tree.scroll_to(x=4, animate=False, force=True)
            await pilot.pause()
            self.assertGreater(tree.scroll_x, 0)
            await self.click_part(pilot, app, entries[-1].id, 'checkbox')
            self.assertEqual(app.selected, {entries[-1].id})
            self.assertEqual(tree.cursor_node.data, entries[-1].id)
            tree.scroll_home(animate=False)
            await pilot.pause()
            pane = app.query_one('#details-pane')
            details_scroll = pane.scroll_y
            await pilot._post_mouse_events([MouseScrollDown], widget=tree, offset=(3, 3))
            await pilot.pause()
            self.assertGreater(tree.scroll_y, 0)
            self.assertEqual(pane.scroll_y, details_scroll)
            self.assertEqual(app.selected, {entries[-1].id})
