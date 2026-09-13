from __future__ import annotations

from dataclasses import replace
from io import StringIO
import json
import unittest
from unittest.mock import patch

from textual.widgets import Static

from homestack import setup_config as definitions
from homestack.setup import Plan
from homestack.setup_catalog import Catalog, select_entries
from homestack.setup_cli import show_plan
from homestack.setup_tui import SetupApp, SetupTree
from support import test_config


TARGET = {"name": "workspace", "vmid": 200, "ip": "192.0.2.200"}


def fixture_config():
    config = definitions.ConfigFile(
        id="config",
        path="~/.fixture/config.toml",
        format="toml",
        values={
            "root.key": "literal",
            "tui": {
                "status_line": ["model", "current-dir", "git-branch"],
                "status_line_use_colors": True,
            },
            "features": {
                "multi_agent": True,
                "context_management": {"experimental_mode": True},
            },
        },
    )
    base = next(entry for entry in definitions.defaults() if entry.id == "codex")
    application = replace(
        base,
        params=replace(base.params, config_files=(config,), validation=None),
    )
    setup = replace(
        test_config().setup,
        items=tuple(application if entry.id == "codex" else entry for entry in test_config().setup.items),
    )
    return replace(test_config(), setup=setup), application


class StructuredCatalogTests(unittest.TestCase):
    def test_rows_keep_application_index_and_expose_nested_values_and_selectors(self):
        cfg, application = fixture_config()
        catalog = Catalog(tuple(cfg.setup.items), {}, timestamps={})
        row = next(item for item in catalog.rows(cfg) if item["id"] == "codex")
        config = row["config_files"][0]
        self.assertEqual(row["index"], 1)
        self.assertEqual(config["path"], "~/.fixture/config.toml")
        self.assertEqual(config["values"]["root.key"], "literal")
        self.assertIn(
            definitions.config_entry_id("codex", "config", ("features", "multi_agent")),
            config["selectors"],
        )
        self.assertNotIn("validation", config)

    def test_stable_leaf_selector_selects_only_that_declared_value(self):
        cfg, application = fixture_config()
        leaf_id = definitions.config_entry_id("codex", "config", ("root.key",))
        selected, snapshot = select_entries(cfg, [f"app={leaf_id}"])
        self.assertIsNone(snapshot)
        self.assertEqual([entry.id for entry in selected], [leaf_id])
        self.assertEqual(selected[0].params.key, ("root.key",))


