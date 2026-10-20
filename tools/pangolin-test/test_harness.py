"""Offline fake Pangolin API; runtime-generated credentials only, no live infrastructure."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import secrets
import stat
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch
import urllib.parse

spec = importlib.util.spec_from_file_location("harness", Path(__file__).with_name("harness.py"))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)
BACKEND_HOST = "backend.example.invalid"


class FakeAPI:
    def __init__(self):
        self.credential = secrets.token_hex(8) + "." + secrets.token_hex(24)
        self.org = "example-org"
        self.sites = {}
        self.resources = {10: {"resourceId": 10, "name": "Production", "fullDomain": BACKEND_HOST,
            "domainId": "test-domain", "orgId": self.org, "mode": "http", "sso": True,
            "resourcePolicyId": None, "defaultResourcePolicyId": 100}}
        self.targets = {11: {"targetId": 11, "resourceId": 10, "siteId": 12,
            "ip": "192.168.90.123", "port": 443, "method": "https", "enabled": True}}
        self.domains = [{"domainId": "test-domain", "baseDomain": "example.invalid",
                         "verified": True, "configManaged": True, "failed": False}]
        self.calls = []
        self.root = False
        self.denied = None
        self.site_online = True
        self.target_healthy = True
        self.malformed = False
        self.lost = None
        self.next_id = 1000

    def handle(self, method, path, body, credential):
        parsed = urllib.parse.urlsplit(path)
        path = parsed.path.removeprefix("/v1")
        query = urllib.parse.parse_qs(parsed.query)
        self.calls.append((method, path, body))
        if credential != self.credential:
            return 401, {"secret": credential}
        if self.denied and self.denied in path:
            return 403, {"secret": credential}
        if path.startswith("/org/homestack-root-probe-"):
            return (404 if self.root else 403), {}
        if method == "GET" and path == "/org/example-org":
            return 200, {"org": {"orgId": "example-org"}}
        if method == "GET" and path == "/org/example-org/sites":
            rows = []
            for site in self.sites.values():
                resource_ids = {t["resourceId"] for t in self.targets.values() if t["siteId"] == site["siteId"]}
                rows.append({**site, "resourceCount": len(resource_ids)})
            return 200, self.page(rows, "sites", query, True)
        if method == "GET" and path == "/org/example-org/resources":
            return 200, self.page(list(self.resources.values()), "resources", query, True)
        if method == "GET" and path == "/org/example-org/domains":
            return 200, self.page(self.domains, "domains", query, False)
        if method == "GET" and path.startswith("/org/example-org/site/"):
            rows = [s for s in self.sites.values() if s["niceId"] == path.split("/")[-1]]
            return (200, rows[0]) if rows else (404, {})
        if method == "GET" and path.endswith("/targets"):
            rid = int(path.split("/")[2])
            return 200, self.page([t for t in self.targets.values() if t["resourceId"] == rid], "targets", query, False)
        if method == "PUT":
            self.next_id += 1
            oid = self.next_id
            if path == "/org/example-org/site":
                row = {**body, "siteId": oid, "orgId": "example-org", "online": self.site_online}
                self.sites[oid] = row
                kind = "site"
            elif path == "/org/example-org/resource":
                row = {**body, "resourceId": oid, "orgId": "example-org", "resourcePolicyId": None,
                       "defaultResourcePolicyId": oid + 5000, "sso": True,
                       "fullDomain": body["subdomain"] + "." + next(d["baseDomain"] for d in self.domains if d["domainId"] == body["domainId"]), "enabled": True}
                self.resources[oid] = row
                kind = "resource"
            elif path.startswith("/resource/") and path.endswith("/target"):
                row = {**body, "targetId": oid, "resourceId": int(path.split("/")[2]),
                       "hcHealth": "healthy" if self.target_healthy else "unknown"}
                self.targets[oid] = row
                kind = "target"
            else:
                return 404, {}
            if self.lost == kind:
                self.lost = None
                return 500, {"secret": self.credential}
            return 201, row
        parts = path.strip("/").split("/")
        if len(parts) == 2 and parts[0] in ("site", "resource", "target"):
            collection = {"site": self.sites, "resource": self.resources, "target": self.targets}[parts[0]]
            oid = int(parts[1])
            if oid not in collection:
                return 404, {}
            if method == "GET":
                return 200, collection[oid]
            if method == "POST":
                collection[oid].update(body)
                return 200, collection[oid]
            if method == "DELETE":
                if oid in (10, 11, 12):
                    raise AssertionError("Production mutation attempted")
                del collection[oid]
                return 200, None
        return 404, {}

    @staticmethod
    def page(rows, field, query, paged):
        limit = int(query.get("pageSize" if paged else "limit", [100])[0])
        page = int(query.get("page", [1])[0])
        offset = (page - 1) * limit if paged else int(query.get("offset", [0])[0])
        pagination = {"page": page, "pageSize": limit} if paged else {"offset": offset, "limit": limit}
        return {field: rows[offset:offset + limit], "pagination": {**pagination, "total": len(rows)}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.dispatch()

    def do_PUT(self):
        self.dispatch()

    def do_POST(self):
        self.dispatch()

    def do_DELETE(self):
        self.dispatch()

    def dispatch(self):
        if self.path == "/health/live":
            raw = b'{"status":"ok"}'
            status = 200
        else:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"]))) if self.headers.get("Content-Length") else None
            credential = self.headers.get("Authorization", "").removeprefix("Bearer ")
            status, data = self.server.fake.handle(self.command, self.path, body, credential)
            raw = b"invalid" if self.server.fake.malformed else json.dumps({"success": status < 400,
                "error": status >= 400, "data": data}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class FakeConnector:
    starts = 0
    def __init__(self, binary, work, credentials):
        self.credentials = credentials
    def __enter__(self):
        type(self).starts += 1
        return self
    def alive(self):
        pass
    def __exit__(self, *_):
        pass


class FakeDns:
    def __init__(self):
        self.records = {}
        self.creates = 0
        self.removed = []

    def prepare(self, name, base_domain, existing=None):
        return dict(existing) if existing else {
            "provider": "ovh", "name": name, "zone": base_domain,
            "fieldType": "CNAME", "subDomain": name[:-len(base_domain) - 1],
            "target": "dashboard.example.invalid.", "ttl": 60, "id": None, "pending": False}

    def inspect(self, plan, allow_missing=False):
        return self.records.get(plan.get("id"))

    def ensure(self, plan, save):
        if plan.get("id") is None:
            self.creates += 1
            plan["id"] = self.creates
            self.records[plan["id"]] = dict(plan)
        plan["published"] = True
        save()

    def remove(self, plan, save):
        if plan.get("id") in self.records:
            self.removed.append(plan["id"])
            del self.records[plan["id"]]
        plan["deleted"] = True
        save()


class HarnessTests(unittest.TestCase):
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
        self.fake = FakeAPI()
        self.server.fake = self.fake
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}/v1"
        self.api = h.Client(self.endpoint, self.fake.credential)
        self.temp = tempfile.TemporaryDirectory(prefix="homestack-pangolin-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = h.Workspace(Path(self.temp.name) / "private")
        self.work.__enter__()
        self.addCleanup(self.work.__exit__, None, None, None)
        self.deploy = Path(self.temp.name) / "deploy.toml"
        self.deploy.write_text('domain="example.invalid"\n[[instances]]\nname="backend"\naddress="192.168.90.123"\n')
        self.client_config = Path(self.temp.name) / "client.toml"
        self.client_config.write_text('[server]\nurls=["https://backend.example.invalid"]\ntoken_file="never-open-this-file"\n')
        self.approvals = []
        self.experiment = h.Experiment(self.api, "example-org", "https://dashboard.example.invalid", self.work, self.approvals.append)
        self.probes = []
        self.dns_provider = FakeDns()
        self.real_require_test_dns = h.require_test_dns
        dns_patch = patch.object(h, "require_test_dns")
        self.dns_check = dns_patch.start()
        self.addCleanup(dns_patch.stop)
    def probe(self, host, port, path, **kwargs):
        self.probes.append((host, port, path, kwargs))
        if kwargs.get("ip"):
            return {"status": 200, "healthy": True, "location": ""}
        return {"status": 302, "healthy": False, "location": "https://dashboard.example.invalid/auth/resource/test"}
    def run_experiment(self, **kwargs):
        return self.experiment.run(self.deploy, self.client_config, "backend", probe=kwargs.pop("probe", self.probe),
            locate_cli=lambda _: Path("/synthetic/pangolin"), connector=FakeConnector,
            wait_seconds=0, interval=0, dns_seconds=0, dns_factory=lambda *_: self.dns_provider, **kwargs)
    def assert_production_untouched(self):
        self.assertEqual(self.fake.resources[10]["name"], "Production")
        for method, path, _ in self.fake.calls:
            if method != "GET":
                self.assertNotIn(path, ("/resource/10", "/target/11", "/site/12"))
    def test_success_online_target_health_and_protected_route(self):
        def approve(plan_text):
            if not self.approvals:
                self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
                self.assertTrue(self.probes)
                plan = json.loads(plan_text)
                self.assertEqual(plan["org"], "example-org")
                self.assertEqual(plan["site"], h.SITE_NAME)
                self.assertEqual(plan["resource"], self.experiment.state["resource_name"])
                self.assertTrue(plan["hostname_unused_in_organization"])
                self.assertEqual(plan["public_url"], "https://" + self.experiment.state["fqdn"] + h.HEALTH_PATH)
                self.assertEqual(plan["target_url"], "https://192.168.90.123:443/health/live")
                self.assertEqual(plan["health_check_path"], h.HEALTH_PATH)
                self.assertEqual(plan["authentication_mode"], "Pangolin SSO; no bypass")
                self.assertEqual(len(plan["objects_to_create"]), 5)
                self.assertEqual(self.dns_provider.creates, 0)
                self.assertEqual(plan["dns_record"]["fieldType"], "CNAME")
                self.assertTrue(any(path.endswith("/domains") for _, path, _ in self.fake.calls))
            self.approvals.append(plan_text)
        self.experiment.approve = approve
        report = self.run_experiment()
        self.assertEqual(report["stage"], "partial_authenticated_edge_health_unavailable")
        self.assertIn("Online", report["site_connectivity"])
        self.assertIn("healthy", report["target_health"])
        self.assertIn("/health/live", report["target_route"])
        self.assertTrue(report["public_checks"]["/v1/status"]["authentication_required"])
        self.assertIn("not exercised", report["unresolved"])
        self.assertEqual(len(self.approvals), 2)
        self.assertFalse(any(path == "/org/example-org" or "root-probe" in path for _, path, _ in self.fake.calls))
        target = self.fake.targets[report["target_id"]]
        self.assertEqual(target["pathMatchType"], "exact")
        self.assertEqual(target["path"], "/health/live")
        self.assertTrue(self.fake.resources[report["resource_id"]]["sso"])
        self.assert_production_untouched()
    def test_repeated_run_creates_no_duplicates(self):
        self.run_experiment()
        before = sum(method == "PUT" for method, _, _ in self.fake.calls)
        self.experiment = h.Experiment(self.api, "example-org", self.experiment.server, self.work, self.approvals.append)
        self.run_experiment()
        self.assertEqual(sum(method == "PUT" for method, _, _ in self.fake.calls), before)
    def test_cleanup_only_recorded_objects_and_repeat_is_noop(self):
        self.run_experiment()
        self.assertTrue(self.experiment.cleanup()["cleaned"])
        self.assertEqual(set(self.fake.resources), {10})
        self.assertEqual(set(self.fake.targets), {11})
        self.assertFalse(self.fake.sites)
        self.assertFalse((self.work.path / "site-credentials.json").exists())
        before = len(self.fake.calls)
        self.experiment.cleanup()
        self.assertEqual(len(self.fake.calls), before)
        self.assert_production_untouched()
    def test_unrecorded_site_is_never_adopted(self):
        self.fake.sites[999] = {"siteId": 999, "name": h.SITE_NAME, "niceId": "foreign", "newtId": secrets.token_hex(8), "orgId": "example-org", "type": "newt"}
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.assertFalse(any(m != "GET" for m, _, _ in self.fake.calls))
    def test_invalid_credentials_and_missing_permissions_are_sanitized(self):
        self.api.credential = secrets.token_hex(8) + "." + secrets.token_hex(24)
        with self.assertRaises(h.ApiFailure) as error:
            self.run_experiment()
        self.assertNotIn(self.api.credential, str(error.exception))
        self.api.credential = self.fake.credential
        self.fake.denied = "/domains"
        with self.assertRaises(h.ApiFailure):
            self.run_experiment()
        self.assertFalse(any(m == "PUT" and p != "/org/example-org/site" for m, p, _ in self.fake.calls))
    def test_missing_deploy_config_stops_before_mutations(self):
        self.deploy.unlink()
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("deploy.toml", str(error.exception))
        self.assertFalse(self.fake.sites)
        self.assertFalse(self.approvals)
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
    def test_backend_ambiguity_or_unreachability_stops(self):
        self.client_config.write_text('[server]\nurls=["https://backend.example.invalid", "https://backend.example.invalid:8443"]\n')
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.client_config.write_text('[server]\nurls=["https://backend.example.invalid"]\n')
        with self.assertRaises(h.Failure):
            self.run_experiment(probe=lambda *_args, **_kwargs: {"healthy": False})
        self.assertFalse(self.approvals)
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
    def test_missing_domain_stops(self):
        self.fake.domains.clear()
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.assertFalse(self.approvals)
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
    def test_hostname_conflict_stops_before_approval(self):
        # Reserve only local recovery state by declining the first proposal.
        def reject(_):
            raise h.Failure("Not approved")
        self.experiment.approve = reject
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.fake.resources[999] = dict(self.fake.resources[10], resourceId=999,
            fullDomain=self.experiment.state["fqdn"])
        self.experiment.approve = self.approvals.append
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("hostname/name conflicts", str(error.exception))
        self.assertFalse(self.approvals)
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
    def test_confirmation_rejection_stops_before_live_writes(self):
        def reject(_):
            raise h.Failure("Not approved")
        self.experiment.approve = reject
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.assertFalse(any(m != "GET" for m, _, _ in self.fake.calls))
    def test_lost_create_responses_reconcile_without_duplicates(self):
        for kind in ("site", "resource", "target"):
            with self.subTest(kind=kind):
                # Each subcase uses its own clean experiment state and fake inventory.
                self.fake = FakeAPI()
                self.server.fake = self.fake
                self.api = h.Client(self.endpoint, self.fake.credential)
                for name in ("state.json", "site-credentials.json"):
                    (self.work.path / name).unlink(missing_ok=True)
                self.experiment = h.Experiment(self.api, "example-org", "https://dashboard.example.invalid", self.work, self.approvals.append)
                self.fake.lost = kind
                with self.assertRaises(h.ApiFailure):
                    self.run_experiment()
                self.assertEqual(self.experiment.state["pending"], kind)
                self.experiment = h.Experiment(self.api, "example-org", self.experiment.server, self.work, self.approvals.append)
                self.run_experiment()
                self.assertEqual(len(self.fake.sites), 1)
                self.assertEqual(len(self.fake.resources), 2)
                self.assertEqual(len(self.fake.targets), 2)
                self.assert_production_untouched()
    def test_unknown_pending_outcome_never_retries_creation(self):
        self.fake.lost = "site"
        with self.assertRaises(h.ApiFailure):
            self.run_experiment()
        self.fake.sites.clear()
        before = sum(m == "PUT" for m, _, _ in self.fake.calls)
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.assertEqual(sum(m == "PUT" for m, _, _ in self.fake.calls), before)
    def test_online_and_health_deadlines(self):
        self.fake.site_online = False
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("Online", str(error.exception))
        self.assertFalse(self.experiment.state.get("resource_id"))
        for row in self.fake.sites.values():
            row["online"] = True
        self.fake.target_healthy = False
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("health", str(error.exception))
    def test_unprotected_public_response_disables_only_test_resource(self):
        def probe(host, port, path, **kwargs):
            if kwargs.get("ip"):
                return self.probe(host, port, path, **kwargs)
            return {"status": 200, "healthy": True}
        with self.assertRaises(h.Failure):
            self.run_experiment(probe=probe)
        self.assertFalse(self.fake.resources[self.experiment.state["resource_id"]]["enabled"])
        self.assert_production_untouched()
    def test_missing_non_health_route_preserves_protected_resource(self):
        def probe(host, port, path, **kwargs):
            if kwargs.get("ip"):
                return self.probe(host, port, path, **kwargs)
            return {"status": 401 if path == h.HEALTH_PATH else 404}
        report = self.run_experiment(probe=probe)
        self.assertTrue(report["public_checks"][h.HEALTH_PATH]["authentication_required"])
        self.assertEqual(report["public_checks"]["/v1/status"], {"status": 404, "route_available": False})
        self.assertTrue(self.fake.resources[report["resource_id"]]["enabled"])
        self.assert_production_untouched()
    def test_missing_health_route_still_fails_authentication_proof(self):
        def probe(host, port, path, **kwargs):
            if kwargs.get("ip"):
                return self.probe(host, port, path, **kwargs)
            return {"status": 404}
        with self.assertRaises(h.Failure):
            self.run_experiment(probe=probe)
        self.assertFalse(self.fake.resources[self.experiment.state["resource_id"]]["enabled"])
        self.assert_production_untouched()
    def test_cleanup_refuses_foreign_target_or_site_attachment(self):
        self.run_experiment()
        rid = self.experiment.state["resource_id"]
        tid = self.experiment.state["target_id"]
        self.fake.targets[999] = dict(self.fake.targets[tid], targetId=999)
        with self.assertRaises(h.Failure):
            self.experiment.cleanup()
        self.assertIn(rid, self.fake.resources)
        self.fake.targets[999]["resourceId"] = 10
        with self.assertRaises(h.Failure):
            self.experiment.cleanup()
        self.assertIn(tid, self.fake.targets)
    def test_cleanup_identity_tampering_is_rejected(self):
        self.run_experiment()
        rid = self.experiment.state["resource_id"]
        self.fake.resources[rid]["fullDomain"] = BACKEND_HOST
        before = len(self.fake.calls)
        with self.assertRaises(h.Failure):
            self.experiment.cleanup()
        self.assertTrue(all(m == "GET" for m, _, _ in self.fake.calls[before:]))
    def test_reports_and_state_never_contain_credentials(self):
        report = self.run_experiment()
        credentials = self.work.load("site-credentials.json")
        serialized = json.dumps(report) + (self.work.path / "state.json").read_text() + "".join(self.approvals)
        for value in (self.fake.credential, self.fake.credential.split(".")[1], credentials["id"], credentials["secret"]):
            self.assertTrue(value not in serialized)
        self.assertEqual(stat.S_IMODE(self.work.path.stat().st_mode), 0o700)
        for name in ("state.json", "site-credentials.json"):
            self.assertEqual(stat.S_IMODE((self.work.path / name).stat().st_mode), 0o600)
    def test_workspace_lock_and_unsafe_paths(self):
        with self.assertRaises(BlockingIOError):
            with h.Workspace(self.work.path):
                self.fail("Concurrent lock unexpectedly succeeded")
        for path in (Path("relative"), Path(__file__).parent):
            with self.assertRaises(h.Failure):
                with h.Workspace(path):
                    self.fail("Unsafe workspace accepted")
        link = Path(self.temp.name) / "link"
        link.symlink_to(self.work.path, target_is_directory=True)
        with self.assertRaises(OSError):
            with h.Workspace(link):
                self.fail("Symlink accepted")
    def test_secret_file_requires_private_mode_and_plain_token(self):
        path = Path(self.temp.name) / "credential"
        path.write_text(self.fake.credential)
        path.chmod(0o644)
        with self.assertRaises(h.Failure):
            h.read_private(path)
        path.chmod(0o600)
        self.assertEqual(h.read_private(path), self.fake.credential)
        with self.assertRaises(h.Failure):
            h.Client(self.endpoint, json.dumps({"apiKey": self.fake.credential}))
    def test_http_security_and_host_validation(self):
        for url in ("http://localhost/v1", "http://192.168.1.1/v1", "http://127.1/v1", "http://[::ffff:127.0.0.1]/v1",
                    "http://user@127.0.0.1/v1", "http://127.0.0.1/v1?secret=x", "http://127.0.0.1/v1#x", "http://127.0.0.1/"):
            with self.subTest(url=url), self.assertRaises(h.Failure):
                h.Client(url, self.fake.credential)
        h.Client("http://[::1]:18080/v1", self.fake.credential)
        with self.assertRaises(h.Failure):
            h.NoRedirect().redirect_request(None)
    def test_proxy_variables_do_not_affect_loopback_requests(self):
        env = {key: "http://proxy.invalid:9" for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy")}
        env.update({"NO_PROXY": "", "no_proxy": ""})
        with patch.dict(os.environ, env):
            api = h.Client(self.endpoint, self.fake.credential)
            self.assertEqual(api.get("/org/example-org")["org"]["orgId"], "example-org")
    def test_malformed_response_is_sanitized(self):
        self.fake.malformed = True
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertNotIn(self.fake.credential, str(error.exception))
    def test_direct_health_probe_uses_fake_loopback_only(self):
        result = h.http_probe("test.invalid", self.server.server_port, "/health/live", ip="127.0.0.1", scheme="http")
        self.assertTrue(result["healthy"])
    def test_dns_propagation_waits_before_site_creation(self):
        self.dns_check.side_effect = h.Failure("Test hostname has no DNS record.")
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("DNS propagation", str(error.exception))
        self.assertEqual(len(self.approvals), 1)
        self.assertEqual(self.dns_provider.creates, 1)
        self.assertIsNotNone(self.experiment.state["dns"]["id"])
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))

    def test_dns_nxdomain_is_specific_and_sanitized(self):
        sensitive = secrets.token_hex(24)
        failure = h.socket.gaierror(h.socket.EAI_NONAME, sensitive)
        with patch.object(h.socket, "getaddrinfo", side_effect=failure):
            with self.assertRaises(h.Failure) as error:
                self.real_require_test_dns("test.example.invalid")
        self.assertIn("does not exist in desktop DNS", str(error.exception))
        self.assertNotIn(sensitive, str(error.exception))

    def test_probe_errors_identify_failure_without_sensitive_exception_text(self):
        sensitive = secrets.token_hex(24)
        cases = [
            (h.socket.gaierror(sensitive), "DNS resolution"),
            (h.ssl.SSLCertVerificationError(sensitive), "TLS certificate verification"),
            (h.ssl.SSLError(sensitive), "TLS handshake"),
            (TimeoutError(sensitive), "timed out"),
            (ConnectionRefusedError(sensitive), "connection was refused"),
            (OSError(sensitive), "network connection"),
            (h.http.client.RemoteDisconnected(sensitive), "connection closed prematurely"),
        ]
        for failure, expected in cases:
            with self.subTest(kind=type(failure).__name__):
                with patch.object(h.http.client, "HTTPSConnection") as connection:
                    connection.return_value.request.side_effect = failure
                    with self.assertRaises(h.Failure) as error:
                        h.http_probe("test.invalid", 443, "/health/live")
                self.assertIn(expected, str(error.exception))
                self.assertNotIn(sensitive, str(error.exception))
    def test_connector_config_is_private_and_secrets_are_not_in_argv_or_environment(self):
        generated = {"id": secrets.token_hex(8), "secret": secrets.token_hex(24), "endpoint": "https://dashboard.example.invalid"}
        observed = {}
        class Process:
            def poll(self):
                return None
            def terminate(self):
                observed["terminated"] = True
            def kill(self):
                pass
            def wait(self, **kwargs):
                pass
        def launch(argv, **kwargs):
            config = Path(argv[-1])
            observed["path"] = config
            self.assertEqual(argv[1:4], ["up", "site", "--config-file"])
            self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
            data = json.loads(config.read_text())
            self.assertEqual(data["secret"], generated["secret"])
            self.assertTrue(data["disableSsh"])
            self.assertFalse(data["nativeMain"])
            for value in generated.values():
                self.assertTrue(value not in json.dumps(argv))
                self.assertTrue(value not in json.dumps(kwargs["env"]))
            self.assertEqual(kwargs["stdout"], h.subprocess.DEVNULL)
            self.assertEqual(kwargs["stderr"], h.subprocess.DEVNULL)
            return Process()
        with patch.object(h.subprocess, "Popen", side_effect=launch):
            with h.Connector(Path("/synthetic/pangolin"), self.work, generated) as process:
                process.alive()
        self.assertTrue(observed["terminated"])
        self.assertFalse(observed["path"].exists())
    def test_official_cli_requires_expected_digest(self):
        binary = self.work.path / "pangolin-test-amd64"
        content = b"synthetic executable content"
        binary.write_bytes(content)
        digest = hashlib.sha256(content).hexdigest()
        with patch.object(h, "CLI_VERSION", "test"), patch.dict(h.ARTIFACTS, {"x86_64": ("amd64", digest)}), \
                patch.object(h.platform, "machine", return_value="x86_64"), patch.object(h.platform, "system", return_value="Linux"):
            self.assertEqual(h.official_cli(self.work), binary)
            binary.write_bytes(b"modified")
            class Reply(io.BytesIO):
                pass
            with patch.object(h.urllib.request, "build_opener") as opener:
                opener.return_value.open.return_value = Reply(b"wrong download")
                with self.assertRaises(h.Failure):
                    h.official_cli(self.work)
            self.assertEqual(binary.read_bytes(), b"modified")
    def test_cleanup_without_state_performs_no_calls(self):
        self.assertEqual(self.experiment.cleanup()["cleanup"], "nothing recorded to remove")
        self.assertFalse(self.fake.calls)
    def test_cleanup_can_remove_owned_resource_after_sso_changed(self):
        self.run_experiment()
        self.fake.resources[self.experiment.state["resource_id"]]["sso"] = False
        self.assertTrue(self.experiment.cleanup()["cleaned"])
        self.assert_production_untouched()

    def test_cleanup_recovers_interrupted_deletion(self):
        self.run_experiment()
        original = self.api.request
        interrupted = False
        def request(method, path, body=None):
            nonlocal interrupted
            result = original(method, path, body)
            if method == "DELETE" and not interrupted:
                interrupted = True
                raise KeyboardInterrupt()
            return result
        with patch.object(self.api, "request", side_effect=request):
            with self.assertRaises(KeyboardInterrupt):
                self.experiment.cleanup()
        self.assertTrue(self.experiment.cleanup()["cleaned"])
        self.assert_production_untouched()

    def test_run_rejects_corrupted_ownership_markers(self):
        self.run_experiment()
        self.experiment.state["resource_name"] = "Production"
        self.experiment.save()
        with self.assertRaises(h.Failure):
            h.Experiment(self.api, "example-org", self.experiment.server, self.work)

    def test_changed_backend_prevents_retargeting(self):
        self.run_experiment()
        self.client_config.write_text('[server]\nurls=["https://backend.example.invalid:8443"]\n')
        with self.assertRaises(h.Failure):
            self.run_experiment()
    def test_discovery_without_existing_pangolin_backend_resource(self):
        self.fake.resources.clear()
        self.fake.targets.clear()
        self.fake.domains[0].update({"domainId": "domain1", "baseDomain": "tunnel.example.invalid"})
        def approve_after_discovery(plan_text):
            if not self.approvals:
                plan = json.loads(plan_text)
                self.assertEqual(plan["selected_domain"], "tunnel.example.invalid")
                self.assertTrue(plan["public_url"].endswith(".tunnel.example.invalid/health/live"))
                self.assertEqual(plan["target_url"], "https://192.168.90.123:443/health/live")
                self.assertTrue(plan["backend_reachable_from_desktop"])
                self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
            self.approvals.append(plan_text)
        self.experiment.approve = approve_after_discovery
        report = self.run_experiment()
        self.assertEqual(len(self.approvals), 2)
        self.assertEqual(len(self.fake.sites), 1)
        self.assertEqual(len(self.fake.resources), 1)
        self.assertEqual(len(self.fake.targets), 1)
        self.assertTrue(report["public_route"].endswith(".tunnel.example.invalid/health/live"))

    def test_configured_https_port_is_used_without_guessing(self):
        self.client_config.write_text('[server]\nurls=["https://backend.example.invalid:8443"]\n')
        def reject(plan_text):
            plan = json.loads(plan_text)
            self.assertEqual(plan["target_url"], "https://192.168.90.123:8443/health/live")
            self.assertEqual(self.probes[0][1], 8443)
            self.approvals.append(plan_text)
            raise h.Failure("Stop before mutation")
        self.experiment.approve = reject
        with self.assertRaises(h.Failure):
            self.run_experiment()
        self.assertEqual(len(self.approvals), 1)
        self.assertFalse(self.fake.sites)

    def test_private_settings_load_without_environment_defaults(self):
        path = self.work.path / "settings.toml"
        path.write_text('api_url="' + self.endpoint + '"\norganization="example-org"\nserver_url="https://dashboard.example.invalid"\nbackend_instance="backend"\n')
        path.chmod(0o600)
        settings = h.load_settings(path)
        self.assertEqual(settings["organization"], "example-org")
        self.assertEqual(settings["api_url"], self.endpoint)
        with self.assertRaises(h.Failure):
            h.load_settings(Path(__file__).with_name("private-settings.toml"))

    def test_missing_client_config_stops_before_approval(self):
        self.client_config.unlink()
        with self.assertRaises(h.Failure) as error:
            self.run_experiment()
        self.assertIn("client.toml", str(error.exception))
        self.assertFalse(self.approvals)
        self.assertTrue(all(method == "GET" for method, _, _ in self.fake.calls))
    def test_no_broad_or_untrusted_redirect_counts_as_authentication(self):
        self.run_experiment()
        for location in ("", "/unrelated", "https://external.invalid/auth/resource/x"):
            self.assertFalse(self.experiment.protected({"status": 302, "location": location}))


if __name__ == "__main__":
    unittest.main()
