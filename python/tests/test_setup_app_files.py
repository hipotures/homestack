from __future__ import annotations

from dataclasses import replace
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from homestack import setup, setup_catalog
from homestack import setup_config as definitions
from homestack.models import AppError
from homestack.setup_tui import SetupApp
from support import test_config


TARGET = {"name": "workspace", "vmid": 200, "ip": "192.0.2.200"}
CREDENTIALS = "~/.claude/.credentials.json"
STATUSLINE = "~/.claude/statusline.sh"


def claude_config(files=(CREDENTIALS, STATUSLINE)):
    cfg = test_config()
    setup_cfg = definitions.parse_setup({"items": [{"id": "claude", "files": list(files)}]})
    return replace(cfg, setup=setup_cfg)


class AppFileDefinitionTests(unittest.TestCase):
    def test_files_expand_to_owned_file_entries_and_round_trip(self):
        cfg = claude_config()
        claude = next(e for e in cfg.setup.items if e.id == "claude")
        children = definitions.app_file_entries(claude)
        self.assertEqual([e.id for e in children],
                         ["claude:file:~/.claude/.credentials.json", "claude:file:~/.claude/statusline.sh"])
        self.assertTrue(all(e.group == "app" and e.handler == "file" for e in children))
        self.assertTrue(all(e.params.application == "claude" for e in children))
        restored = definitions.parse_setup(tomllib.loads(definitions.setup_to_toml(cfg.setup))["setup"])
        self.assertEqual(restored, cfg.setup)

    def test_unsafe_or_duplicate_files_are_rejected(self):
        for files in (["/etc/passwd"], ["~/../x"], [STATUSLINE, STATUSLINE], [""]):
            with self.subTest(files=files), self.assertRaises(AppError):
                claude_config(files)

    def test_file_items_cannot_claim_an_application(self):
        with self.assertRaises(AppError):
            definitions.parse_setup({"items": [{"id": "x", "group": "files", "handler": "file", "label": "X",
                                                "description": "", "path": "~/.x", "application": "claude"}]})


def desktop_home(test):
    """Give the test a temporary desktop HOME containing both Claude files."""
    tmp = tempfile.TemporaryDirectory()
    test.addCleanup(tmp.cleanup)
    home = Path(tmp.name)
    (home / ".claude").mkdir()
    (home / ".claude/.credentials.json").write_text("{}")
    (home / ".claude/statusline.sh").write_text("#!/bin/sh\n")
    environment = patch.dict(os.environ, {"HOME": str(home), "XDG_STATE_HOME": str(home / "state")})
    environment.start()
    test.addCleanup(environment.stop)
    return tmp


class AppFilePlanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = desktop_home(self)
        self.cfg = claude_config()
        self.claude = next(e for e in self.cfg.setup.items if e.id == "claude")

    def test_application_selection_copies_its_files_after_installation(self):
        plan = setup.build_plan(self.cfg, TARGET, (self.claude,))
        self.assertEqual([e.id for e in plan.entries],
                         ["claude", "claude:file:~/.claude/.credentials.json", "claude:file:~/.claude/statusline.sh"])

    def test_single_file_can_be_selected_by_stable_id(self):
        entries, _ = setup_catalog.select_entries(self.cfg, ["app=claude:file:~/.claude/statusline.sh"])
        self.assertEqual([e.id for e in entries], ["claude:file:~/.claude/statusline.sh"])
        plan = setup.build_plan(self.cfg, TARGET, entries)
        self.assertEqual([e.id for e in plan.entries], ["claude:file:~/.claude/statusline.sh"])

    def test_catalog_reports_desktop_availability_per_file(self):
        (Path(self.tmp.name) / ".claude/statusline.sh").unlink()
        catalog = setup_catalog.load_catalog(self.cfg)
        row = next(r for r in catalog.rows(self.cfg) if r["id"] == "claude")
        self.assertEqual({f["path"]: f["availability"] for f in row["files"]},
                         {CREDENTIALS: "ready", STATUSLINE: "missing"})


class AppFileTUITests(unittest.IsolatedAsyncioTestCase):
    async def test_files_are_children_of_their_application(self):
        desktop_home(self)
        cfg = claude_config()
        app = SetupApp(cfg, TARGET)
        async with app.run_test(size=(120, 40)):
            credentials = "claude:file:~/.claude/.credentials.json"
            self.assertIs(app.nodes[credentials].parent, app.nodes["claude"])
            self.assertIn("Claude Code  0/2", app.nodes["claude"].label.plain)
            app.toggle_node(app.nodes["claude"])
            self.assertIn(credentials, app.selected)
            self.assertIn("Claude Code  2/2", app.nodes["claude"].label.plain)
            self.assertIn("Application: claude", app.details(credentials))
            self.assertIn("Desktop file copies:", app.details("claude"))


if __name__ == "__main__":
    unittest.main()
