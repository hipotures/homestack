"""Offline tests. All credential material is generated synthetically at runtime."""
import contextlib
import importlib.util
import io
import json
import os
import pty
import select
import signal
import sys
import time
from pathlib import Path
import secrets
import stat
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("bootstrap", Path(__file__).with_name("bootstrap.py"))
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)


class Fake:
    def __init__(self):
        self.org = "test-org"
        self.bootstrap_id = secrets.token_hex(7)
        self.bootstrap = self.bootstrap_id + "." + secrets.token_urlsafe(30)
        self.root = True
        self.permissions = set(b.MANAGEMENT)
        self.keys = []
        self.actions = {}
        self.new_tokens = set()
        self.requests = []
        self.creates = 0
        self.fail = None
        self.malformed = False
        self.leak_extra_action = False
        self.allow_key_reads = False
        self.allow_cross_org = False
        self.interrupt_confirmation = False
        self.reported_total = None
        self.reject_new_auth = False
        self.bad_creation = False
        self.bad_envelope = False

    def handle(self, handler):
        from urllib.parse import urlsplit, parse_qs
        parsed = urlsplit(handler.path)
        path = parsed.path.removeprefix("/v1")
        query = parse_qs(parsed.query)
        method = handler.command
        self.requests.append((method, path))
        credential = handler.headers.get("Authorization", "").removeprefix("Bearer ")
        is_bootstrap = credential == self.bootstrap
        is_new = credential in self.new_tokens and not self.reject_new_auth
        if not is_bootstrap and not is_new:
            return 401, {"message": credential}
        parts = path.split("/")
        org = parts[2] if len(parts) > 2 else None
        if not is_bootstrap and org != self.org and not self.allow_cross_org:
            return 403, {"message": credential}
        if path.endswith("/api-key") or path.endswith("/actions"):
            if not is_bootstrap or not self.root:
                return 403, {"message": credential}
        if path.endswith("api-keys"):
            if (not is_bootstrap or not self.root) and not self.allow_key_reads:
                return 403, {"message": credential}
            if is_bootstrap and "listApiKeys" not in self.permissions:
                return 403, {"message": credential}
            return 200, self.page("apiKeys", self.keys if org == self.org else [], query)
        if path.endswith("/actions"):
            key_id = parts[-2]
            permission = "listApiKeyActions" if method == "GET" else "setApiKeyActions"
            if permission not in self.permissions:
                return 403, {"message": credential}
            if method == "GET":
                actions = self.permissions if key_id == self.bootstrap_id else self.actions.get(key_id, set())
                return 200, self.page("actions", [{"actionId": a} for a in sorted(actions)], query)
            body = json.loads(handler.rfile.read(int(handler.headers["Content-Length"])))
            if self.fail == "assignment":
                return 500, {"message": credential + " " + " ".join(self.new_tokens)}
            self.actions[key_id] = set(body["actionIds"])
            if self.leak_extra_action:
                self.actions[key_id].add("deleteApiKey")
            return 200, {}
        if method == "PUT" and path.endswith("/api-key"):
            if "createApiKey" not in self.permissions:
                return 403, {"message": credential}
            self.creates += 1
            key_id = secrets.token_hex(7)
            secret = secrets.token_urlsafe(30)
            self.new_tokens.add(key_id + "." + secret)
            self.keys.append({"apiKeyId": key_id, "orgId": org, "name": "homestack"})
            self.actions[key_id] = set()
            if self.fail == "lost_creation_response":
                handler.connection.shutdown(2)
                handler.connection.close()
                return None
            if self.bad_creation:
                return 201, {"apiKeyId": key_id, "apiKey": {"unexpected": credential}}
            return 201, {"apiKeyId": key_id, "name": "homestack", "apiKey": secret}
        if method == "GET" and path == f"/org/{self.org}":
            if is_bootstrap and "getOrg" not in self.permissions:
                return 403, {"message": credential}
            return 200, {"org": {"orgId": self.org, "name": "Offline test"}}
        if method == "GET" and parts[-1] in ("sites", "resources", "domains"):
            return 200, self.page(parts[-1], [], query)
        return 404, {"message": credential}

    def page(self, field, rows, query):
        offset = int(query.get("offset", [0])[0])
        limit = int(query.get("limit", [100])[0])
        page = rows[offset:offset + limit]
        return {field: page, "pagination": {"offset": offset, "limit": limit,
                "total": len(page) if self.reported_total is None else self.reported_total}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.dispatch()

    def do_PUT(self):
        self.dispatch()

    def do_POST(self):
        self.dispatch()

    def dispatch(self):
        result = self.server.fake.handle(self)
        if result is None:
            return
        status, data = result
        payload = {"success": status < 400, "error": status >= 400, "data": data}
        if self.server.fake.bad_envelope:
            payload["success"] = "true"
        raw = b"invalid json" if self.server.fake.malformed else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class OfflineClient(b.Client):
    """Use the production loopback exception against a random local test port."""
    def __init__(self, endpoint, credential, *, allow_http_loopback=True):
        super().__init__(endpoint, credential, allow_http_loopback=allow_http_loopback)


class BootstrapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        self.fake = Fake()
        self.server.fake = self.fake
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}/v1"
        self.client = OfflineClient(self.endpoint, self.fake.bootstrap)
        self.temp = tempfile.TemporaryDirectory(prefix="homestack-bootstrap-test-")
        self.addCleanup(self.temp.cleanup)
        self.output = Path(self.temp.name) / "private" / "credential"
        self.report = {}
        self.stdout = io.StringIO()
        self.redirect = contextlib.redirect_stdout(self.stdout)
        self.redirect.__enter__()
        self.addCleanup(self.redirect.__exit__, None, None, None)

    def apply(self, confirm=None):
        return b.provision(self.client, self.fake.org, str(self.output), self.report,
            confirm=confirm or (lambda _: f"CREATE homestack IN {self.fake.org}"), client_factory=OfflineClient)

    def assert_no_secrets(self, content):
        for value in [self.fake.bootstrap, self.fake.bootstrap.split(".")[1], *self.fake.new_tokens]:
            self.assertNotIn(value, content)
            self.assertNotIn(value.split(".")[-1], content)

    def test_check_confirms_authentication_and_root_without_mutation(self):
        b.preflight(self.client, self.fake.org, self.report)
        self.assertTrue(self.report["can_provision_inferred"])
        self.assertFalse(self.report["organization_existence_confirmed"])
        self.assertIn("Root access", self.report["confirmed_by_successful_calls"])
        self.assertEqual(self.report["write_permissions_inferred_only"], ["createApiKey", "setApiKeyActions"])
        self.assertTrue(all(method == "GET" for method, _ in self.fake.requests))
        self.assert_no_secrets(json.dumps(self.report))

    def test_invalid_credentials_are_sanitized(self):
        self.client.credential = secrets.token_hex(7) + "." + secrets.token_urlsafe(30)
        with self.assertRaises(b.ApiFailure) as error:
            b.preflight(self.client, self.fake.org, self.report)
        self.assertEqual(error.exception.status, 401)
        self.assertNotIn(self.client.credential, str(error.exception))

    def test_missing_root_privileges(self):
        self.fake.root = False
        with self.assertRaises(b.ApiFailure) as error:
            self.apply()
        self.assertEqual(error.exception.status, 403)
        self.assertEqual(self.fake.creates, 0)

    def test_every_missing_management_permission_blocks_creation(self):
        for permission in b.MANAGEMENT:
            with self.subTest(permission=permission):
                self.fake.permissions = set(b.MANAGEMENT) - {permission}
                with self.assertRaises(b.Failure):
                    self.apply()
                self.assertEqual(self.fake.creates, 0)

    def test_create_assign_verify_and_private_persistence(self):
        self.apply()
        self.assertEqual(self.report["stage"], "complete")
        credential = self.output.read_text().strip()
        self.assertIn(credential, self.fake.new_tokens)
        self.assertEqual(self.fake.actions[credential.split(".")[0]], set(b.ACTIONS))
        self.assertEqual(stat.S_IMODE(self.output.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.output.parent.stat().st_mode), 0o700)
        state = self.output.with_name(self.output.name + ".state.json")
        self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o600)
        self.assert_no_secrets(state.read_text() + self.stdout.getvalue())
        writes = [(method, path) for method, path in self.fake.requests if method != "GET"]
        self.assertEqual([method for method, _ in writes], ["PUT", "POST"])
        self.assertTrue(all("api-key" in path for _, path in writes))

    def test_assignment_failure_preserves_key_and_recovery(self):
        self.fake.fail = "assignment"
        with self.assertRaises(b.ApiFailure) as error:
            self.apply()
        self.assertEqual(self.fake.creates, 1)
        self.assertIn(self.output.read_text().strip(), self.fake.new_tokens)
        state = self.output.with_name(self.output.name + ".state.json").read_text()
        self.assertTrue(json.loads(state)["recovery_required"])
        self.assertEqual(json.loads(state)["stage"], "assigning_actions")
        self.assert_no_secrets(state + str(error.exception) + self.stdout.getvalue())

    def test_lost_creation_response_never_retries_or_deletes(self):
        self.fake.fail = "lost_creation_response"
        with self.assertRaises(b.Failure):
            self.apply()
        state = json.loads(self.output.with_name(self.output.name + ".state.json").read_text())
        self.assertEqual(state["stage"], "creation_requested_outcome_unknown")
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertEqual(self.fake.creates, 1)
        self.assertFalse(any(method == "DELETE" for method, _ in self.fake.requests))

    def test_interrupted_assignment_preserves_credential(self):
        original = self.client.request
        def request(method, path, body=None):
            if method == "POST":
                raise KeyboardInterrupt()
            return original(method, path, body)
        with patch.object(self.client, "request", side_effect=request):
            with self.assertRaises(KeyboardInterrupt):
                self.apply()
        self.assertIn(self.output.read_text().strip(), self.fake.new_tokens)
        state = json.loads(self.output.with_name(self.output.name + ".state.json").read_text())
        self.assertTrue(state["recovery_required"])

    def test_existing_key_and_repeated_execution_stop(self):
        self.apply()
        self.output = Path(self.temp.name) / "another" / "credential"
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertEqual(self.fake.creates, 1)
        self.assertFalse(self.output.exists())

    def test_conflict_on_later_page_is_detected(self):
        self.fake.keys = [{"apiKeyId": f"test{i}", "orgId": self.fake.org, "name": "other"} for i in range(205)]
        self.fake.keys[-1]["name"] = "homestack"
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertEqual(self.fake.creates, 0)

    def test_conflict_created_during_confirmation_is_detected(self):
        def confirm(_):
            self.fake.keys.append({"apiKeyId": "another", "orgId": self.fake.org, "name": "homestack"})
            return f"CREATE homestack IN {self.fake.org}"
        with self.assertRaises(b.Failure):
            self.apply(confirm)
        self.assertEqual(self.fake.creates, 0)

    def test_incorrect_confirmation_has_no_effect(self):
        with self.assertRaises(b.Failure):
            self.apply(lambda _: "no")
        self.assertFalse(self.output.exists())
        self.assertEqual(self.fake.creates, 0)

    def test_unexpected_server_response(self):
        self.fake.malformed = True
        with self.assertRaises(b.Failure) as error:
            self.apply()
        self.assert_no_secrets(str(error.exception))
        self.assertEqual(self.fake.creates, 0)

    def test_extra_permissions_are_rejected(self):
        self.fake.leak_extra_action = True
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertTrue(self.output.exists())
        self.assertNotEqual(self.report["stage"], "complete")

    def test_unauthorized_key_administration_success_is_rejected(self):
        self.fake.allow_key_reads = True
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertTrue(self.report["recovery_required"])

    def test_unauthorized_cross_organization_success_is_rejected(self):
        self.fake.allow_cross_org = True
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertTrue(self.report["recovery_required"])

    def test_existing_file_is_never_overwritten(self):
        self.output.parent.mkdir(mode=0o700)
        self.output.write_text("non-secret marker")
        with self.assertRaises(FileExistsError):
            self.apply()
        self.assertEqual(self.output.read_text(), "non-secret marker")
        self.assertEqual(self.fake.creates, 0)

    def test_symlink_and_insecure_directory_are_rejected(self):
        parent = Path(self.temp.name)
        real = parent / "real"
        real.mkdir(mode=0o700)
        link = parent / "link"
        link.symlink_to(real, target_is_directory=True)
        with self.assertRaises(OSError):
            with b.Storage(str(link / "credential")):
                self.fail("Symlink was accepted")
        real.chmod(0o755)
        with self.assertRaises(b.Failure):
            with b.Storage(str(real / "credential")):
                self.fail("Insecure directory was accepted")

    def test_repository_and_relative_paths_are_rejected(self):
        for path in ("relative", str(Path(__file__).with_name("credential"))):
            with self.subTest(path=path), self.assertRaises(b.Failure):
                with b.Storage(path):
                    self.fail("Unsafe path was accepted")

    def test_network_security_rejects_http_and_credential_urls(self):
        for endpoint in (self.endpoint, "https://user:password@example.invalid/v1",
                         "https://example.invalid/v1?secret=value", "https://example.invalid/"):
            with self.subTest(endpoint=endpoint), self.assertRaises(b.Failure):
                b.Client(endpoint, self.fake.bootstrap)
        with self.assertRaises(b.Failure):
            b.NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.invalid/v1")

    def test_endpoint_transport_and_validation_matrix(self):
        https = ["https://example.invalid/v1", "https://127.0.0.1/v1",
                 "https://[::1]:18080/v1/", "https://192.168.1.2/v1"]
        loopback = ["http://127.0.0.1:18080/v1", "http://127.0.0.1/v1/",
                    "http://[::1]:18080/v1", "http://[::1]/v1/"]
        forbidden = [
            "http://localhost:18080/v1", "http://example.invalid/v1",
            "http://192.168.1.2/v1", "http://10.0.0.1/v1", "http://172.16.0.1/v1",
            "http://0.0.0.0/v1", "http://127.0.0.2/v1", "http://127.1/v1",
            "http://2130706433/v1", "http://0177.0.0.1/v1", "http://127.0.0.1./v1",
            "http://%31%32%37.0.0.1/v1", "http://[::]/v1", "http://[fd00::1]/v1",
            "http://[::ffff:127.0.0.1]/v1", "http://[::1%25lo]/v1",
            "http://127.0.0.1.example.invalid/v1", "ftp://127.0.0.1/v1",
        ]
        invalid_shape = [
            f"{base}{suffix}"
            for base in ("https://example.invalid", "http://127.0.0.1:18080", "http://[::1]:18080")
            for suffix in ("/", "/v2", "/v1/docs", "/v1?value=1", "/v1#fragment")
        ] + ["http://user@127.0.0.1/v1", "http://user:password@[::1]/v1",
             "http://127.0.0.1@external.invalid/v1", "https://user@example.invalid/v1"]
        for allow in (False, True):
            for endpoint in https + loopback + forbidden + invalid_shape:
                permitted = endpoint in https or (allow and endpoint in loopback)
                with self.subTest(endpoint=endpoint, allow=allow):
                    if permitted:
                        client = b.Client(endpoint, self.fake.bootstrap, allow_http_loopback=allow)
                        self.assertEqual(client.allow_http_loopback, allow)
                    else:
                        with self.assertRaises(b.Failure):
                            b.Client(endpoint, self.fake.bootstrap, allow_http_loopback=allow)

    def test_loopback_ignores_proxy_environment(self):
        proxy = "http://proxy-must-not-be-used.invalid:9"
        environment = {name: proxy for name in (
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy")}
        environment.update({"NO_PROXY": "", "no_proxy": ""})
        with patch.dict(os.environ, environment):
            client = b.Client(self.endpoint, self.fake.bootstrap, allow_http_loopback=True)
            b.preflight(client, self.fake.org, self.report)
        self.assertTrue(self.report["can_provision_inferred"])

    def test_cli_loopback_flag_allows_read_only_check(self):
        with patch.object(b, "prompt_secret", return_value=self.fake.bootstrap):
            result = b.main(["--endpoint", self.endpoint, "--org", self.fake.org,
                             "--allow-http-loopback"])
        self.assertEqual(result, 0)
        self.assertTrue(self.fake.requests)
        self.assertTrue(all(method == "GET" for method, _ in self.fake.requests))
        self.assert_no_secrets(self.stdout.getvalue())

    def test_cli_error_does_not_print_sensitive_exception(self):
        with patch.object(b, "prompt_secret", side_effect=RuntimeError(self.fake.bootstrap)):
            self.assertEqual(b.main(["--endpoint", "https://example.invalid/v1", "--org", self.fake.org]), 1)
        self.assert_no_secrets(self.stdout.getvalue())

    def test_new_credential_must_authenticate_independently(self):
        self.fake.reject_new_auth = True
        with self.assertRaises(b.ApiFailure) as error:
            self.apply()
        self.assertEqual(error.exception.status, 401)
        self.assertTrue(self.report["recovery_required"])
        self.assertIn(self.output.read_text().strip(), self.fake.new_tokens)

    def test_incompatible_creation_response_retains_recovery_id(self):
        self.fake.bad_creation = True
        with self.assertRaises(b.Failure) as error:
            self.apply()
        self.assertEqual(self.fake.creates, 1)
        self.assertIn("created_key_id", self.report)
        self.assertEqual(self.output.read_text(), "")
        self.assert_no_secrets(str(error.exception) + self.stdout.getvalue())

    def test_invalid_response_envelope_is_rejected(self):
        self.fake.bad_envelope = True
        with self.assertRaises(b.Failure):
            self.apply()
        self.assertEqual(self.fake.creates, 0)

    def test_check_confirms_organization_when_already_permitted(self):
        self.fake.permissions.add("getOrg")
        b.preflight(self.client, self.fake.org, self.report)
        self.assertTrue(self.report["organization_existence_confirmed"])
        self.assertTrue(all(method == "GET" for method, _ in self.fake.requests))

    def test_real_terminal_prompt_hides_synthetic_secret_and_reads_confirmation(self):
        code = ("import runpy; n=runpy.run_path(" + repr(str(Path(b.__file__).resolve())) + "); "
                "s=n['prompt_secret'](); a=n['prompt_confirmation']('Confirm: '); "
                "print('PROMPT_OK' if s and a == 'approve' else 'PROMPT_BAD')")
        pid, master = pty.fork()
        if pid == 0:
            os.execv(sys.executable, [sys.executable, "-c", code])
        output = bytearray()
        def read_until(marker):
            deadline = time.monotonic() + 10
            while marker not in output:
                if time.monotonic() >= deadline:
                    self.fail("Terminal prompt timed out")
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError:
                        self.fail("Terminal child exited before completing prompts")
                    if not chunk:
                        self.fail("Terminal child closed before completing prompts")
                    output.extend(chunk)
        try:
            read_until(b"(hidden): ")
            os.write(master, self.fake.bootstrap.encode() + b"\n")
            read_until(b"Confirm: ")
            self.assertTrue(self.fake.bootstrap.encode() not in output)
            os.write(master, b"approve\n")
            read_until(b"PROMPT_OK")
            self.assertTrue(self.fake.bootstrap.encode() not in output)
        finally:
            os.close(master)
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)

    def test_getpass_echo_fallback_is_refused(self):
        with patch("builtins.open", side_effect=OSError("No terminal")):
            with self.assertRaises(OSError):
                b.prompt_secret()


if __name__ == "__main__":
    unittest.main()
