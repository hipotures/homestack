"""Offline OVH record ownership tests; all credentials are generated at runtime."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import secrets
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch
import urllib.parse

import ovh_dns as d

NAME = "homestack-test-0123456789abcdef.example.invalid"
ZONE = "example.invalid"


class OvhTests(unittest.TestCase):
    def setUp(self):
        self.keys = [secrets.token_hex(24) for _ in range(3)]
        self.rows = {1: {"id": 1, "zone": ZONE, "fieldType": "A", "subDomain": "production",
                         "target": "192.0.2.1", "ttl": 3600}}
        self.calls = []
        self.fail = None
        self.signature_errors = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                self.handle_request()

            do_POST = do_DELETE = do_GET

            def handle_request(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
                path = urllib.parse.urlsplit(self.path)
                owner.calls.append((self.command, self.path))
                status, result = 200, None
                if path.path == "/auth/time":
                    result = int(time.time())
                else:
                    timestamp = self.headers.get("X-Ovh-Timestamp", "")
                    signing = "+".join((owner.keys[1], owner.keys[2], self.command,
                                        owner.url + self.path, raw, timestamp))
                    expected = "$1$" + hashlib.sha1(signing.encode()).hexdigest()
                    if (self.headers.get("X-Ovh-Signature") != expected
                            or self.headers.get("X-Ovh-Application") != owner.keys[0]
                            or self.headers.get("X-Ovh-Consumer") != owner.keys[2]):
                        owner.signature_errors += 1
                        status = 403
                    elif path.path == "/domain/zone/" + ZONE:
                        result = {"name": ZONE}
                    elif path.path == "/domain/zone/" + ZONE + "/refresh" and self.command == "POST":
                        result = None
                    elif path.path == "/domain/zone/" + ZONE + "/record":
                        if self.command == "GET":
                            subdomain = urllib.parse.parse_qs(path.query).get("subDomain", [None])[0]
                            result = [key for key, row in owner.rows.items() if row["subDomain"] == subdomain]
                        elif self.command == "POST":
                            record_id = max(owner.rows, default=0) + 1
                            row = {**json.loads(raw), "id": record_id, "zone": ZONE}
                            owner.rows[record_id] = row
                            result = row
                    elif path.path.startswith("/domain/zone/" + ZONE + "/record/"):
                        record_id = int(path.path.rsplit("/", 1)[1])
                        result = owner.rows.get(record_id)
                        if result is None:
                            status = 404
                        elif self.command == "DELETE":
                            del owner.rows[record_id]
                            result = None
                    else:
                        status = 404
                # Simulate a committed write with a lost/error response.
                if owner.fail == (self.command, path.path):
                    owner.fail = None
                    status, result = 503, {"sensitive": owner.keys}
                payload = json.dumps(result).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = "http://127.0.0.1:" + str(self.server.server_port)
        # Production accepts only the fixed official HTTPS endpoints.
        self.endpoints = patch.dict(d.ENDPOINTS, {"offline-test": self.url})
        self.endpoints.start()
        self.dns = d.OvhDns(self.url, *self.keys, "dashboard.example.invalid")
        self.snapshots = []

    def tearDown(self):
        self.endpoints.stop()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def prepare(self):
        return self.dns.prepare(NAME, ZONE)

    def save(self):
        self.snapshots.append(json.loads(json.dumps(self.plan)))

    def test_preflight_is_read_only_and_signatures_are_valid(self):
        self.plan = self.prepare()
        self.assertIsNone(self.plan["id"])
        self.assertTrue(all(method == "GET" for method, _ in self.calls))
        self.assertEqual(self.signature_errors, 0)

    def test_create_repeat_and_cleanup_preserve_other_records(self):
        self.plan = self.prepare()
        before = dict(self.rows[1])
        self.dns.ensure(self.plan, self.save)
        record_id = self.plan["id"]
        self.dns.ensure(self.plan, self.save)
        self.assertEqual(len(self.rows), 2)
        self.assertTrue(any(s["pending"] and s["id"] is None for s in self.snapshots))
        self.dns.remove(self.plan, self.save)
        self.dns.remove(self.plan, self.save)
        self.assertEqual(self.rows, {1: before})
        self.assertEqual([path for method, path in self.calls if method == "DELETE"],
                         [f"/domain/zone/{ZONE}/record/{record_id}"])
        self.assertEqual(self.signature_errors, 0)

    def test_existing_name_is_not_adopted_even_with_same_target(self):
        self.rows[2] = {"id": 2, "zone": ZONE, "fieldType": "CNAME", "subDomain": NAME.split(".")[0],
                        "target": self.dns.target, "ttl": 60}
        with self.assertRaisesRegex(d.DnsFailure, "unrecorded"):
            self.prepare()
        self.assertTrue(all(method == "GET" for method, _ in self.calls))

    def test_changed_record_or_plan_refuses_cleanup(self):
        self.plan = self.prepare()
        self.dns.ensure(self.plan, self.save)
        record = self.rows[self.plan["id"]]
        original = dict(record)
        for key, value in (("target", "other.example.invalid."), ("subDomain", "production"),
                           ("zone", "other.invalid"), ("ttl", 3600), ("fieldType", "A")):
            with self.subTest(field=key):
                record.clear()
                record.update(original)
                record[key] = value
                with self.assertRaises(d.DnsFailure):
                    self.dns.remove(self.plan, self.save)
        record.clear()
        record.update(original)
        changed = {**self.plan, "id": 1}
        with self.assertRaises(d.DnsFailure):
            self.dns.remove(changed, self.save)
        changed = {**self.plan, "zone": "invalid"}
        with self.assertRaises(d.DnsFailure):
            self.dns.remove(changed, self.save)
        self.assertFalse(any(method == "DELETE" for method, _ in self.calls))

    def test_lost_create_response_recovers_without_duplicate(self):
        self.plan = self.prepare()
        self.fail = ("POST", f"/domain/zone/{ZONE}/record")
        with self.assertRaises(d.DnsFailure):
            self.dns.ensure(self.plan, self.save)
        self.assertTrue(self.plan["pending"])
        self.dns.ensure(self.plan, self.save)
        self.assertEqual(len(self.rows), 2)
        self.assertEqual(sum(method == "POST" and path.endswith("/record") for method, path in self.calls), 1)

    def test_unresolved_pending_creation_never_retries(self):
        self.plan = self.prepare()
        self.plan["pending"] = True
        with self.assertRaisesRegex(d.DnsFailure, "unresolved"):
            self.dns.ensure(self.plan, self.save)
        self.assertTrue(all(method == "GET" for method, _ in self.calls))

    def test_lost_delete_response_and_refresh_failure_are_recoverable(self):
        self.plan = self.prepare()
        self.dns.ensure(self.plan, self.save)
        self.fail = ("DELETE", f"/domain/zone/{ZONE}/record/{self.plan['id']}")
        with self.assertRaises(d.DnsFailure):
            self.dns.remove(self.plan, self.save)
        self.assertTrue(self.plan["deletion_pending"])
        self.fail = ("POST", f"/domain/zone/{ZONE}/refresh")
        with self.assertRaises(d.DnsFailure):
            self.dns.remove(self.plan, self.save)
        self.dns.remove(self.plan, self.save)
        self.assertEqual(set(self.rows), {1})
        self.assertTrue(self.plan["deleted"])
        self.assertFalse(self.plan["deletion_pending"])

    def test_error_responses_never_expose_credentials(self):
        self.plan = self.prepare()
        self.fail = ("POST", f"/domain/zone/{ZONE}/record")
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            with self.assertRaises(d.OvhFailure) as caught:
                self.dns.ensure(self.plan, self.save)
        text = str(caught.exception) + output.getvalue() + json.dumps(self.snapshots)
        for credential in self.keys:
            self.assertNotIn(credential, text)

    def test_private_credential_file_is_loaded_without_sourcing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            deploy = root / "deploy.toml"
            deploy.write_text('[acme]\novh_file = "credentials"\n')
            credential = root / "credentials"
            credential.write_text("\n".join(name + "=" + value for name, value in
                zip(("OVH_AK", "OVH_AS", "OVH_CK"), self.keys)) + "\nOVH_END_POINT=offline-test\n")
            credential.chmod(0o600)
            loaded = d.OvhDns.from_deploy(deploy, "https://dashboard.example.invalid")
            self.assertEqual(loaded.target, self.dns.target)
            credential.chmod(0o644)
            with self.assertRaisesRegex(d.DnsFailure, "private regular"):
                d.OvhDns.from_deploy(deploy, "https://dashboard.example.invalid")
            credential.chmod(0o600)
            link = root / "linked"
            link.symlink_to(credential)
            deploy.write_text('[acme]\novh_file = "linked"\n')
            with self.assertRaises(d.DnsFailure):
                d.OvhDns.from_deploy(deploy, "https://dashboard.example.invalid")

    def test_custom_endpoint_is_rejected_before_transport(self):
        for endpoint in ("http://example.invalid", "https://example.invalid", "https://eu.api.ovh.com/1.0/evil"):
            with self.subTest(endpoint=endpoint), patch.object(d.OvhDns, "call") as call:
                with self.assertRaises(d.DnsFailure):
                    d.OvhDns(endpoint, *self.keys, "dashboard.example.invalid")
                call.assert_not_called()

    def test_desktop_integration_credentials_without_acme_reference(self):
        for metadata in ('domain = "example.invalid"\n', '[acme]\nzone = "example.invalid"\n'):
            with self.subTest(metadata=metadata), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                deploy = root / "deploy.toml"
                deploy.write_text(metadata)
                (root / "integrations").mkdir(mode=0o700)
                credential = root / "integrations/ovh-dns"
                fd = os.open(credential, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
                with os.fdopen(fd, "w") as stream:
                    stream.write("\n".join(name + "=" + value for name, value in
                        zip(("OVH_AK", "OVH_AS", "OVH_CK"), self.keys)) + "\nOVH_END_POINT=offline-test\n")
                loaded = d.OvhDns.from_deploy(deploy, "https://dashboard.example.invalid")
                self.assertEqual(loaded.target, self.dns.target)
                self.assertTrue(all(method == "GET" for method, _ in self.calls))

    def test_missing_explicit_credential_file_does_not_select_other_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            deploy = root / "deploy.toml"
            deploy.write_text('[acme]\novh_file = "missing"\n')
            with patch.object(d.os, "open", side_effect=FileNotFoundError) as opened:
                with self.assertRaisesRegex(d.DnsFailure, "Cannot load private OVH credentials"):
                    d.OvhDns.from_deploy(deploy, "https://dashboard.example.invalid")
                self.assertEqual(opened.call_args.args[0], root / "missing")
                self.assertEqual(opened.call_count, 1)


if __name__ == "__main__":
    unittest.main()
