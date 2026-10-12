"""Translation of the shell commands HomeStack runs into canonical operations."""

from __future__ import annotations

import shlex
import unittest

from homestack_parity import ops


class FormatTests(unittest.TestCase):
    def test_parameters_are_sorted(self) -> None:
        self.assertEqual(
            ops.format_op("get", {"type": "vm", "path": "/cluster/resources"}),
            "get{path=/cluster/resources,type=vm}",
        )


class PveshTranslationTests(unittest.TestCase):
    def test_plain_read(self) -> None:
        call = ops.translate("pvesh get /nodes/pve-example-1/status --output-format json")
        self.assertEqual(call.op, "get{path=/nodes/pve-example-1/status}")
        self.assertEqual(call.path, "/nodes/pve-example-1/status")
        self.assertEqual(call.query, {})

    def test_options_become_query_parameters(self) -> None:
        call = ops.translate("pvesh get /cluster/resources --type vm --output-format json")
        self.assertEqual(call.op, "get{path=/cluster/resources,type=vm}")
        self.assertEqual(call.query, {"type": "vm"})

    def test_quoted_arguments_are_unquoted(self) -> None:
        call = ops.translate("pvesh get '/nodes/pve-example-1/storage/local/content' --content images --output-format json")
        self.assertEqual(call.op, "get{content=images,path=/nodes/pve-example-1/storage/local/content}")

    def test_ssh_routing_to_another_node_is_transparent(self) -> None:
        local = ops.translate("pvesh get /nodes/pve-example-2/status --output-format json")
        routed = ops.translate(
            "ssh -o BatchMode=yes root@pve-example-2 'pvesh get /nodes/pve-example-2/status --output-format json'"
        )
        self.assertEqual(routed.ops, local.ops)
        self.assertEqual((local.node, routed.node), (None, "pve-example-2"))

    def test_a_path_query_parameter_does_not_replace_the_api_path(self) -> None:
        call = ops.translate("pvesh get /access/permissions --path /vms --output-format json")
        self.assertEqual(call.op, "get{path=/access/permissions,query_path=/vms}")

    def test_reads_must_ask_for_json(self) -> None:
        with self.assertRaises(ops.UnsupportedCommand):
            ops.translate("pvesh get /version")

    def test_anything_else_is_refused_loudly(self) -> None:
        for command in ["qm status 200", "pvesh set /nodes/x/config --foo 1 --output-format json", "rm -f /x", ""]:
            with self.subTest(command=command), self.assertRaises(ops.UnsupportedCommand):
                ops.translate(command)


class GuestTranslationTests(unittest.TestCase):
    def test_ping_with_or_without_timeout(self) -> None:
        for command in ["qm guest cmd 200 ping", "timeout -k 2s 5s qm guest cmd 200 ping"]:
            with self.subTest(command=command):
                call = ops.translate(command)
                self.assertEqual(call, ops.GuestPing(200))
                self.assertEqual(call.ops, ["guest_ping{vmid=200}"])

    def test_exec_is_one_command_and_two_operations(self) -> None:
        shell = "ip -4 -o addr show dev eth0 2>/dev/null | awk '{print $4}' | head -n1"
        call = ops.translate(
            "ssh -o BatchMode=yes root@pve-example-2 "
            + shlex.quote(f"qm guest exec 200 -- /bin/sh -lc {shlex.quote(shell)}")
        )
        self.assertEqual(call, ops.GuestExec(200, shell, "pve-example-2"))
        self.assertEqual(
            call.ops,
            [
                "guest_exec{cmd=ip -4 -o addr show dev eth0 2>/dev/null | awk '%7Bprint $4%7D' | head -n1,vmid=200}",
                "guest_exec_status{vmid=200}",
            ],
        )


if __name__ == "__main__":
    unittest.main()
