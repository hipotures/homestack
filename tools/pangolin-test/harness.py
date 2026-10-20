#!/usr/bin/env python3
"""Manual-only Pangolin v1.24 local experiment. Python 3.11+, Linux, no root."""
import argparse
import contextlib
import fcntl
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import resource
import secrets
import shutil
import signal
import socket
import ssl
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from ovh_dns import DnsFailure, OvhDns
from connector import PersistentConnector, process_info

SITE_NAME = "homestack-test-desktop"
HEALTH_PATH = "/health/live"
CLI_VERSION = "0.18.1"
ARTIFACTS = {
    "x86_64": ("amd64", "07ae60ba0da7f3d25da6b78b76f0f2f40069605c2c52340f2f364e3cef72053f"),
    "aarch64": ("arm64", "27a0ce04bb23bff83ed51638902034577ae71f460fe9a01ae0d0de035eb10118"),
}
NAME = re.compile(r"[a-z0-9_-]{1,100}\Z")
TOKEN = re.compile(r"[A-Za-z0-9_-]{1,128}\.[A-Za-z0-9_-]{1,512}\Z")
DNS = re.compile(r"(?=.{1,253}\Z)[a-z0-9]+(?:[a-z0-9.-]*[a-z0-9])?\Z")


class Failure(DnsFailure):
    """Messages must be locally authored; never include raw exceptions or replies."""


class ApiFailure(Failure):
    def __init__(self, status):
        self.status = status
        super().__init__(f"Integration API returned HTTP {status}; response omitted.")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise Failure("Redirect refused; use the final Integration API URL.")


def positive(value):
    if type(value) is not int or value <= 0:
        raise Failure("Expected a positive object ID; incompatible API response.")
    return value


def identifier(value):
    if not isinstance(value, str) or not NAME.fullmatch(value):
        raise Failure("Invalid non-secret identifier.")
    return value


def hostname(value):
    if not isinstance(value, str) or not DNS.fullmatch(value) or ".." in value:
        raise Failure("Invalid domain metadata.")
    return value


def read_private(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "r") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600):
            raise Failure("Secret/state files must be regular, owned by this user, with mode 0600.")
        text = stream.read(1024 * 1024 + 1)
        if len(text) > 1024 * 1024:
            raise Failure("Secret/state file exceeds the size limit.")
        return text


def load_settings(path):
    """Read environment metadata only from an explicitly local private file."""
    resolved = Path(path).resolve()
    repo = Path(__file__).resolve().parents[2]
    if resolved.is_relative_to(repo) or any(
            (p / ".git").is_file() or (p / ".git" / "HEAD").is_file()
            for p in resolved.parents):
        raise Failure("Environment settings must stay outside Git repositories.")
    try:
        settings = tomllib.loads(read_private(path))
    except (OSError, ValueError):
        raise Failure("Missing/invalid local pangolin-test.toml; create the private settings file outside Git before running.") from None
    fields = {"api_url", "organization", "server_url", "backend_instance"}
    if set(settings) != fields or not all(isinstance(value, str) and value for value in settings.values()):
        raise Failure("Local settings require exactly api_url, organization, server_url and backend_instance string fields. Store the token separately.")
    identifier(settings["organization"])
    identifier(settings["backend_instance"])
    return settings


class Client:
    def __init__(self, endpoint, credential):
        parts = urllib.parse.urlsplit(endpoint)
        transport = parts.scheme == "https" or (
            parts.scheme == "http" and parts.hostname in ("127.0.0.1", "::1"))
        if (not transport or not parts.hostname or parts.username is not None
                or parts.password is not None or parts.query or parts.fragment
                or parts.path.rstrip("/") != "/v1"):
            raise Failure("Integration API must use HTTPS or literal loopback HTTP, end in /v1, and contain no credentials/query/fragment.")
        if not TOKEN.fullmatch(credential):
            raise Failure("Credential file must contain only the organization key in ID.secret format, not JSON.")
        self.endpoint = endpoint.rstrip("/")
        self.credential = credential
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(),
            urllib.request.HTTPSHandler(context=ssl.create_default_context(), debuglevel=0))

    def request(self, method, path, body=None):
        request = urllib.request.Request(self.endpoint + path, method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + self.credential, "Content-Type": "application/json"})
        try:
            with self.opener.open(request, timeout=8) as response:
                data = response.read(4 * 1024 * 1024 + 1)
            if len(data) > 4 * 1024 * 1024:
                raise Failure("Integration API response exceeds the size limit.")
            data = json.loads(data)
            if (not isinstance(data, dict) or data.get("success") is not True
                    or data.get("error") is not False
                    or (data.get("data") is not None and not isinstance(data["data"], dict))):
                raise Failure("Unexpected Integration API envelope; response omitted.")
            return data.get("data")
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
            raise ApiFailure(status) from None
        except (OSError, ValueError, http.client.HTTPException, urllib.error.URLError):
            raise Failure("Integration API transport/JSON failure; response omitted. Write outcome may be unknown.") from None

    def get(self, path):
        data = self.request("GET", path)
        if not isinstance(data, dict):
            raise Failure("Unexpected Integration API data; response omitted.")
        return data

    def optional(self, path):
        try:
            return self.get(path)
        except ApiFailure as error:
            if error.status == 404:
                return None
            raise

    def rows(self, path, field, paged=False):
        result = []
        for index in range(1000):
            query = f"page={index + 1}&pageSize=100" if paged else f"offset={index * 100}&limit=100"
            data = self.get(path + "?" + query)
            rows = data.get(field)
            pagination = data.get("pagination", {})
            if (not isinstance(rows, list) or len(rows) > 100
                    or not all(isinstance(row, dict) for row in rows)
                    or not isinstance(pagination, dict)):
                raise Failure("Unexpected paginated API response.")
            if paged:
                valid = pagination.get("page") == index + 1 and pagination.get("pageSize") == 100
            else:
                valid = pagination.get("offset") == index * 100 and pagination.get("limit") == 100
            if not valid:
                raise Failure("Incompatible pagination; refusing incomplete ownership evidence.")
            result.extend(rows)
            if len(rows) < 100:
                return result
        raise Failure("Pagination limit reached.")