class StructuredTUITests(unittest.IsolatedAsyncioTestCase):
    @staticmethod
    def default_leaf_id(key):
        return definitions.config_entry_id("codex", "config", key)

    async def test_hierarchical_counters_use_visible_semantic_levels(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)):
            self.assertIn("Applications  0/3", app.nodes["app"].label.plain)
            self.assertIn("0/8", app.nodes["codex"].label.plain)
            self.assertNotRegex(app.nodes["opencode"].label.plain, r"\d+/\d+")
            self.assertNotRegex(app.nodes["hermes"].label.plain, r"\d+/\d+")
            self.assertFalse(app.nodes["codex"].is_expanded)
            self.assertFalse(app.nodes["codex:config"].is_expanded)

            app.toggle_node(app.nodes[self.default_leaf_id(("approvals_reviewer",))])
            self.assertIn("Applications  1/3", app.nodes["app"].label.plain)
            self.assertIn("Codex  1/8", app.nodes["codex"].label.plain)

            app.toggle_node(app.nodes["opencode"])
            self.assertIn("Applications  2/3", app.nodes["app"].label.plain)

    async def test_config_file_and_section_counters_count_only_leaf_descendants(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)):
            app.toggle_node(app.nodes["codex"])
            codex_leaves = definitions.config_entries(
                next(entry for entry in test_config().setup.items if entry.id == "codex")
            )
            self.assertEqual(
                app.selected,
                {"codex", *(leaf.id for leaf in codex_leaves)},
            )
            file_id = "codex:config"
            tui_id = self.default_leaf_id(("tui",))
            colors_id = self.default_leaf_id(("tui", "status_line_use_colors"))
            features_id = self.default_leaf_id(("features",))
            context_id = self.default_leaf_id(("features", "context_management"))

            self.assertIn("Codex  8/8", app.nodes["codex"].label.plain)
            self.assertIn("~/.codex/config.toml  8/8", app.nodes[file_id].label.plain)
            self.assertIn("tui  2/2", app.nodes[tui_id].label.plain)
            self.assertIn("features  3/3", app.nodes[features_id].label.plain)
            self.assertIn("context_management  1/1", app.nodes[context_id].label.plain)

            app.toggle_node(app.nodes[colors_id])
            self.assertEqual(app.checkbox("codex"), "[-]")
            self.assertIn("Codex  7/8", app.nodes["codex"].label.plain)
            self.assertIn("~/.codex/config.toml  7/8", app.nodes[file_id].label.plain)
            self.assertIn("tui  1/2", app.nodes[tui_id].label.plain)
            self.assertIn("features  3/3", app.nodes[features_id].label.plain)
            self.assertIn("context_management  1/1", app.nodes[context_id].label.plain)

    async def test_section_bulk_selection_only_selects_managed_leaf_descendants(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)):
            section_id = definitions.config_entry_id("codex", "config", ("tui",))
            tui_leaves = {
                leaf.id
                for leaf in definitions.config_entries(
                    next(entry for entry in test_config().setup.items if entry.id == "codex")
                )
                if leaf.params.key[:1] == ("tui",)
            }

            app.toggle_node(app.nodes[section_id])
            self.assertEqual(app.selected, tui_leaves)
            self.assertEqual(app.checkbox(section_id), "[x]")
            self.assertIn("tui  2/2", app.nodes[section_id].label.plain)

            app.toggle_node(app.nodes[section_id])
            self.assertFalse(app.selected)
            self.assertEqual(app.checkbox(section_id), "[ ]")

    async def test_installer_only_application_is_partial_and_group_counts_applications(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)):
            app.toggle_node(app.nodes["codex"])
            app.toggle_node(app.nodes["codex:config"])

            self.assertEqual(app.selected, {"codex"})
            self.assertEqual(app.checkbox("codex"), "[-]")
            self.assertIn("Codex  0/8", app.nodes["codex"].label.plain)
            self.assertIn("Applications  1/3", app.nodes["app"].label.plain)

    async def test_application_group_tri_state_tracks_involved_visible_applications(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)):
            self.assertEqual(app.checkbox("app"), "[ ]")

            app.toggle_node(app.nodes[self.default_leaf_id(("approvals_reviewer",))])
            self.assertEqual(app.checkbox("app"), "[-]")
            self.assertIn("Applications  1/3", app.nodes["app"].label.plain)

            app.toggle_node(app.nodes["codex"])
            app.toggle_node(app.nodes["opencode"])
            self.assertEqual(app.checkbox("app"), "[-]")
            self.assertIn("Applications  2/3", app.nodes["app"].label.plain)

            app.toggle_node(app.nodes["hermes"])
            self.assertEqual(app.checkbox("app"), "[x]")
            self.assertIn("Applications  3/3", app.nodes["app"].label.plain)

            app.toggle_node(app.nodes[self.default_leaf_id(("approvals_reviewer",))])
            self.assertEqual(app.checkbox("app"), "[-]")
            self.assertIn("Applications  3/3", app.nodes["app"].label.plain)

    async def test_application_and_config_details_distinguish_state_and_counts(self):
        cfg = test_config()
        leaves = definitions.config_entries(next(entry for entry in cfg.setup.items if entry.id == "codex"))
        states = {leaf.id: {"state": "matching"} for leaf in leaves}
        states[leaves[0].id] = {"state": "different"}
        app = SetupApp(
            cfg,
            TARGET,
            state={"items": {"codex": {"state": "installed"}, **states}},
        )
        async with app.run_test(size=(120, 40)):
            app.selected = {leaf.id for leaf in leaves[1:]}
            application_details = app.details("codex")
            self.assertIn("Application state: installed", application_details)
            self.assertIn("Install/update: not selected", application_details)
            self.assertIn("Configuration: selected 7/8 managed options", application_details)

            file_details = app.details("codex:config")
            self.assertIn("Configuration: selected 7/8 managed options", file_details)
            self.assertIn("Workspace state: 1 different, 7 matching", file_details)

            app.selected = {self.default_leaf_id(("tui", "status_line"))}
            section_details = app.details(
                definitions.config_entry_id("codex", "config", ("tui",))
            )
            self.assertIn("Configuration: selected 1/2 managed options", section_details)
            self.assertIn("Workspace state: 2 matching", section_details)

    async def test_filtered_bulk_selection_preserves_hidden_selected_counts(self):
        app = SetupApp(test_config(), TARGET)
        async with app.run_test(size=(120, 40)) as pilot:
            app.toggle_node(app.nodes["codex"])
            app.query_one("#filter").value = "approvals_reviewer"
            await pilot.pause()

            self.assertIn("Applications  1/3", app.nodes["app"].label.plain)
            self.assertIn("Codex  8/8", app.nodes["codex"].label.plain)
            self.assertIn("~/.codex/config.toml  8/8", app.nodes["codex:config"].label.plain)

            app.toggle_node(app.nodes["codex:config"])
            self.assertIn("Applications  1/3", app.nodes["app"].label.plain)
            self.assertIn("Codex  7/8", app.nodes["codex"].label.plain)
            self.assertIn("~/.codex/config.toml  7/8", app.nodes["codex:config"].label.plain)
            self.assertIn(self.default_leaf_id(("tui", "status_line")), app.selected)

    async def test_config_branches_start_collapsed_and_preserve_manual_expansion(self):
        cfg, _ = fixture_config()
        app = SetupApp(cfg, TARGET)
        async with app.run_test(size=(120, 40)):
            branches = ("codex", "codex:config", definitions.config_entry_id("codex", "config", ("tui",)))
            for identity in branches:
                self.assertFalse(app.nodes[identity].is_expanded)
            for identity in branches:
                app.nodes[identity].expand()
            app.rebuild()
            for identity in branches:
                self.assertTrue(app.nodes[identity].is_expanded)
            app.nodes["codex"].collapse()
            app.rebuild()
            self.assertFalse(app.nodes["codex"].is_expanded)

    async def test_config_tree_has_file_sections_and_leaves_without_root_group(self):
        cfg, application = fixture_config()
        app = SetupApp(cfg, TARGET)
        async with app.run_test(size=(120, 40)):
            file_id = "codex:config"
            tui_id = definitions.config_entry_id("codex", "config", ("tui",))
            colors_id = definitions.config_entry_id(
                "codex", "config", ("tui", "status_line_use_colors")
            )
            context_id = definitions.config_entry_id(
                "codex", "config", ("features", "context_management")
            )
            self.assertIn(file_id, app.nodes)
            self.assertIn(tui_id, app.nodes)
            self.assertIn(colors_id, app.nodes)
            self.assertIn(context_id, app.nodes)
            self.assertEqual(app.nodes[file_id].parent.data, "codex")
            self.assertEqual(app.nodes[tui_id].parent.data, file_id)
            self.assertEqual(app.nodes[colors_id].parent.data, tui_id)
            self.assertNotIn("config", [group.id for group in cfg.setup.groups])

            label = app.nodes[colors_id].label.plain
            self.assertIn("status_line_use_colors", label)
            self.assertIn("true", label)

    async def test_application_file_section_and_leaf_selection_are_tri_state(self):
        cfg, application = fixture_config()
        app = SetupApp(cfg, TARGET)
        async with app.run_test(size=(120, 40)):
            file_id = "codex:config"
            tui_id = definitions.config_entry_id("codex", "config", ("tui",))
            colors_id = definitions.config_entry_id(
                "codex", "config", ("tui", "status_line_use_colors")
            )
            app.toggle_node(app.nodes[file_id])
            self.assertEqual(set(app._descendant_ids(file_id)), app.selected)
            self.assertEqual(app.checkbox(file_id), "[x]")
            self.assertEqual(app.checkbox("codex"), "[-]")
            app.toggle_node(app.nodes[colors_id])
            self.assertNotIn(colors_id, app.selected)
            self.assertEqual(app.checkbox(tui_id), "[-]")
            self.assertEqual(app.checkbox(file_id), "[-]")
            app.rebuild()
            self.assertIn("[-] 1. Codex", app.nodes["codex"].label.plain)
            self.assertIn("[-] ~/.fixture/config.toml", app.nodes[file_id].label.plain)
            self.assertIn("[-] tui", app.nodes[tui_id].label.plain)

            app.toggle_node(app.nodes["codex"])
            self.assertIn("codex", app.selected)
            self.assertEqual(app.checkbox("codex"), "[x]")

            app.toggle_node(app.nodes[colors_id])
            self.assertEqual(app.checkbox(tui_id), "[-]")
            self.assertEqual(app.checkbox("codex"), "[-]")

    async def test_leaf_details_show_complete_declared_array_and_review_uses_explicit_leaves(self):
        cfg, application = fixture_config()
        app = SetupApp(cfg, TARGET)
        captured = {}

        def prepare(selected, catalog):
            captured["selected"] = selected

        async with app.run_test(size=(120, 40)):
            app.prepare_review = prepare
            leaf_id = definitions.config_entry_id("codex", "config", ("tui", "status_line"))
            details = app.details(leaf_id)
            for value in ("model", "current-dir", "git-branch"):
                self.assertIn(value, details)
            app.toggle_node(app.nodes[leaf_id])
            app.review()
            self.assertEqual([entry.id for entry in captured["selected"]], [leaf_id])


class StructuredPlanRenderingTests(unittest.TestCase):
    def test_text_plan_includes_structured_key_and_desired_value(self):
        cfg, application = fixture_config()
        leaf = definitions.config_entries(application)[0]
        plan = Plan(TARGET, (leaf,))
        output = StringIO()
        from rich.console import Console

        show_plan(Console(file=output, force_terminal=False), plan)
        rendered = output.getvalue()
        self.assertIn("root.key", rendered)
        self.assertIn('"literal"', rendered)
