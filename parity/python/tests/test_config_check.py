"""Mapping of HomeStack configuration errors to field names."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from homestack_parity import config_check

EXAMPLE = Path(__file__).resolve().parents[3] / "python" / "config.example.toml"


class FieldTests(unittest.TestCase):
    def test_messages_map_to_fields(self) -> None:
        cases = {
            "Missing configuration value [network] gateway": "network.gateway",
            "[storage_layouts] homestack-storage-1 contains duplicate storage IDs": "storage_layouts.homestack-storage-1",
            "[storage_layouts] must be a TOML table": "storage_layouts",
            "[workspace_ssh] user 'a' must match [user] name 'b'": "workspace_ssh.user",
            "[workspace_ssh] fido_devices entries must be strings": "workspace_ssh.fido_devices",
            "[workspace_ssh] identity_files contains duplicate entries": "workspace_ssh.identity_files",
            "[transport.herdr] debug must be true or false": "transport.herdr.debug",
            "[sounds] has unknown keys: x": "sounds",
            "[root] disk and [home] disk must be non-empty": "root.disk",
            "Invalid GitHub repository owner 'x'": "repo.owner",
            "something new": "unmapped: something new",
        }
        for message, expected in cases.items():
            with self.subTest(message=message):
                self.assertEqual(config_check.field(message), expected)


class VerdictTests(unittest.TestCase):
    def test_example_is_accepted_and_errors_are_classified(self) -> None:
        self.assertEqual(config_check.verdict(EXAMPLE)["verdict"], "accepted")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            path.write_text(EXAMPLE.read_text().replace('type = "herdr"', 'type = "ssh"'))
            self.assertEqual(config_check.verdict(path)["verdict"], "transport.type")
            path.write_text(EXAMPLE.read_text().replace("gold_vmid = 100", 'gold_vmid = "abc"'))
            self.assertEqual(config_check.verdict(path)["verdict"], "crash:ValueError")


if __name__ == "__main__":
    unittest.main()