class Workspace:
    """Private, locked recovery directory outside Git. No original credentials copied here."""
    def __init__(self, path):
        self.path = Path(path).expanduser()
        self.fd = None
        self.lock = None

    def __enter__(self):
        if not self.path.is_absolute() or ".." in self.path.parts:
            raise Failure("State directory must be an absolute path without parent traversal.")
        resolved = self.path.resolve()
        repo = Path(__file__).resolve().parents[2]
        if resolved.is_relative_to(repo) or any(
                (p / ".git").is_file() or (p / ".git" / "HEAD").is_file()
                for p in (resolved, *resolved.parents)):
            raise Failure("State and credentials must stay outside Git repositories.")
        fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
        try:
            for component in self.path.parts[1:]:
                try:
                    os.mkdir(component, 0o700, dir_fd=fd)
                except FileExistsError:
                    pass
                next_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                os.close(fd)
                fd = next_fd
                info = os.fstat(fd)
                sticky = info.st_uid == 0 and info.st_mode & stat.S_ISVTX
                if info.st_uid not in (0, os.getuid()) or (info.st_mode & 0o022 and not sticky):
                    raise Failure("State ancestors are writable by other accounts.")
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise Failure("State directory must be owned by this user with mode 0700.")
            self.fd = fd
            fd = None
            self.lock = os.open("lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600, dir_fd=self.fd)
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise
        finally:
            if fd is not None:
                os.close(fd)

    def load(self, name):
        try:
            return json.loads(read_private(self.path / name))
        except FileNotFoundError:
            return None

    def save(self, name, data):
        fd, temporary = tempfile.mkstemp(prefix=".write-", dir=self.path)
        try:
            with os.fdopen(fd, "w") as stream:
                os.fchmod(stream.fileno(), 0o600)
                json.dump(data, stream, sort_keys=True, indent=2)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path / name)
            os.fsync(self.fd)
        finally:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(temporary)

    def __exit__(self, *_):
        if self.lock is not None:
            os.close(self.lock)
        if self.fd is not None:
            os.close(self.fd)
        self.lock = self.fd = None


def confirm(text):
    print(text, flush=True)
    with open("/dev/tty", "w") as writer, open("/dev/tty", "r") as reader:
        if not reader.isatty() or not writer.isatty():
            raise Failure("Interactive confirmation requires a terminal.")
        print("Type APPROVE to perform only this plan: ", end="", file=writer, flush=True)
        if reader.readline().strip() != "APPROVE":
            raise Failure("Plan not approved; no live mutations performed.")


def http_probe(host, port, path, *, ip=None, scheme="https"):
    """No credentials, proxies, redirects, or raw body diagnostics. Preserve TLS SNI/Host."""
    connection = None
    try:
        if scheme == "https":
            connection = http.client.HTTPSConnection(host, port, timeout=6, context=ssl.create_default_context())
            if ip is not None:
                raw = socket.create_connection((ip, port), timeout=6)
                try:
                    connection.sock = connection._context.wrap_socket(raw, server_hostname=host)
                except BaseException:
                    raw.close()
                    raise
        elif scheme == "http" and ip is not None:
            connection = http.client.HTTPConnection(ip, port, timeout=6)
        else:
            raise Failure("Invalid probe transport.")
        connection.request("GET", path, headers={"Host": host, "Accept": "application/json"})
        response = connection.getresponse()
        body = response.read(65537)
        location = response.getheader("Location", "")
        healthy = False
        if len(body) <= 65536 and response.status == 200:
            with contextlib.suppress(ValueError, UnicodeError):
                parsed_body = json.loads(body)
                healthy = isinstance(parsed_body, dict) and parsed_body.get("status") == "ok"
        return {"status": response.status, "healthy": healthy, "location": location}
    except socket.gaierror:
        raise Failure("Health/routing probe failed: DNS resolution failed for the requested hostname.") from None
    except ssl.SSLCertVerificationError:
        raise Failure("Health/routing probe failed: TLS certificate verification failed for the requested hostname.") from None
    except ssl.SSLError:
        raise Failure("Health/routing probe failed: TLS handshake failed.") from None
    except TimeoutError:
        raise Failure("Health/routing probe failed: connection or response timed out.") from None
    except ConnectionRefusedError:
        raise Failure("Health/routing probe failed: connection was refused.") from None
    except (ValueError, http.client.HTTPException):
        raise Failure("Health/routing probe failed: invalid HTTP response or connection closed prematurely.") from None
    except OSError:
        raise Failure("Health/routing probe failed: network connection failed.") from None
    finally:
        if connection is not None:
            connection.close()


def require_test_dns(host):
    hostname(host)
    try:
        addresses = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
    except socket.gaierror as error:
        if error.errno == socket.EAI_NONAME:
            raise Failure(f"Test hostname {host} does not exist in desktop DNS. A DNS record or matching wildcard is required.") from None
        raise Failure(f"DNS lookup failed for test hostname {host}.") from None
    if not addresses:
        raise Failure(f"Test hostname {host} has no usable DNS address.")


