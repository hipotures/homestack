from contextlib import ExitStack, contextmanager
from unittest import TestCase
from unittest.mock import Mock, patch

from homestack import cli, setup_catalog, setup_cli
from homestack.models import AppError
from support import test_config


TARGET = {"vmid": 200, "name": "workspace", "ip": "192.0.2.200"}


class StartupProgressTests(TestCase):
    def run_startup(self, *, fail_inspection=False, fail_connection=False):
        cfg = test_config()
        console = Mock()
        active = []
        connected = []
        phases = []
        workspace = Mock()

        @contextmanager
        def status(label, **kwargs):
            progress = Mock()
            phases.append(str(label))
            active.append(str(label))

            def update(label=None, **kwargs):
                text = str(label if label is not None else kwargs["status"])
                active[-1] = text
                phases.append(text)

            progress.update.side_effect = update
            try:
                yield progress
            finally:
                active.pop()

        @contextmanager
        def connection(*args):
            if fail_connection:
                raise AppError("Authentication failed")
            connected.append(workspace)
            try:
                yield workspace
            finally:
                connected.pop()

        def inspect(*args):
            self.assertEqual(connected, [workspace])
            self.assertTrue(active, "State inspection must have an active spinner")
            self.assertIn("SSH connected", active[-1])
            if fail_inspection:
                raise AppError("Inspection failed")
            return {"items": {}}

        def tui_run():
            self.assertEqual(active, [], "Spinner must stop before the TUI starts")
            self.assertEqual(connected, [workspace])
            return None

        console.status.side_effect = status
        args = cli.build_parser().parse_args(["setup", "workspace"])
        with ExitStack() as stack:
            stack.enter_context(patch.object(setup_cli, "Console", return_value=console))
            stack.enter_context(patch("sys.stdin.isatty", return_value=True))
            stack.enter_context(patch.object(setup_cli, "open_transport"))
            stack.enter_context(patch.object(setup_cli, "resolve_target", return_value=TARGET))
            stack.enter_context(patch.object(setup_cli, "load_catalog", return_value=setup_catalog.load_catalog(cfg)))
            stack.enter_context(patch.object(setup_cli, "save_snapshot"))
            stack.enter_context(patch.object(setup_cli.WorkspaceSSH, "configured", side_effect=connection))
            stack.enter_context(patch.object(setup_cli, "inspect_workspace_state", side_effect=inspect))
            app = stack.enter_context(patch("homestack.setup_tui.SetupApp"))
            app.return_value.run.side_effect = tui_run
            if fail_inspection or fail_connection:
                failure = "Authentication failed" if fail_connection else "Inspection failed"
                with self.assertRaisesRegex(AppError, failure):
                    setup_cli.run_setup(args, cfg, json_mode=False, assume_yes=False)
                app.assert_not_called()
            else:
                self.assertEqual(setup_cli.run_setup(args, cfg, json_mode=False, assume_yes=False), 0)
                app.return_value.run.assert_called_once()
        self.assertEqual(active, [])
        self.assertEqual(connected, [])
        return phases

    def test_connected_spinner_during_inspection_and_stopped_before_tui(self):
        phases = self.run_startup()
        self.assertGreaterEqual(len(phases), 3)

    def test_inspection_error_stops_spinner_and_closes_connection(self):
        self.run_startup(fail_inspection=True)

    def test_failed_authentication_stops_spinner_without_connected_status(self):
        phases = self.run_startup(fail_connection=True)
        self.assertFalse(any("SSH connected" in phase for phase in phases))
