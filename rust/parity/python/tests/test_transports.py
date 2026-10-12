"""Fixture and recording transports, without a simulator."""

from __future__ import annotations

import unittest
from typing import Any

from homestack import models
from homestack.transports.base import Transport
from homestack_parity import transports


class FakeApi:
    """Stands in for the HTTPS client of FixtureTransport."""

    def __init__(self, responses: dict[str, tuple[int, Any]]) -> None:
        self.responses = responses
        self.paths: list[str] = []

    def get(self, path: str, query: dict[str, str]) -> tuple[int, Any]:
        self.paths.append(path)
        return self.responses[path]


def fixture(responses: dict[str, tuple[int, Any]]) -> transports.FixtureTransport:
    return transports.FixtureTransport(cfg=object(), api=FakeApi(responses), sim_url="https://localhost:1")


class FixtureTransportTests(unittest.TestCase):
    def test_satisfies_the_transport_protocol(self) -> None:
        self.assertIsInstance(fixture({}), Transport)
        self.assertIsInstance(transports.RecordingTransport(fixture({})), Transport)

    def test_data_is_printed_like_pvesh(self) -> None:
        transport = fixture({"/version": (200, {"data": {"release": "9.2", "version": "9.2.21"}})})
        result = transport.run("pvesh get /version --output-format json")
        self.assertEqual(result, models.RemoteResult(0, '{"release":"9.2","version":"9.2.21"}'))
        self.assertEqual(transport.run_json("pvesh get /version --output-format json")["version"], "9.2.21")

    def test_api_errors_fail_like_pvesh(self) -> None:
        transport = fixture({"/nodes/x/status": (500, {"data": None, "message": "hostname lookup 'x' failed\n"})})
        result = transport.run("pvesh get /nodes/x/status --output-format json", check=False)
        self.assertEqual(result, models.RemoteResult(255, "hostname lookup 'x' failed"))
        with self.assertRaises(models.AppError):
            transport.run("pvesh get /nodes/x/status --output-format json")

    def test_unknown_paths_fail_like_pvesh(self) -> None:
        transport = fixture({"/cluster/nothing": (501, {"data": None, "message": "Method 'GET /cluster/nothing' not implemented"})})
        result = transport.run("pvesh get /cluster/nothing --output-format json", check=False)
        self.assertEqual(result, models.RemoteResult(1, "No 'get' handler defined for '/cluster/nothing'"))

    def test_progress_callback_receives_output_lines(self) -> None:
        transport = fixture({"/nodes": (200, {"data": [{"node": "a"}]})})
        lines: list[str] = []
        transport.run_with_progress("pvesh get /nodes --output-format json", lines.append)
        self.assertEqual(lines, ['[{"node":"a"}]'])


class RecordingTransportTests(unittest.TestCase):
    def test_records_commands_and_operations_in_order(self) -> None:
        recording = transports.RecordingTransport(
            fixture({"/version": (200, {"data": {}}), "/nodes": (200, {"data": []})})
        )
        recording.run_json("pvesh get /version --output-format json")
        recording.run_json_value("pvesh get /nodes --output-format json")
        self.assertEqual(
            recording.commands,
            ["pvesh get /version --output-format json", "pvesh get /nodes --output-format json"],
        )
        self.assertEqual(recording.ops, ["get{path=/version}", "get{path=/nodes}"])
        self.assertEqual(recording.execution_info()["type"], "fixture")


if __name__ == "__main__":
    unittest.main()
