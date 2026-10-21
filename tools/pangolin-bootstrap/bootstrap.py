#!/usr/bin/env python3
"""Standalone, interactive Pangolin Integration API bootstrap (POSIX, Python 3.10+)."""
import argparse
import contextlib
import getpass
import http.client
import json
import os
from pathlib import Path
import re
import resource
import signal
import ssl
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
import warnings

SOURCE_REVISION = "0842e6df53e898759ed05b77086ddd3c673b6149"
MANAGEMENT = frozenset({"listApiKeys", "listApiKeyActions", "setApiKeyActions", "createApiKey"})
# This profile covers blueprint application, public resources and their access policies.
ACTIONS = tuple(sorted({
    "applyBlueprint",
    "createSite", "getSite", "listSites", "updateSite", "deleteSite",
    "createResource", "getResource", "listResources", "updateResource", "deleteResource",
    "createTarget", "getTarget", "listTargets", "updateTarget", "deleteTarget",
    "getOrg", "listOrgDomains", "getDomain",
    "getResourcePolicy", "updateResourcePolicy", "setResourcePolicyRules",
    "createResourceRule", "listResourceRules", "updateResourceRule", "deleteResourceRule",
}))
IDENTIFIER = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
TOKEN_PART = re.compile(r"[A-Za-z0-9_-]{1,512}\Z")


class Failure(Exception):
    """Only fixed, locally authored messages may be passed to this exception."""


class ApiFailure(Failure):
    def __init__(self, status):
        self.status = status
        super().__init__(f"API request rejected (HTTP {status}); response omitted.")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise Failure("HTTP redirect refused; use the final permitted Integration API endpoint.")


def identifier(value):
    if not isinstance(value, str) or not IDENTIFIER.fullmatch(value):
        raise Failure("Invalid identifier or incompatible API response.")
    return value


def token(value):
    if not isinstance(value, str) or len(value.split(".")) != 2:
        raise Failure("Expected an API credential in ID.secret format.")
    key_id, secret = value.split(".")
    identifier(key_id)
    if not TOKEN_PART.fullmatch(secret):
        raise Failure("Invalid API credential format.")
    return key_id


