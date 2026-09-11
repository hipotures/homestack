from __future__ import annotations

from dataclasses import replace
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from textual.widgets import Input, Static
from homestack import setup_catalog as catalog, setup_config as definitions
from homestack.setup_tui import CompactAction, SetupApp, SetupTree, Review
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
            for selector in ('#counts', '#controls', '#keys', 'Footer'):
                self.assertFalse(app.query(selector))
            self.assertNotIn('Hidden selected:', str(app.query_one('#target').render()))
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
            self.assertIn('Hidden selected: 1', str(app.query_one('#target', Static).render()))
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
            self.assertIn('Hidden selected: 1', str(app.query_one('#target', Static).render()))
            app.busy = True
            await self.click_part(pilot, app, 'bash', 'checkbox')
            await pilot.press('space')
            self.assertEqual(app.selected, {'bash', 'codex'})
            app.busy = False
            await self.click_part(pilot, app, 'env', 'checkbox')
            self.assertEqual(app.selected, {'codex'})

    async def test_details_tab_page_keys_and_wheel_do_not_change_selection(self):
        import asyncio
        from textual.containers import VerticalScroll
        from textual.events import MouseScrollDown

        app = TestApp(replace(test_config(), sync_paths=('~/short-fixture',)), TARGET)
        short_entry = next(e for e in app.catalog.entries if e.handler == 'file')
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
            # A short file detail fits without scrolling even with complete
            # metadata; installer command length must not determine this case.
            # Wait for Textual's deferred layout/focus update, not just the
            # highlight event, which still sees the previous content height.
            settled = asyncio.Event()
            update_focus = app.update_details_focus

            def focus_updated():
                update_focus()
                if app.details_identity == short_entry.id and not pane.can_focus:
                    settled.set()

            with patch.object(app, 'update_details_focus', side_effect=focus_updated):
                tree.select_node(app.nodes[short_entry.id])
                await asyncio.wait_for(settled.wait(), timeout=5)
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
            self.assertGreaterEqual(pane.region.x, tree.region.right)
            self.assertGreater(tree.size.height, 0)
            self.assertEqual(app.selected, {'bash'})
            self.assertEqual(tree.cursor_node.data, 'bash')
            footer = app.query_one('#setup-footer')
            self.assertEqual(footer.region.bottom, 24)
            self.assertEqual(footer.size.height, 1)
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
            self.assertIn('example-owner/project', app.details(entry.id))
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