def discover_backend(api, org, deploy_file, client_file, backend_name, probe=http_probe):
    domains = api.rows(f"/org/{org}/domains", "domains")
    available = [d for d in domains if d.get("failed") is False
                 and (d.get("verified") is True or d.get("configManaged") is True)]
    print(json.dumps({"available_organization_domains": [
        {"domain_id": identifier(d.get("domainId")), "base_domain": hostname(d.get("baseDomain"))}
        for d in available]}, indent=2), flush=True)
    if not available:
        raise Failure("No available verified/configuration-managed organization domain; no mutation approval offered.")
    # Parse paths/metadata only. Never follow deploy.toml's references to secret files.
    try:
        with open(deploy_file, "rb") as stream:
            config = tomllib.load(stream)
    except (OSError, ValueError):
        raise Failure("Missing/invalid HomeStack deploy.toml; cannot reliably discover the selected backend's private IP.") from None
    backend_host = hostname(identifier(backend_name) + "." + hostname(config.get("domain")))
    instances = [i for i in config.get("instances", []) if i.get("name") == backend_name]
    if len(instances) != 1:
        raise Failure("Missing or ambiguous selected backend instance in HomeStack deploy.toml.")
    try:
        address = ipaddress.IPv4Address(instances[0]["address"])
    except (KeyError, ValueError):
        raise Failure("Backend private address is missing or invalid in HomeStack configuration.") from None
    private_ranges = [ipaddress.ip_network(value) for value in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
    if not any(address in network for network in private_ranges):
        raise Failure("Backend address is not a private RFC1918 address; refusing guessed/public backend routing.")
    # Use HomeStack's own configured HTTPS origin, not an assumed Pangolin Resource.
    # Do not open client.toml's token_file or send a HomeStack management token.
    try:
        with open(client_file, "rb") as stream:
            client_config = tomllib.load(stream)
        urls = client_config.get("server", {}).get("urls", [])
        if not isinstance(urls, list) or not all(isinstance(url, str) for url in urls):
            raise ValueError()
        matches = [urllib.parse.urlsplit(url) for url in urls
                   if urllib.parse.urlsplit(url).hostname == backend_host]
        if len(matches) != 1:
            raise Failure("HomeStack client.toml must identify exactly one URL for the selected backend; backend scheme/port unavailable.")
        origin = matches[0]
        port = 443 if origin.port is None else origin.port
        if (origin.scheme != "https" or origin.username is not None or origin.password is not None
                or origin.query or origin.fragment or origin.path not in ("", "/")
                or not 1 <= port <= 65535):
            raise Failure("Configured backend server URL must be an HTTPS origin without credentials/query/fragment.")
    except (OSError, ValueError, AttributeError):
        raise Failure("Missing/invalid HomeStack client.toml; cannot discover the selected backend's configured HTTPS port. No token files were opened.") from None
    backend = {"ip": str(address), "port": port, "scheme": origin.scheme, "host": backend_host}
    target_url = f"{backend['scheme']}://{backend['ip']}:{backend['port']}{HEALTH_PATH}"
    try:
        result = probe(backend_host, backend["port"], HEALTH_PATH, ip=backend["ip"], scheme=backend["scheme"])
    except Failure:
        raise Failure(f"Backend probe failed for {target_url} (Host: {backend_host}): desktop connection/TLS/protocol validation failed; no mutation approval offered.") from None
    if result.get("healthy") is not True:
        raise Failure(f"Backend {target_url} (Host: {backend_host}) did not return HTTP 200 with JSON status ok; no mutation approval offered.")
    preferred = [d for d in available if backend_host.endswith("." + d["baseDomain"])]
    domain = sorted(preferred or available, key=lambda d: (d["baseDomain"], d["domainId"]))[0]
    return backend, domain


def official_cli(workspace):
    if platform.system() != "Linux" or platform.machine() not in ARTIFACTS:
        raise Failure("This harness supports Linux x86_64/aarch64 official CLI artifacts only.")
    arch, expected = ARTIFACTS[platform.machine()]
    destination = workspace.path / f"pangolin-{CLI_VERSION}-{arch}"
    installed = shutil.which("pangolin")
    for path in (destination, Path(installed) if installed else None):
        if path is not None and path.is_file():
            with open(path, "rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if digest == expected:
                if path == destination:
                    path.chmod(0o700)
                return path
    url = f"https://github.com/fosrl/cli/releases/download/{CLI_VERSION}/pangolin-cli_linux_{arch}"
    # No credentials in this opener; GitHub asset redirects are necessary.
    class HttpsRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            if urllib.parse.urlsplit(newurl).scheme != "https":
                raise Failure("Non-HTTPS release redirect refused.")
            return super().redirect_request(req, fp, code, msg, headers, newurl)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), HttpsRedirect())
    fd, temporary = tempfile.mkstemp(prefix=".cli-", dir=workspace.path)
    try:
        digest = hashlib.sha256()
        total = 0
        with os.fdopen(fd, "wb") as stream, opener.open(url, timeout=30) as response:
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                if total > 200 * 1024 * 1024:
                    raise Failure("CLI artifact exceeds the size limit.")
                digest.update(chunk)
                stream.write(chunk)
            stream.flush()
            os.fsync(stream.fileno())
        if digest.hexdigest() != expected:
            raise Failure("Official CLI artifact SHA-256 mismatch; nothing will be executed.")
        os.chmod(temporary, 0o700)
        os.replace(temporary, destination)
        return destination
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)


