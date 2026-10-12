from __future__ import annotations

import json
import tomllib
import unittest

from homestack import setup_documents
from homestack.models import AppError


class SetupDocumentTests(unittest.TestCase):
    def test_empty_array_is_a_value_and_unrelated_null_is_preserved(self):
        cases = (("toml", 'items = [1]\nother = "keep"\n'),
                 ("json", '{"items": [1], "other": null}'),
                 ("yaml", 'items: [1]\nother: null\n'))
        for format, source in cases:
            with self.subTest(format=format):
                candidate, states = setup_documents.merge_document(format, source, [(("items",), [])])
                self.assertEqual(states, ["different"])
                from ruamel.yaml import YAML
                parsed = {"toml": tomllib.loads, "json": json.loads, "yaml": YAML(typ="safe").load}[format](candidate)
                self.assertEqual(list(parsed["items"]), [])
                self.assertIn("other", parsed)
                self.assertEqual(parsed["other"], "keep" if format == "toml" else None)

    def test_existing_empty_json_is_malformed(self):
        for text in ("", " \n\t"):
            with self.subTest(text=text), self.assertRaises(AppError):
                setup_documents.merge_document("json", text, [(("enabled",), True)])

    def test_toml_root_and_nested_insertion_preserve_unrelated_content(self) -> None:
        original = (
            'model = "workspace-only"\n'
            '\n[projects."/home/user/DEV/example"]\n'
            'trust_level = "trusted"\n'
            '\n[tui]\n'
            'theme = "custom"\n'
        )
        candidate, states = setup_documents.merge_document(
            "toml",
            original,
            [
                (("approval_policy",), "never"),
                (("features", "context_management", "experimental_mode"), True),
                (("tui", "status_line_use_colors"), True),
            ],
        )
        parsed = tomllib.loads(candidate)
        self.assertEqual(states, ["missing", "missing", "missing"])
        self.assertEqual(parsed["model"], "workspace-only")
        self.assertEqual(parsed["projects"]["/home/user/DEV/example"]["trust_level"], "trusted")
        self.assertEqual(parsed["tui"]["theme"], "custom")
        self.assertTrue(parsed["features"]["context_management"]["experimental_mode"])
        self.assertTrue(parsed["tui"]["status_line_use_colors"])

    def test_toml_existing_managed_key_update_and_unrelated_section_preserved(self) -> None:
        original = (
            'approval_policy = "ask"\n'
            'model = "private"\n'
            '\n[projects."/secret"]\n'
            'trust_level = "trusted"\n'
            '\n[tui]\n'
            'theme = "custom"\n'
            'status_line_use_colors = false\n'
        )
        candidate, states = setup_documents.merge_document(
            "toml", original,
            [(("approval_policy",), "never"), (("tui", "status_line_use_colors"), True)],
        )
        parsed = tomllib.loads(candidate)
        self.assertEqual(states, ["different", "different"])
        self.assertEqual(parsed["approval_policy"], "never")
        self.assertEqual(parsed["tui"]["status_line_use_colors"], True)
        self.assertEqual(parsed["tui"]["theme"], "custom")
        self.assertEqual(parsed["projects"]["/secret"]["trust_level"], "trusted")
        self.assertEqual(parsed["model"], "private")

    def test_toml_array_is_replaced_atomically(self) -> None:
        original = 'status_line = ["model", "custom"]\nother = "preserve"\n'
        desired = ["model-with-reasoning", "current-dir"]
        candidate, states = setup_documents.merge_document("toml", original, [(('status_line',), desired)])
        self.assertEqual(states, ["different"])
        parsed = tomllib.loads(candidate)
        self.assertEqual(parsed["status_line"], desired)
        self.assertEqual(parsed["other"], "preserve")

    def test_toml_noop_returns_original_and_distinguishes_bool_from_integer(self) -> None:
        original = 'enabled = true\n'
        candidate, states = setup_documents.merge_document("toml", original, [(('enabled',), True)])
        self.assertEqual(states, ["matching"])
        self.assertEqual(candidate, original)
        candidate, states = setup_documents.merge_document("toml", original, [(('enabled',), 1)])
        self.assertEqual(states, ["different"])
        self.assertEqual(tomllib.loads(candidate)["enabled"], 1)

    def test_missing_toml_file_is_created(self) -> None:
        candidate, states = setup_documents.merge_document(
            "toml", None, [(("approvals_reviewer",), "user"), (("tui", "theme"), "dark")]
        )
        self.assertEqual(states, ["missing", "missing"])
        parsed = tomllib.loads(candidate)
        self.assertEqual(parsed["approvals_reviewer"], "user")
        self.assertEqual(parsed["tui"]["theme"], "dark")

    def test_inline_toml_table_is_promoted_for_nested_insertion(self) -> None:
        candidate, states = setup_documents.merge_document(
            "toml", "features = { existing = 1 }\n", [(('features', 'context_management', 'experimental_mode'), True)]
        )
        self.assertEqual(states, ["missing"])
        parsed = tomllib.loads(candidate)
        self.assertEqual(parsed["features"]["existing"], 1)
        self.assertTrue(parsed["features"]["context_management"]["experimental_mode"])

    def test_structural_conflict_blocks_without_returning_partial_candidate(self) -> None:
        original = 'features = "not-a-table"\n'
        with self.assertRaisesRegex(AppError, "requires a mapping"):
            setup_documents.merge_document("toml", original, [(('features', 'multi_agent'), True)])

    def test_json_merge_preserves_unrelated_keys(self) -> None:
        original = json.dumps({"model": "private", "unrelated": None, "tui": {"theme": "custom"}, "projects": {"/x": {"trust": True}}})
        candidate, states = setup_documents.merge_document(
            "json", original, [(('approval_policy',), "never"), (('tui', 'status_line_use_colors'), True)]
        )
        parsed = json.loads(candidate)
        self.assertEqual(states, ["missing", "missing"])
        self.assertEqual(parsed["model"], "private")
        self.assertIsNone(parsed["unrelated"])
        self.assertEqual(parsed["tui"]["theme"], "custom")
        self.assertTrue(parsed["tui"]["status_line_use_colors"])
        self.assertTrue(parsed["projects"]["/x"]["trust"])

    def test_yaml_merge_preserves_unrelated_keys_and_aliases(self) -> None:
        original = (
            "defaults: &defaults\n"
            "  theme: dark\n"
            "  unrelated: preserve\n"
            "profile: *defaults\n"
            "unrelated: null\n"
        )
        candidate, states = setup_documents.merge_document("yaml", original, [(('defaults', 'theme'), "light")])
        self.assertEqual(states, ["different"])
        self.assertIn("unrelated: preserve", candidate)
        # The alias value remains unchanged; the edited branch may be
        # expanded because its anchor must be detached before mutation.
        self.assertIn("profile:", candidate)
        parsed = setup_documents._parse_yaml(candidate)
        self.assertEqual(parsed["defaults"]["theme"], "light")
        self.assertEqual(parsed["profile"]["theme"], "dark")
        self.assertEqual(parsed["profile"]["unrelated"], "preserve")
        self.assertIsNone(parsed["unrelated"])

    def test_yaml_merge_key_shared_descendant_is_detached_before_edit(self) -> None:
        original = (
            "base: &base\n"
            "  nested:\n"
            "    keep: 1\n"
            "app:\n"
            "  <<: *base\n"
        )
        candidate, states = setup_documents.merge_document(
            "yaml", original, [(('app', 'nested', 'keep'), 2)]
        )
        self.assertEqual(states, ["different"])
        parsed = setup_documents._parse_yaml(candidate)
        self.assertEqual(parsed["base"]["nested"]["keep"], 1)
        self.assertEqual(parsed["app"]["nested"]["keep"], 2)

    def test_literal_dotted_key_is_not_split(self) -> None:
        original = '"foo.bar" = "old"\n'
        candidate, states = setup_documents.merge_document("toml", original, [(('foo.bar',), "new")])
        self.assertEqual(states, ["different"])
        parsed = tomllib.loads(candidate)
        self.assertEqual(parsed["foo.bar"], "new")
        self.assertNotIn("foo", parsed)

    def test_inspection_reports_per_leaf_state_and_unrelated_keys_do_not_drift(self) -> None:
        original = 'model = "private"\napproval_policy = "never"\nfeatures = "bad"\n'
        states = setup_documents.inspect_document(
            "toml",
            original,
            [
                (("approval_policy",), "never"),
                (("sandbox_mode",), "danger-full-access"),
                (("features", "multi_agent"), True),
            ],
        )
        self.assertEqual(states, ["matching", "missing", "unavailable"])

    def test_malformed_documents_and_explicit_yaml_null_are_rejected(self) -> None:
        for format, text in (("toml", "broken = ["), ("json", "{"), ("yaml", "broken: [")):
            with self.subTest(format=format), self.assertRaises(AppError):
                setup_documents.merge_document(format, text, [(('enabled',), True)])
        with self.assertRaisesRegex(AppError, "root must be a mapping"):
            setup_documents.merge_document("yaml", "null\n", [(('enabled',), True)])

    def test_json_duplicate_and_nonstandard_numbers_are_rejected(self) -> None:
        for text in ('{"key": 1, "key": 2}', '{"key": NaN}'):
            with self.subTest(text=text), self.assertRaises(AppError):
                setup_documents.merge_document("json", text, [(('enabled',), True)])


if __name__ == "__main__":
    unittest.main()