class SetupActionTests(unittest.IsolatedAsyncioTestCase):
    async def test_enter_opens_review_and_requires_a_second_confirmation(self):
        app = TestApp(test_config(), TARGET)
        with patch.object(app, 'execute') as execute:
            async with app.run_test(size=(120, 40)) as pilot:
                tree = app.query_one(SetupTree)
                tree.select_node(app.nodes['codex'])
                await pilot.press('space', 'enter')
                await pilot.pause()
                self.assertIsInstance(app.screen, Review)
                self.assertEqual(app.focused.id, 'apply')
                execute.assert_not_called()
                self.assertFalse(app.busy)
                await pilot.press('enter')
                execute.assert_called_once_with()
                self.assertTrue(app.busy)
                action = app.query_one('#review', CompactAction)
                self.assertTrue(action.disabled)
                self.assertFalse(action.pending)
                app.review()
                execute.assert_called_once()

    async def test_empty_selection_notifies_and_filter_enter_reviews(self):
        app = TestApp(test_config(), TARGET)
        with patch.object(app, 'prepare_review') as prepare, patch.object(app, 'notify') as notify:
            async with app.run_test(size=(120, 40)) as pilot:
                await pilot.press('enter')
                prepare.assert_not_called()
                notify.assert_called_with('No setup actions selected.')
                app.selected = {'codex'}
                app.query_one(Input).focus()
                await pilot.press('c', 'enter')
                self.assertEqual(app.query_one(Input).value, 'c')
                prepare.assert_called_once()
                self.assertNotIsInstance(app.screen, Review)

    async def test_compact_mouse_actions_back_focus_and_escape(self):
        app = TestApp(test_config(), TARGET)
        with patch.object(app, 'execute') as execute:
            async with app.run_test(size=(120, 40)) as pilot:
                app.selected = {'codex'}
                app.update_selection()
                await pilot.click('#review')
                await pilot.pause()
                self.assertIsInstance(app.screen, Review)
                for control in app.screen.query(CompactAction):
                    self.assertEqual(control.size.height, 1)
                    self.assertTrue(control.can_focus)
                await pilot.press('shift+tab')
                self.assertEqual(app.focused.id, 'back')
                await pilot.press('enter')
                self.assertNotIsInstance(app.screen, Review)
                execute.assert_not_called()
                await pilot.click('#review')
                await pilot.pause()
                await pilot.press('escape')
                self.assertNotIsInstance(app.screen, Review)
                await pilot.click('#review')
                await pilot.pause()
                await pilot.click('#back')
                execute.assert_not_called()
                await pilot.click('#review')
                await pilot.pause()
                await pilot.click('#apply')
                execute.assert_called_once()

    async def test_details_enter_and_tab_order(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(80, 24)) as pilot:
            tree = app.query_one(SetupTree)
            tree.select_node(app.nodes['codex'])
            await pilot.press('space')
            await pilot.pause()
            await pilot.press('tab')
            self.assertEqual(app.focused.id, 'details-pane')
            await pilot.press('enter')
            await pilot.pause()
            self.assertIsInstance(app.screen, Review)
            await pilot.press('escape')
            app.query_one('#review', CompactAction).focus()
            await pilot.press('tab')
            self.assertEqual(app.focused.id, 'cancel')
            await pilot.press('shift+tab')
            self.assertEqual(app.focused.id, 'review')

    async def test_pending_key_transitions_include_hidden_selections(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)) as pilot:
            action = app.query_one('#review', CompactAction)
            self.assertEqual(action.render().plain, 'Enter Review & apply')
            self.assertEqual(action.render().spans[0].end, len('Enter'))
            normal = action.render().spans[0].style
            self.assertEqual(normal, '#d29922')
            self.assertEqual(app.query_one('#cancel', CompactAction).render().spans[0].style, normal)
            self.assertFalse(action.pending)
            app.selected = {'codex'}
            app.update_selection()
            action._blink_timer.pause()
            self.assertTrue(action.pending)
            bright = action.render().spans[0].style
            self.assertIn('#d29922', bright)
            action.advance_blink()
            self.assertIn('#6e7681', action.render().spans[0].style)
            action.advance_blink()
            self.assertEqual(action.render().spans[0].style, bright)
            app.query_one(Input).value = 'bash'
            await pilot.pause()
            self.assertTrue(action.pending)
            self.assertNotIn('codex', app.nodes)
            app.selected.clear()
            app.update_selection()
            self.assertFalse(action.pending)
            self.assertEqual(action.render().spans[0].style, normal)
            action.advance_blink()
            self.assertEqual(action.render().spans[0].style, normal)
        self.assertIsNone(action._blink_timer)

    async def test_success_clears_selection_and_restores_ready_green(self):
        state = {'items': {'codex': {'ready': True, 'state': 'installed'}}}
        app = TestApp(test_config(), TARGET, state=state)
        async with app.run_test(size=(120, 40)) as pilot:
            entry = next(e for e in app.catalog.entries if e.id == 'codex')
            self.assertEqual(app.entry_style(entry), 'green')
            app.toggle_node(app.nodes['codex'])
            self.assertEqual(app.entry_style(entry), 'bold red')
            self.assertTrue(app.query_one('#review', CompactAction).pending)
            app.busy = True
            app.update_review_action()
            self.assertFalse(app.query_one('#review', CompactAction).pending)
            app.finished({'ok': True, 'results': [
                {'id': 'codex', 'label': 'Codex', 'status': 'succeeded', 'detail': 'done'}
            ]}, state)
            await pilot.pause()
            self.assertFalse(app.selected)
            self.assertEqual(app.entry_style(entry), 'green')
            self.assertNotIsInstance(app.screen, Review)
            self.assertEqual(app.focused.id, 'tree')
            self.assertFalse(app.query_one('#review', CompactAction).pending)
            self.assertFalse(app.query_one('#review', CompactAction).disabled)

    async def test_complete_details_fold_at_narrow_and_wide_widths(self):
        app = TestApp(test_config(), TARGET)
        entry = next(e for e in app.catalog.entries if e.id == 'codex')
        description = 'Long description ' + 'unbroken-path-' * 60
        app.catalog.entries = tuple(replace(e, description=description) if e.id == 'codex'
                                    else e for e in app.catalog.entries)
        async with app.run_test(size=(120, 40)) as pilot:
            app.query_one(SetupTree).select_node(app.nodes['codex'])
            for width, height in ((120, 40), (80, 24), (40, 30), (140, 42)):
                await pilot.resize_terminal(width, height)
                await pilot.pause()
                details = app.query_one('#details', Static)
                pane = app.query_one('#details-pane')
                content = details.render().plain
                self.assertIn('ID: codex', content)
                self.assertIn('Command: ' + entry.params.command, content)
                self.assertIn(description, content)
                self.assertEqual(pane.max_scroll_x, 0)
                self.assertGreater(pane.max_scroll_y, 0)
                self.assertGreater(details.size.height, len(content.splitlines()))
                for control in app.query(CompactAction):
                    self.assertEqual(control.size.height, 1)
                    control.focus()
                    await pilot.pause()
                    await pilot.wait_for_scheduled_animations()
                    self.assertLessEqual(control.region.right, width)
                    self.assertGreaterEqual(control.region.x, 0)
                    self.assertIn(control.label, control.render_line(0).text)
            self.assertIn('State registry:', app.details('root'))
            self.assertIn('State registry:', app.details('app'))

    async def test_narrow_confirmation_actions_keep_complete_labels(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(40, 30)) as pilot:
            app.push_screen(Review('\n'.join(f'Action {i}' for i in range(100))))
            await pilot.pause()
            self.assertGreater(app.screen.query_one('#review-content').max_scroll_y, 0)
            back = app.screen.query_one('#back', CompactAction)
            apply = app.screen.query_one('#apply', CompactAction)
            row = app.screen.query_one('#review-actions')
            self.assertEqual(back.render().plain, ' Esc  Back')
            self.assertEqual(apply.render().plain, ' Enter  Apply this plan')
            self.assertLessEqual(abs((back.region.x + apply.region.right) -
                                     (row.region.x + row.region.right)), 1)
            for control in app.screen.query(CompactAction):
                rendered = control.render()
                self.assertEqual(rendered.spans[0].end, len(control.key) + 2)
                self.assertIn('on #30363d', rendered.spans[0].style)
                self.assertNotIn('on ', rendered.spans[1].style)
                self.assertEqual(control.size.height, 1)
                self.assertGreaterEqual(control.content_size.width, control.render().cell_len)
                self.assertIn(control.label, control.render_line(0).text)
                self.assertLessEqual(control.region.right, 40)
            await pilot.press('escape')

    def test_custom_commands_stay_redacted_and_no_button_or_details_action_remains(self):
        import inspect
        from homestack import setup_tui
        app = TestApp(test_config(), TARGET)
        app.catalog.entries = tuple(replace(e, params=replace(e.params, command='SECRET_PAYLOAD'))
                                    if e.id == 'codex' else e for e in app.catalog.entries)
        self.assertNotIn('SECRET_PAYLOAD', app.details('codex'))
        self.assertIn('Custom payload withheld', app.details('codex'))
        self.assertNotIn('Button', inspect.getsource(setup_tui))
        self.assertFalse(hasattr(SetupApp, 'action_details'))

    def test_complete_details_preserve_state_metadata_and_overwrite_colors(self):
        app = TestApp(test_config(), TARGET, state={
            'checked_at': '2026-09-11T01:00:00Z',
            'items': {'bash': {
                'ready': False, 'will_overwrite': True, 'state': 'needs update',
                'first_managed_at': 'first-managed', 'installed_at': 'installed',
                'last_applied_at': 'last-applied', 'last_snapshot': '~/snapshot/example',
                'files': [{'path': '.bashrc', 'exists': True, 'size': 123,
                           'sha256': 'abcdef0123456789', 'mtime_ns': 1_000_000_000,
                           'birthtime_ns': 1_000_000_000}],
            }},
        })
        content = app.details('bash')
        for value in ('ID: bash', 'Status: UPDATE', 'first-managed', 'last-applied',
                      '~/snapshot/example', 'Managed path: ~/.bashrc',
                      'SHA-256: abcdef0123456789', 'Size: 123 bytes',
                      'Created: 1970-01-01T00:00:01Z', 'Modified: 1970-01-01T00:00:01Z',
                      'State checked: 2026-09-11T01:00:00Z', 'Managed paths:'):
            self.assertIn(value, content)
        entry = next(e for e in app.catalog.entries if e.id == 'bash')
        self.assertEqual(app.entry_style(entry), '')
        app.selected = {'bash'}
        self.assertEqual(app.entry_style(entry), 'bold red')

    async def test_ready_environment_review_does_not_claim_an_update(self):
        app = TestApp(test_config(), TARGET, state={
            'items': {'bash': {'ready': True, 'state': 'configured'}},
        })
        async with app.run_test(size=(120, 40)) as pilot:
            self.assertTrue(app.details('bash').startswith('Status: NO CHANGES'))
            app.toggle_node(app.nodes['bash'])
            bash = next(entry for entry in app.catalog.entries if entry.id == 'bash')
            self.assertEqual(app.entry_style(bash), 'green')
            app.review()
            await pilot.pause()
            self.assertIsInstance(app.screen, Review)
            self.assertIn('Verify shell configuration; write only if changes are needed', app.screen.text)
            self.assertNotIn('Update / reapply existing state', app.screen.text)
            await pilot.press('escape')

    def test_managed_environment_with_missing_shell_is_not_green(self):
        app = TestApp(test_config(), TARGET, state={
            'items': {'bash': {'ready': False, 'managed': True, 'state': 'shell missing'}},
        })
        bash = next(entry for entry in app.catalog.entries if entry.id == 'bash')
        self.assertEqual(app.entry_style(bash), '')

    async def test_modified_shell_file_shows_backup_and_overwrite(self):
        app = TestApp(test_config(), TARGET, state={
            'items': {'bash': {'ready': False, 'managed': True, 'state': 'needs update',
                               'will_overwrite': True, 'changed_since_apply': ['.profile']}},
        })
        async with app.run_test(size=(120, 40)) as pilot:
            app.query_one(SetupTree).select_node(app.nodes['bash'])
            await pilot.pause()
            content = app.query_one('#details', Static).render().plain
            self.assertTrue(content.startswith('Status: UPDATE'))
            self.assertIn('Changed since last apply:\n~/.profile', content)
            self.assertIn('backed up before replacement', content)
            bash = next(entry for entry in app.catalog.entries if entry.id == 'bash')
            self.assertEqual(app.entry_style(bash), 'green')
            app.toggle_node(app.nodes['bash'])
            self.assertEqual(app.entry_style(bash), 'bold red')
            app.toggle_node(app.nodes['bash'])
            self.assertEqual(app.entry_style(bash), 'green')
            app.toggle_node(app.nodes['bash'])
            app.review()
            await pilot.pause()
            self.assertIn('Changed since last apply: ~/.profile', app.screen.text)
            self.assertIn('Back up modified shell files; overwrite configuration', app.screen.text)
            self.assertNotIn('Overwrite existing configuration', app.screen.text)
            await pilot.press('escape')

    async def test_stale_rebuild_restore_cannot_replace_newer_highlight(self):
        app = TestApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            tree = app.query_one(SetupTree)
            old_generation = app._rebuild_generation
            app.rebuild()
            await pilot.pause()
            tree.move_cursor(app.nodes['codex'])
            app.restore_cursor('bash', 'root', old_generation)
            self.assertEqual(tree.cursor_node.data, 'codex')
            app.restore_cursor('bash', 'root', app._rebuild_generation)
            self.assertEqual(tree.cursor_node.data, 'codex')
            self.assertIn('ID: codex', app.query_one('#details', Static).render().plain)