class Client:
    def __init__(self, endpoint, credential, *, allow_http_loopback=False):
        parsed = urllib.parse.urlsplit(endpoint)
        permitted_transport = parsed.scheme == "https" or (
            allow_http_loopback and parsed.scheme == "http"
            and parsed.hostname in ("127.0.0.1", "::1"))
        if (not permitted_transport or not parsed.hostname or parsed.username is not None
                or parsed.password is not None or parsed.query or parsed.fragment
                or parsed.path.rstrip("/") != "/v1"):
            raise Failure("Endpoint must use HTTPS, or HTTP on literal 127.0.0.1 or ::1 with --allow-http-loopback, and end in /v1 without credentials or query parameters.")
        token(credential)
        self.endpoint = endpoint.rstrip("/")
        self.allow_http_loopback = allow_http_loopback
        self.credential = credential
        http.client.HTTPConnection.debuglevel = 0
        http.client.HTTPSConnection.debuglevel = 0
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), NoRedirect(),
            urllib.request.HTTPSHandler(context=ssl.create_default_context(), debuglevel=0))

    def request(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(self.endpoint + path, data=data, method=method,
            headers={"Authorization": "Bearer " + self.credential,
                     "Content-Type": "application/json", "Accept": "application/json"})
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read(4 * 1024 * 1024 + 1)
                if len(raw) > 4 * 1024 * 1024:
                    raise Failure("API response exceeds the size limit.")
            result = json.loads(raw)
            if (not isinstance(result, dict) or result.get("success") is not True
                    or result.get("error") is not False or not isinstance(result.get("data"), dict)):
                raise Failure("Unexpected API response envelope; response omitted.")
            return result["data"]
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
            raise ApiFailure(status) from None
        except (OSError, ValueError, urllib.error.URLError):
            raise Failure("Transport, TLS, or JSON failure; details omitted. Do not retry creation blindly.") from None

    def collection(self, path, field):
        # Pangolin sometimes reports page length as total. Exhaust pages, not total.
        rows = []
        for offset in range(0, 100000, 100):
            data = self.request("GET", f"{path}?limit=100&offset={offset}")
            page = data.get(field)
            pagination = data.get("pagination")
            if (not isinstance(page, list) or len(page) > 100
                    or not all(isinstance(row, dict) for row in page)
                    or not isinstance(pagination, dict)
                    or pagination.get("offset") != offset or pagination.get("limit") != 100):
                raise Failure("Unexpected paginated response; refusing incomplete authorization evidence.")
            rows.extend(page)
            if len(page) < 100:
                return rows
        raise Failure("Pagination limit reached; refusing incomplete authorization evidence.")


def key_path(org_id, key_id):
    return f"/org/{identifier(org_id)}/api-key/{identifier(key_id)}/actions"


def read_actions(client, org_id, key_id):
    rows = client.collection(key_path(org_id, key_id), "actions")
    return {identifier(row.get("actionId")) for row in rows}


def org_keys(client, org_id):
    rows = client.collection(f"/org/{identifier(org_id)}/api-keys", "apiKeys")
    for row in rows:
        identifier(row.get("apiKeyId"))
        if row.get("orgId") != org_id or not isinstance(row.get("name"), str):
            raise Failure("Unexpected organization key metadata.")
    return rows


def preflight(client, org_id, report):
    report["stage"] = "checking_bootstrap"
    keys = org_keys(client, org_id)
    report["confirmed_by_successful_calls"] = ["bootstrap authentication", "Root access", "listApiKeys"]
    own = read_actions(client, org_id, token(client.credential))
    report["confirmed_by_successful_calls"].append("listApiKeyActions")
    report["management_actions_present_in_metadata"] = sorted(MANAGEMENT & own)
    report["missing_management_actions"] = sorted(MANAGEMENT - own)
    report["write_permissions_inferred_only"] = sorted({"createApiKey", "setApiKeyActions"} & own)
    report["organization_existence_confirmed"] = False
    try:
        data = client.request("GET", f"/org/{org_id}")
        if not isinstance(data.get("org"), dict) or data["org"].get("orgId") != org_id:
            raise Failure("Unexpected organization metadata.")
        report["organization_existence_confirmed"] = True
    except ApiFailure as error:
        if error.status != 403:
            raise
    report["organization_access"] = "Root guard confirmed; org existence needs getOrg or later independent validation"
    report["homestack_conflict"] = any(row["name"] == "homestack" for row in keys)
    report["can_provision_inferred"] = MANAGEMENT <= own and not report["homestack_conflict"]
    report["available_action_catalog"] = "No Integration API catalog route; profile derived from pinned source"
    if report["homestack_conflict"]:
        raise Failure("An organization key named homestack already exists. No duplicate or replacement is allowed.")
    if MANAGEMENT - own:
        raise Failure("Bootstrap management permissions are missing; see sanitized metadata report.")


class Storage:
    """Reserve credential and recovery files before creating anything remotely."""
    def __init__(self, output):
        self.path = Path(output)
        self.credential_fd = None
        self.state_fd = None

    def __enter__(self):
        if not self.path.is_absolute() or self.path.name in ("", ".", ".."):
            raise Failure("Credential output must be an absolute file path.")
        # Reject repository paths including symlinked ancestors, and any enclosing Git checkout.
        resolved = self.path.resolve()
        repository = Path(__file__).resolve().parents[2]
        if resolved.is_relative_to(repository) or any(
                (p / ".git").is_file() or (p / ".git" / "HEAD").is_file()
                for p in resolved.parents):
            raise Failure("Credential output must be outside every Git repository.")
        # Walk with directory FDs and O_NOFOLLOW to avoid symlink substitution races.
        directory_fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
        try:
            for component in self.path.parent.parts[1:]:
                if component in (".", ".."):
                    raise Failure("Dot path components are not allowed.")
                try:
                    os.mkdir(component, mode=0o700, dir_fd=directory_fd)
                except FileExistsError:
                    pass
                next_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory_fd)
                os.close(directory_fd)
                directory_fd = next_fd
                info = os.fstat(directory_fd)
                sticky_root = info.st_uid == 0 and info.st_mode & stat.S_ISVTX
                if info.st_uid not in (0, os.getuid()) or (info.st_mode & 0o022 and not sticky_root):
                    raise Failure("Credential ancestors must be owned by root or this user and not writable by others.")
            info = os.fstat(directory_fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise Failure("Credential parent directory must be owned by this user with mode 0700.")
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
            self.state_fd = os.open(self.path.name + ".state.json", flags, 0o600, dir_fd=directory_fd)
            self.credential_fd = os.open(self.path.name, flags, 0o600, dir_fd=directory_fd)
            os.fsync(directory_fd)
        except BaseException:
            self.__exit__(None, None, None)
            raise
        finally:
            os.close(directory_fd)
        return self

    def state(self, report):
        content = (json.dumps(report, sort_keys=True, indent=2) + "\n").encode()
        os.lseek(self.state_fd, 0, os.SEEK_SET)
        self._write(self.state_fd, content)
        os.ftruncate(self.state_fd, len(content))
        os.fsync(self.state_fd)

    @staticmethod
    def _write(fd, content):
        while content:
            count = os.write(fd, content)
            if count <= 0:
                raise Failure("Failed to persist local state.")
            content = content[count:]

    def save_key(self, credential):
        self._write(self.credential_fd, (credential + "\n").encode())
        os.fsync(self.credential_fd)

    def __exit__(self, *_):
        for fd in (self.credential_fd, self.state_fd):
            if fd is not None:
                os.close(fd)
        self.credential_fd = self.state_fd = None


def denied(client, path):
    try:
        client.request("GET", path)
    except ApiFailure as error:
        if error.status == 403:
            return
        raise
    raise Failure("Negative authorization check unexpectedly succeeded.")


def prompt_confirmation(prompt):
    with open("/dev/tty", "w") as writer, open("/dev/tty", "r") as reader:
        if not writer.isatty() or not reader.isatty():
            raise Failure("An interactive terminal is required.")
        print(prompt, end="", file=writer, flush=True)
        return reader.readline().rstrip("\n")


def interrupted(*_):
    raise KeyboardInterrupt()


def provision(client, org_id, output, report, confirm=None, client_factory=Client):
    confirm = prompt_confirmation if confirm is None else confirm
    preflight(client, org_id, report)
    print(json.dumps(report, indent=2, sort_keys=True))
    print("Approved actions: " + ", ".join(ACTIONS))
    print("Creates one organization key, assigns this profile, and saves it at the approved output path.")
    print("Organization existence may be unconfirmed until the new key can read it. Verify the ID in your dashboard.")
    print("Filesystem permissions do not isolate secrets from other processes using the same OS account.")
    if confirm(f"Type 'CREATE homestack IN {org_id}' to approve this organization, action list, and output path: ") != f"CREATE homestack IN {org_id}":
        raise Failure("Confirmation did not match; nothing created.")
    with Storage(output) as storage:
        try:
            # Close the confirmation-time conflict window. The server has no name uniqueness constraint.
            if any(row["name"] == "homestack" for row in org_keys(client, org_id)):
                raise Failure("A homestack key appeared after confirmation; creation refused.")
            report["stage"] = "creation_requested_outcome_unknown"
            storage.state(report)
            data = client.request("PUT", f"/org/{org_id}/api-key", {"name": "homestack"})
            new_id = identifier(data.get("apiKeyId"))
            report["created_key_id"] = new_id
            report["stage"] = "creation_response_received"
            storage.state(report)
            secret = data.get("apiKey")
            if not isinstance(secret, str) or not TOKEN_PART.fullmatch(secret):
                raise Failure("Creation returned an incompatible secret format; recover manually by key ID.")
            new_credential = new_id + "." + secret
            storage.save_key(new_credential)
            report["credential_persisted"] = True
            report["stage"] = "assigning_actions"
            storage.state(report)
            client.request("POST", key_path(org_id, new_id), {"actionIds": list(ACTIONS)})
            if read_actions(client, org_id, new_id) != set(ACTIONS):
                raise Failure("Assigned permissions differ from the approved profile.")
            rows = org_keys(client, org_id)
            matches = [row for row in rows if row["apiKeyId"] == new_id and row["name"] == "homestack"]
            if len(matches) != 1 or sum(row["name"] == "homestack" for row in rows) != 1:
                raise Failure("Cannot confirm unique organization-scoped, non-Root key metadata.")
            report["exact_actions_verified"] = True
            report["non_root_confirmed_by_filtered_org_listing"] = True
            report["stage"] = "independent_authorization_checks"
            storage.state(report)
            new_client = client_factory(client.endpoint, new_credential,
                                        allow_http_loopback=client.allow_http_loopback)
            organization = new_client.request("GET", f"/org/{org_id}")
            if not isinstance(organization.get("org"), dict) or organization["org"].get("orgId") != org_id:
                raise Failure("New key could not independently confirm the intended organization.")
            report["organization_existence_confirmed"] = True
            for suffix, field in (("sites", "sites"), ("resources", "resources"), ("domains", "domains")):
                data = new_client.request("GET", f"/org/{org_id}/{suffix}")
                if not isinstance(data.get(field), list):
                    raise Failure("Unexpected read-only authorization response.")
            denied(new_client, f"/org/{org_id}/api-keys")
            denied(new_client, key_path(org_id, new_id))
            denied(new_client, f"/org/bootstrap-scope-probe-{uuid.uuid4().hex}/sites")
            report["safe_read_checks"] = ["getOrg", "listSites", "listResources", "listOrgDomains"]
            report["negative_checks"] = ["API key listing denied", "API key action listing denied", "other organization denied"]
            report["stage"] = "complete"
            storage.state(report)
        except BaseException:
            report["recovery_required"] = True
            with contextlib.suppress(Exception):
                storage.state(report)
            raise


def prompt_secret():
    # Refuse getpass's fallback to echoed stdin, including redirected invocation.
    with open("/dev/tty", "w") as terminal:
        if not terminal.isatty():
            raise Failure("An interactive terminal is required.")
        with warnings.catch_warnings():
            warnings.simplefilter("error", getpass.GetPassWarning)
            return getpass.getpass("Temporary bootstrap API key (hidden): ", stream=terminal)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True, help="HTTPS Integration API base URL ending in /v1")
    parser.add_argument("--allow-http-loopback", action="store_true",
                        help="Allow HTTP only on literal 127.0.0.1 or ::1 (for a trusted local tunnel)")
    parser.add_argument("--org", required=True, help="Organization ID from the Pangolin dashboard")
    parser.add_argument("--apply", action="store_true", help="Create and verify the permanent key after confirmation")
    parser.add_argument("--output", help="Explicitly approved absolute credential path outside Git (apply only)")
    args = parser.parse_args(argv)
    report = {"mode": "apply" if args.apply else "check", "stage": "input_validation",
              "source_revision": SOURCE_REVISION, "approved_actions": list(ACTIONS)}
    try:
        os.umask(0o077)
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        signal.signal(signal.SIGTERM, interrupted)
        org_id = identifier(args.org)
        report["organization_id"] = org_id
        if args.apply != bool(args.output):
            raise Failure("--apply requires --output; --output is not accepted in check mode.")
        credential = prompt_secret()
        client = Client(args.endpoint, credential, allow_http_loopback=args.allow_http_loopback)
        if args.apply:
            # Paths are non-secret configuration. JSON quoting prevents terminal control sequences.
            print("Approved credential output: " + json.dumps(args.output))
            provision(client, org_id, args.output, report)
        else:
            preflight(client, org_id, report)
            report["stage"] = "check_complete_no_mutations"
        print(json.dumps(report, sort_keys=True, indent=2))
        return 0
    except BaseException as error:
        report["outcome"] = "failed_or_interrupted"
        report["error"] = str(error) if isinstance(error, Failure) else "Local failure or interruption; sensitive details omitted."
        print(json.dumps(report, sort_keys=True, indent=2))
        return 1


if __name__ == "__main__":
    sys.exit(main())