class Connector:
    def __init__(self, binary, workspace, credentials):
        self.binary, self.workspace, self.credentials = binary, workspace, credentials
        self.process = self.timer = self.temporary = None

    def __enter__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="connector-", dir=self.workspace.path)
        try:
            config = Path(self.temporary.name) / "site.json"
            fd = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as stream:
                json.dump({**self.credentials, "native": False, "nativeMain": False,
                           "disableClients": True, "disableSsh": True, "logLevel": "ERROR",
                           "metrics": False, "otlp": False, "pprof": False, "enforceHcCert": True}, stream)
            # Deliberately omit inherited NEWT_*, CONFIG_FILE, proxy and debugging variables.
            environment = {key: os.environ[key] for key in ("PATH", "LANG", "HOME") if key in os.environ}
            environment.update({"PANGOLIN_CLI_LOG_FILE": os.devnull,
                                "PANGOLIN_CLI_DISABLE_COMPANION_MODE": "true",
                                "PANGOLIN_CLI_DISABLE_UPDATE_CHECK": "true"})
            self.process = subprocess.Popen([str(self.binary), "up", "site", "--config-file", str(config)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                env=environment, start_new_session=True, cwd=self.temporary.name)
            # The pinned CLI begins its embedded self-update check after two minutes.
            self.timer = threading.Timer(105, self.expire)
            self.timer.daemon = True
            self.timer.start()
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def expire(self):
        with contextlib.suppress(ProcessLookupError):
            self.process.kill()

    def alive(self):
        if self.process.poll() is not None:
            raise Failure("Desktop CLI exited or reached its bounded lifetime; sensitive output was discarded.")

    def __exit__(self, *_):
        if self.timer is not None:
            self.timer.cancel()
        if self.process is not None:
            with contextlib.suppress(ProcessLookupError):
                self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if self.temporary is not None:
            self.temporary.cleanup()


class Experiment:
    def __init__(self, api, org, server, workspace, approve=confirm):
        self.api, self.org, self.server, self.work = api, identifier(org), server, workspace
        self.approve = approve
        self.dns = None
        self.state = workspace.load("state.json")
        if self.state is not None:
            run = self.state.get("run")
            if (self.state.get("version") != 1 or not isinstance(run, str) or not re.fullmatch(r"[a-f0-9]{16}", run)
                    or self.state.get("org") != self.org or self.state.get("api") != api.endpoint
                    or self.state.get("server") != server
                    or self.state.get("site_nice_id") != "homestack-test-" + run
                    or self.state.get("resource_name") != SITE_NAME + "-" + run
                    or (self.state.get("fqdn") is not None and (
                        not isinstance(self.state["fqdn"], str)
                        or not self.state["fqdn"].startswith("homestack-test-" + run + ".")))
                    or (self.state.get("resource_id") is not None and self.state.get("fqdn") is None)):
                raise Failure("Recovery state belongs to a different experiment/endpoint; refusing adoption.")

    def save(self):
        self.work.save("state.json", self.state)

    def sites(self):
        return self.api.rows(f"/org/{self.org}/sites", "sites", paged=True)

    def connector_objects(self):
        if not self.state or self.state.get("cleaned") or self.state.get("pending"):
            raise Failure("A complete existing experiment is required; no provisioning or discovery performed.")
        credentials = self.work.load("site-credentials.json")
        if (not isinstance(credentials, dict) or credentials.get("endpoint") != self.server
                or not isinstance(credentials.get("id"), str) or not re.fullmatch(r"[a-f0-9]{15}", credentials["id"])
                or not isinstance(credentials.get("secret"), str) or not re.fullmatch(r"[a-f0-9]{48}", credentials["secret"])):
            raise Failure("Existing Site credentials are missing/invalid; no rotation or provisioning performed.")
        site_id, resource_id, target_id = (positive(self.state.get(k)) for k in ("site_id", "resource_id", "target_id"))
        site = self.api.get(f"/site/{site_id}")
        resource_row = self.api.get(f"/resource/{resource_id}")
        target = self.api.get(f"/target/{target_id}")
        self.owned_site(site, credentials)
        self.owned_resource(resource_row)
        self.owned_target(target)
        targets = self.api.rows(f"/resource/{resource_id}/targets", "targets")
        if len(targets) != 1 or targets[0].get("targetId") != target_id:
            raise Failure("Test Resource has unexpected Targets; connector operation refused.")
        return credentials, site, resource_row, target

    def connector_status(self, probe=http_probe):
        _, site, resource_row, target = self.connector_objects()
        runtime = PersistentConnector(self.work, self.state["run"])
        record = runtime.running()
        online = site.get("online") is True
        healthy = (record is not None and online and target.get("hcHealth") == "healthy"
                   and target.get("enabled") is True and resource_row.get("enabled") is True)
        returned = False
        if record is not None:
            try:
                returned = process_info(record["starter_pid"]) != record["starter_info"]
            except (DnsFailure, KeyError):
                returned = False
        result = {"operation": "status", "local_connector": "running" if record else "stopped",
            "site_id": self.state["site_id"], "site_online": online,
            "target_id": self.state["target_id"], "target_healthy": target.get("hcHealth") == "healthy",
            "resource_id": self.state["resource_id"], "resource_healthy": healthy,
            "start_command_returned": returned, "healthy_after_start_return": healthy and returned,
            "public_url": "https://" + self.state["fqdn"] + HEALTH_PATH,
            "authenticated_public_health": "Not tested; requires an end-user browser session."}
        if probe is not None:
            try:
                response = probe(self.state["fqdn"], 443, HEALTH_PATH)
                result["public_tls"] = {"verified": True, "http_status": response["status"]}
                result["public_authentication_required"] = self.protected(response)
            except Failure as error:
                result["public_tls"] = {"verified": False, "error": str(error)}
        return result

    def start_connector(self, locate_cli=official_cli, probe=http_probe, wait_seconds=40, interval=2):
        credentials, site, resource_row, target = self.connector_objects()
        if resource_row.get("enabled") is not True or target.get("enabled") is not True:
            raise Failure("Recorded Resource/Target is disabled; start does not modify server configuration.")
        runtime = PersistentConnector(self.work, self.state["run"])
        existing = runtime.running()
        if existing is None:
            if site.get("online") is True:
                raise Failure("Site is already Online without a recorded local connector; refusing a possible duplicate. Use status to inspect it.")
            started = runtime.start(locate_cli(self.work), credentials)
        else:
            started = {"local_connector": "running", "pid": existing["pid"], "reused": True}
        end = time.monotonic() + wait_seconds
        while True:
            result = self.connector_status(probe=None)
            if result["resource_healthy"]:
                break
            if time.monotonic() >= end:
                raise Failure("Connector remains recorded in the background, but Site/Target health was not confirmed. Run status or stop; do not create another connector.")
            time.sleep(min(interval, max(0, end - time.monotonic())))
        result = self.connector_status(probe=probe)
        result.update(started)
        result.update({"operation": "start", "outcome": "running; verify persistence with a separate status command",
                       "healthy_after_start_return": False})
        return result

    def status_connector(self, probe=http_probe, interval=2):
        first = self.connector_status(probe=None)
        time.sleep(interval)
        result = self.connector_status(probe=probe)
        result["healthy_after_start_return"] = (first["healthy_after_start_return"]
                                               and result["healthy_after_start_return"])
        result["outcome"] = "healthy_after_start_return" if result["healthy_after_start_return"] else "not_confirmed_healthy"
        return result

    def owned_site(self, row, credentials):
        if (row.get("name") != SITE_NAME or row.get("niceId") != self.state["site_nice_id"]
                or row.get("orgId") != self.org or row.get("type") != "newt"
                or row.get("newtId") != credentials.get("id")):
            raise Failure("Site ownership evidence mismatched; refusing adoption or mutation.")
        positive(row.get("siteId"))

    def owned_resource(self, row, require_sso=True):
        if (row.get("name") != self.state["resource_name"] or row.get("orgId") != self.org
                or row.get("fullDomain") != self.state["fqdn"] or row.get("mode") != "http"
                or row.get("resourcePolicyId") is not None):
            raise Failure("Resource ownership or inline-policy evidence mismatched.")
        positive(row.get("resourceId"))
        if self.state.get("default_policy_id") is not None and row.get("defaultResourcePolicyId") != self.state["default_policy_id"]:
            raise Failure("Test Resource inline policy identity changed; refusing mutation.")
        if require_sso and row.get("sso") is not True:
            raise Failure("Test Resource lost SSO protection; refusing further routing.")

    def owned_target(self, row):
        expected = self.target_body()
        for key in ("siteId", "ip", "port", "method", "path", "pathMatchType"):
            if row.get(key) != expected[key]:
                raise Failure("Target ownership evidence mismatched.")
        if row.get("resourceId") != self.state.get("resource_id"):
            raise Failure("Target belongs to a different Resource.")
        positive(row.get("targetId"))

    def target_body(self):
        backend = self.state["backend"]
        backend_host = backend["host"]
        return {"siteId": self.state["site_id"], "ip": backend["ip"], "port": backend["port"],
                "mode": "http", "method": backend["scheme"], "enabled": True,
                "path": HEALTH_PATH, "pathMatchType": "exact", "hcEnabled": True,
                "hcPath": HEALTH_PATH, "hcHostname": backend_host, "hcScheme": backend["scheme"],
                "hcTlsServerName": backend_host, "hcPort": backend["port"], "hcInterval": 2,
                "hcUnhealthyInterval": 2, "hcTimeout": 5, "hcStatus": 200,
                "hcFollowRedirects": False, "hcMethod": "GET"}

    def reconcile(self, credentials):
        """Only reconcile known pending requests. Never adopt an unrecorded object by name alone."""
        pending = self.state.get("pending")
        if pending == "site":
            row = self.api.optional(f"/org/{self.org}/site/{self.state['site_nice_id']}")
            if row is None:
                raise Failure("Site creation outcome remains unknown; no automatic retry. Inspect the recorded experiment.")
            self.owned_site(row, credentials)
            self.state["site_id"] = row["siteId"]
        elif pending == "resource":
            rows = self.api.rows(f"/org/{self.org}/resources", "resources", paged=True)
            matches = [r for r in rows if r.get("name") == self.state["resource_name"] and r.get("fullDomain") == self.state["fqdn"]]
            if len(matches) != 1:
                raise Failure("Resource creation outcome remains unknown; refusing automatic retry/adoption.")
            row = self.api.get(f"/resource/{positive(matches[0].get('resourceId'))}")
            self.owned_resource(row)
            self.state["resource_id"] = row["resourceId"]
            self.state["default_policy_id"] = positive(row.get("defaultResourcePolicyId"))
        elif pending == "target":
            rows = self.api.rows(f"/resource/{self.state['resource_id']}/targets", "targets")
            if len(rows) != 1:
                raise Failure("Target creation outcome remains unknown; refusing automatic retry.")
            self.owned_target(rows[0])
            self.state["target_id"] = rows[0]["targetId"]
        elif pending is not None:
            raise Failure("Unknown pending recovery stage.")
        self.state["pending"] = None
        self.save()

    def create(self, kind, path, body):
        self.state["pending"] = kind
        self.save()
        self.api.request("PUT", path, body)
        # Re-read through authoritative GET routes; do not trust a partial create response.
        credentials = self.work.load("site-credentials.json")
        self.reconcile(credentials)

    def run(self, deploy_file, client_file, backend_name, probe=http_probe, locate_cli=official_cli, connector=Connector,
            wait_seconds=40, interval=2, dns_factory=OvhDns.from_deploy, dns_seconds=180):
        if self.state and PersistentConnector(self.work, self.state["run"]).running() is not None:
            raise Failure("A persistent connector is running; use status, or stop it before provisioning checks.")
        if self.state and self.state.get("cleaned"):
            raise Failure("This experiment was cleaned. Use a new state directory for another experiment.")
        backend, domain = discover_backend(self.api, self.org, deploy_file, client_file, backend_name, probe)
        backend_host = backend["host"]
        existing = [s for s in self.sites() if s.get("name") == SITE_NAME]
        if self.state is None and existing:
            raise Failure("Test Site already exists without this experiment's ownership record; it will not be adopted.")
        binary = locate_cli(self.work)
        if self.state is None:
            run = secrets.token_hex(8)
            self.state = {"version": 1, "run": run, "org": self.org, "api": self.api.endpoint,
                "server": self.server, "site_nice_id": "homestack-test-" + run,
                "resource_name": SITE_NAME + "-" + run,
                "fqdn": "homestack-test-" + run + "." + domain["baseDomain"],
                "domain_id": domain["domainId"], "backend": backend, "pending": None}
            self.save()
            self.work.save("site-credentials.json", {"endpoint": self.server,
                "id": secrets.token_hex(8)[:15], "secret": secrets.token_hex(24)})
        credentials = self.work.load("site-credentials.json")
        if (not isinstance(credentials, dict) or credentials.get("endpoint") != self.server
                or not isinstance(credentials.get("id"), str) or not re.fullmatch(r"[a-f0-9]{15}", credentials["id"])
                or not isinstance(credentials.get("secret"), str) or not re.fullmatch(r"[a-f0-9]{48}", credentials["secret"])):
            raise Failure("Site recovery credentials are missing/invalid; refusing to rotate or duplicate the Site.")
        self.reconcile(credentials)
        if self.state.get("backend") is None:
            self.state.update({"backend": backend, "domain_id": domain["domainId"],
                "fqdn": "homestack-test-" + self.state["run"] + "." + domain["baseDomain"]})
        elif self.state["backend"] != backend or self.state["domain_id"] != domain["domainId"]:
            raise Failure("Backend/domain discovery changed since the recorded plan; refusing retargeting.")
        self.state["backend_health"] = "Direct private health returned status ok"
        self.save()
        if existing and (len(existing) != 1 or existing[0].get("siteId") != self.state.get("site_id")):
            raise Failure("Conflicting test Site name; refusing duplicates.")
        rows = self.api.rows(f"/org/{self.org}/resources", "resources", paged=True)
        conflicts = [r for r in rows if (r.get("fullDomain") == self.state["fqdn"]
            or r.get("name") == self.state["resource_name"]
            or (isinstance(r.get("fullDomain"), str) and r["fullDomain"].startswith("*.")
                and self.state["fqdn"].endswith(r["fullDomain"][1:])))
            and (not self.state.get("resource_id") or r.get("resourceId") != self.state["resource_id"])]
        if conflicts:
            raise Failure("Proposed test hostname/name conflicts with an existing Resource; no mutation approval offered.")
        self.dns = dns_factory(deploy_file, self.server)
        self.state["dns"] = self.dns.prepare(self.state["fqdn"], domain["baseDomain"], self.state.get("dns"))
        self.save()
        objects = []
        if not self.state["dns"].get("id"):
            objects.append("OVH test CNAME record")
        if not self.state.get("site_id"):
            objects.append("Pangolin Site (Pangolin CLI)")
        if not self.state.get("resource_id"):
            objects.extend(("protected HTTP Resource", "Resource inline policy"))
        if not self.state.get("target_id"):
            objects.append("health-only Target")
        plan = {"org": self.org, "integration_api": self.api.endpoint, "pangolin_server": self.server,
            "backend_instance": backend_name, "site": SITE_NAME, "resource": self.state["resource_name"],
            "selected_domain": domain["baseDomain"], "test_hostname": self.state["fqdn"],
            "dns_record": self.state["dns"],
            "hostname_unused_in_organization": not bool(self.state.get("resource_id")),
            "public_url": "https://" + self.state["fqdn"] + HEALTH_PATH,
            "target_url": f"{backend['scheme']}://{backend['ip']}:{backend['port']}{HEALTH_PATH}",
            "backend_host_and_tls_name": backend_host, "backend_reachable_from_desktop": True,
            "health_check_path": HEALTH_PATH, "authentication_mode": "Pangolin SSO; no bypass",
            "objects_to_create": objects, "cli_version": CLI_VERSION,
            "production": "Existing backend Resources and Targets are read-only"}
        self.approve(json.dumps({**plan, "operation": "connect_test_site",
            "approval_scope": "Create/reuse only the recorded test DNS CNAME and Pangolin Site; publish that DNS zone and start Pangolin CLI after DNS resolves. Resource and Target creation require separate approval."}, indent=2))
        self.state["stage"] = "publishing_test_dns"
        self.save()
        self.dns.ensure(self.state["dns"], self.save)
        print(json.dumps({"dns_record_id": self.state["dns"]["id"], "dns": "Published; waiting for desktop DNS resolution"}), flush=True)
        self.state["stage"] = "waiting_dns"
        self.save()
        deadline = time.monotonic() + dns_seconds
        while True:
            try:
                require_test_dns(self.state["fqdn"])
                break
            except Failure:
                if time.monotonic() >= deadline:
                    raise Failure("Recorded test CNAME was published, but desktop DNS propagation is still pending. No new Site/Resource created; keep state and retry later or run cleanup.") from None
                time.sleep(min(2, max(0, deadline - time.monotonic())))
        if not self.state.get("site_id"):
            if any(row.get("name") == SITE_NAME for row in self.sites()):
                raise Failure("Test Site appeared after confirmation; creation refused.")
            self.create("site", f"/org/{self.org}/site", {"name": SITE_NAME, "niceId": self.state["site_nice_id"],
                "type": "newt", "newtId": credentials["id"], "secret": credentials["secret"]})
        site_id = positive(self.state["site_id"])
        self.owned_site(self.api.get(f"/site/{site_id}"), credentials)
        self.state["stage"] = "connecting_site"
        self.save()
        with connector(binary, self.work, credentials) as process:
            self.wait(lambda: self.api.get(f"/site/{site_id}").get("online") is True,
                      process, wait_seconds, interval, "Site did not become Online within the deadline.")
            self.state["site_connectivity"] = "Online observed; connector stops when this command exits"
            self.state["stage"] = "site_online_backend_preverified"
            self.save()
            print(json.dumps({"site_id": site_id, "site_connectivity": "Online verified"}), flush=True)
            self.approve(json.dumps({"operation": "prepare_test_resource", "org": self.org,
                "proposed_test_hostname": self.state["fqdn"], "verified_backend": backend,
                "mutations": "Create/reuse only the recorded SSO-protected test Resource and its exact health-path Target. Set backend Host/TLS names and health checks. Disable only this test Resource if protection cannot be proven. Production is read-only."}, indent=2))
            process.alive()
            self.state["stage"] = "preparing_resource"
            self.save()
            self.prepare_resource()
            process.alive()
            target_id = positive(self.state["target_id"])
            self.state["stage"] = "waiting_target_health"
            self.save()
            self.wait(lambda: self.api.get(f"/target/{target_id}").get("hcHealth") == "healthy",
                      process, wait_seconds, interval, "Connector Target health did not become healthy within the deadline.")
            self.state["target_health"] = "healthy observed via Pangolin CLI Site health check"
            self.state["target_route"] = f"desktop Pangolin CLI -> {backend['scheme']}://{backend['ip']}:{backend['port']}{HEALTH_PATH} (Host: {backend_host})"
            self.state["stage"] = "checking_public_protection"
            self.save()
            for path in (HEALTH_PATH, "/v1/status"):
                end = time.monotonic() + wait_seconds
                while True:
                    process.alive()
                    try:
                        result = probe(self.state["fqdn"], 443, path)
                        if result.get("status") not in (502, 503, 504) or time.monotonic() >= end:
                            break
                    except Failure:
                        if time.monotonic() >= end:
                            raise
                    time.sleep(min(interval, max(0, end - time.monotonic())))
                if path != HEALTH_PATH and result.get("status") == 404:
                    self.state.setdefault("public_checks", {})[path] = {"status": 404, "route_available": False}
                    continue
                if not self.protected(result):
                    # Only the verified, recorded test Resource can be disabled here.
                    self.disable_owned_resource()
                    raise Failure("Public test route did not prove authentication protection; test Resource disabled.")
                self.state.setdefault("public_checks", {})[path] = {"status": result["status"], "authentication_required": True}
            self.state["public_route"] = "https://" + self.state["fqdn"] + HEALTH_PATH
            self.state["authentication_method"] = "Pangolin SSO (verified configuration and unauthenticated rejection)"
            self.state["unresolved"] = "End-user authenticated edge health not exercised: no Pangolin user session supplied. Integration API keys cannot authenticate protected public Resources."
            self.state["stage"] = "partial_authenticated_edge_health_unavailable"
            self.save()
        return self.report()

    @staticmethod
    def wait(predicate, process, seconds, interval, message):
        end = time.monotonic() + seconds
        while True:
            process.alive()
            if predicate():
                return
            if time.monotonic() >= end:
                raise Failure(message)
            time.sleep(min(interval, max(0, end - time.monotonic())))

    def protected(self, result):
        if result.get("status") in (401, 403):
            return True
        if result.get("status") in (302, 303, 307, 308):
            location = urllib.parse.urlsplit(result.get("location", ""))
            return (bool(location.path) and location.path.startswith(("/auth/", "/login"))
                and location.scheme in ("", "https") and location.hostname in (
                None, urllib.parse.urlsplit(self.server).hostname, self.state["fqdn"]))
        return False

    def disable_owned_resource(self):
        resource_id = positive(self.state["resource_id"])
        row = self.api.get(f"/resource/{resource_id}")
        # Identity checks remain required even if SSO was changed unexpectedly.
        if (row.get("orgId") != self.org or row.get("name") != self.state["resource_name"]
                or row.get("fullDomain") != self.state["fqdn"]):
            raise Failure("Test Resource identity changed; no mutation performed.")
        self.api.request("POST", f"/resource/{resource_id}", {"enabled": False})

    def prepare_resource(self):
        backend_host = self.state["backend"]["host"]
        if not self.state.get("resource_id"):
            rows = self.api.rows(f"/org/{self.org}/resources", "resources", paged=True)
            if any(r.get("name") == self.state["resource_name"] or r.get("fullDomain") == self.state["fqdn"] for r in rows):
                raise Failure("Unrecorded Resource name/domain conflict; refusing adoption.")
            self.create("resource", f"/org/{self.org}/resource", {"name": self.state["resource_name"],
                "mode": "http", "domainId": self.state["domain_id"],
                "subdomain": self.state["fqdn"].split(".")[0]})
        resource_id = positive(self.state["resource_id"])
        self.owned_resource(self.api.get(f"/resource/{resource_id}"))
        self.api.request("POST", f"/resource/{resource_id}", {
            "setHostHeader": backend_host, "tlsServerName": backend_host})
        self.owned_resource(self.api.get(f"/resource/{resource_id}"))
        targets = self.api.rows(f"/resource/{resource_id}/targets", "targets")
        if not self.state.get("target_id"):
            if targets:
                raise Failure("Unrecorded Target exists on the test Resource; refusing duplicate creation.")
            self.create("target", f"/resource/{resource_id}/target", self.target_body())
        elif len(targets) != 1 or targets[0].get("targetId") != self.state["target_id"]:
            raise Failure("Test Resource contains unexpected Targets; refusing mutation.")
        self.owned_target(self.api.get(f"/target/{positive(self.state['target_id'])}"))

    def cleanup(self, deploy_file=None, dns_factory=OvhDns.from_deploy):
        if self.state is None or self.state.get("cleaned"):
            return {"cleanup": "nothing recorded to remove"}
        if PersistentConnector(self.work, self.state["run"]).running() is not None:
            raise Failure("Stop the recorded local connector before cleanup; no server mutation performed.")
        credentials = self.work.load("site-credentials.json")
        if not isinstance(credentials, dict):
            raise Failure("Site ownership credentials are missing; cleanup refused.")
        self.reconcile(credentials)
        site_id, resource_id, target_id = (self.state.get(k) for k in ("site_id", "resource_id", "target_id"))
        site = self.api.optional(f"/site/{positive(site_id)}") if site_id else None
        resource_row = self.api.optional(f"/resource/{positive(resource_id)}") if resource_id else None
        target = self.api.optional(f"/target/{positive(target_id)}") if target_id else None
        if site:
            self.owned_site(site, credentials)
        if resource_row:
            self.owned_resource(resource_row, require_sso=False)
            children = self.api.rows(f"/resource/{resource_id}/targets", "targets")
            if {positive(t.get("targetId")) for t in children} != ({target_id} if target else set()):
                raise Failure("Unrecorded Targets on test Resource; cleanup refused.")
        if target:
            self.owned_target(target)
        if site:
            rows = [s for s in self.sites() if s.get("siteId") == site_id]
            expected_count = 1 if target else 0
            if len(rows) != 1 or rows[0].get("resourceCount") != expected_count:
                raise Failure("Site has unrecorded associated resources; cleanup refused.")
        dns_plan = self.state.get("dns")
        if dns_plan is not None:
            if self.dns is None:
                if deploy_file is None:
                    raise Failure("HomeStack deployment metadata is needed to load OVH credentials for recorded DNS cleanup.")
                self.dns = dns_factory(deploy_file, self.server)
            self.dns.inspect(dns_plan, allow_missing=True)
            self.save()
        self.approve(json.dumps({"operation": "cleanup", "org": self.org,
            "recorded_site_id": site_id, "recorded_resource_id": resource_id,
            "recorded_target_id": target_id, "recorded_inline_policy_id": self.state.get("default_policy_id"),
            "recorded_dns": dns_plan, "production": "Only the exact recorded test CNAME can be deleted; no domain/zone deletion"}, indent=2))
        for kind, object_id, row in (("target", target_id, target), ("resource", resource_id, resource_row), ("site", site_id, site)):
            if row is not None:
                current = self.api.optional(f"/{kind}/{object_id}")
                if current is None:
                    self.state[kind + "_deleted"] = True
                    self.save()
                    continue
                if kind == "site":
                    self.owned_site(current, credentials)
                elif kind == "resource":
                    self.owned_resource(current, require_sso=False)
                else:
                    self.owned_target(current)
                # Repeat dependency checks immediately before cascade-capable deletions.
                if kind == "resource" and self.api.rows(f"/resource/{object_id}/targets", "targets"):
                    raise Failure("Targets appeared during cleanup; Resource deletion refused.")
                if kind == "site":
                    rows = [s for s in self.sites() if s.get("siteId") == object_id]
                    if len(rows) != 1 or rows[0].get("resourceCount") != 0:
                        raise Failure("Associated resources appeared during cleanup; Site deletion refused.")
                suffix = "?deleteResources=false" if kind == "site" else ""
                self.api.request("DELETE", f"/{kind}/{object_id}{suffix}")
            self.state[kind + "_deleted"] = True
            self.save()
        if dns_plan is not None:
            self.dns.remove(dns_plan, self.save)
        self.state["cleaned"] = True
        self.state["stage"] = "cleanup_complete"
        self.save()
        with contextlib.suppress(FileNotFoundError):
            (self.work.path / "site-credentials.json").unlink()
        with contextlib.suppress(FileNotFoundError):
            (self.work.path / "connector-site.json").unlink()
        return self.report()

    def report(self):
        fields = ("org", "stage", "pending", "site_id", "resource_id", "target_id", "site_connectivity",
                  "target_health", "target_route", "public_route", "public_checks", "unresolved", "cleaned", "backend_health", "authentication_method", "dns")
        return {key: self.state[key] for key in fields if key in self.state}


def interrupted(*_):
    raise KeyboardInterrupt()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("run", "start", "status", "stop", "cleanup"))
    parser.add_argument("--settings-file", type=Path, default=Path.home() / ".config/homestack/pangolin-test.toml",
                        help="Private local environment settings outside Git")
    parser.add_argument("--credential-file", type=Path, default=Path.home() / ".config/homestack/secrets/pangolin.json")
    parser.add_argument("--deploy-file", type=Path, default=Path.home() / ".config/homestack/deploy.toml")
    parser.add_argument("--client-file", type=Path, default=Path.home() / ".config/homestack/client.toml")
    parser.add_argument("--state-dir", type=Path, default=Path.home() / ".local/state/homestack/pangolin-test")
    args = parser.parse_args(argv)
    experiment = None
    try:
        if os.geteuid() == 0:
            raise Failure("Run as your normal desktop user, without sudo.")
        os.umask(0o077)
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        signal.signal(signal.SIGTERM, interrupted)
        http.client.HTTPConnection.debuglevel = http.client.HTTPSConnection.debuglevel = 0
        settings = load_settings(args.settings_file.expanduser())
        parsed = urllib.parse.urlsplit(settings["server_url"])
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
                or parsed.password is not None or parsed.query or parsed.fragment or parsed.path not in ("", "/")):
            raise Failure("Pangolin server endpoint must be an HTTPS origin without credentials or query parameters.")
        with Workspace(args.state_dir) as workspace:
            if args.command == "stop":
                state = workspace.load("state.json")
                if (not isinstance(state, dict) or state.get("version") != 1
                        or state.get("org") != settings["organization"]
                        or state.get("api") != settings["api_url"].rstrip("/")
                        or state.get("server") != settings["server_url"].rstrip("/")
                        or not isinstance(state.get("run"), str) or not re.fullmatch(r"[a-f0-9]{16}", state["run"])):
                    raise Failure("No matching recorded experiment for stop; no process signaled.")
                result = PersistentConnector(workspace, state["run"]).stop()
            else:
                credential = read_private(args.credential_file.expanduser()).strip()
                api = Client(settings["api_url"], credential)
                experiment = Experiment(api, settings["organization"], settings["server_url"].rstrip("/"), workspace)
                if args.command == "start":
                    result = experiment.start_connector()
                elif args.command == "status":
                    result = experiment.status_connector()
                elif args.command == "run":
                    result = experiment.run(args.deploy_file.expanduser(), args.client_file.expanduser(), settings["backend_instance"])
                else:
                    result = experiment.cleanup(args.deploy_file.expanduser())
            print(json.dumps(result, indent=2, sort_keys=True))
        if args.command == "status":
            return 0 if result["healthy_after_start_return"] else 2
        return 2 if args.command == "run" and result.get("unresolved") else 0
    except BaseException as error:
        report = {"outcome": "stopped", "error": str(error) if isinstance(error, DnsFailure)
                  else "Local failure/interruption; sensitive exception details omitted. Preserve the state directory for recovery."}
        if args.command in ("run", "cleanup") and experiment is not None and experiment.state is not None:
            report["experiment"] = experiment.report()
        print(json.dumps(report, indent=2, sort_keys=True))
        return 1


if __name__ == "__main__":
    sys.exit(main())
