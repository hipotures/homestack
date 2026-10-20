"""OVH test CNAME lifecycle. No secrets in arguments, diagnostics or recovery state."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import ssl
import stat
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request

ENDPOINTS = {
    "ovh-eu": "https://eu.api.ovh.com/1.0",
    "ovh-ca": "https://ca.api.ovh.com/1.0",
    "ovh-us": "https://api.us.ovhcloud.com/1.0",
}


class DnsFailure(Exception):
    """Only locally authored, sanitized messages belong here."""


class OvhFailure(DnsFailure):
    def __init__(self, status):
        self.status = status
        super().__init__(f"OVH DNS request rejected (HTTP {status}); response omitted. Check zone-scoped record/read/create/delete and refresh permissions.")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise DnsFailure("OVH redirect refused; credentials were not forwarded.")


def positive(value):
    if type(value) is not int or value <= 0:
        raise DnsFailure("Invalid OVH record ID; refusing mutation.")
    return value


def domain(value):
    if not isinstance(value, str) or not re.fullmatch(r"[a-z0-9][a-z0-9.-]{0,251}[a-z0-9]", value) or ".." in value:
        raise DnsFailure("Invalid DNS name in experiment metadata.")
    return value


class OvhDns:
    def __init__(self, endpoint, application, secret, consumer, server_host):
        if endpoint not in ENDPOINTS.values():
            raise DnsFailure("Unsupported OVH API endpoint; refusing to send credentials to a custom host.")
        self.endpoint = endpoint
        self.application, self.secret, self.consumer = application, secret, consumer
        self.target = domain(server_host) + "."
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(),
            urllib.request.HTTPSHandler(context=ssl.create_default_context(), debuglevel=0))
        server_time = self.call("GET", "/auth/time", signed=False)
        if type(server_time) is not int:
            raise DnsFailure("OVH did not return a valid server time.")
        self.time_delta = server_time - int(time.time())

    @classmethod
    def from_deploy(cls, deploy_file, server):
        try:
            with open(deploy_file, "rb") as stream:
                metadata = tomllib.load(stream)
            # The desktop Integrations editor saves this file independently of ACME.
            location = metadata.get("acme", {}).get("ovh_file", "integrations/ovh-dns")
            if not isinstance(location, str) or not location:
                raise DnsFailure("Invalid acme.ovh_file credential path in HomeStack deployment metadata.")
            path = Path(location).expanduser()
            if not path.is_absolute():
                path = Path(deploy_file).parent / path
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd) as stream:
                info = os.fstat(stream.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                        or stat.S_IMODE(info.st_mode) not in (0o400, 0o600)):
                    raise DnsFailure("OVH credentials must be a private regular file owned by this user (0400/0600).")
                text = stream.read(65537)
            if len(text) > 65536:
                raise DnsFailure("OVH credential file exceeds the size limit.")
            fields = {}
            for line in text.splitlines():
                name, separator, value = line.strip().partition("=")
                if separator and name.strip() in ("OVH_AK", "OVH_AS", "OVH_CK", "OVH_END_POINT"):
                    fields[name.strip()] = value.strip().strip("\"'")
            if not all(fields.get(key) for key in ("OVH_AK", "OVH_AS", "OVH_CK")):
                raise DnsFailure("OVH credential file needs OVH_AK, OVH_AS and OVH_CK; values are never printed.")
            endpoint_name = fields.get("OVH_END_POINT", "ovh-eu")
            endpoint = ENDPOINTS.get(endpoint_name, endpoint_name)
            return cls(endpoint, fields["OVH_AK"], fields["OVH_AS"], fields["OVH_CK"],
                       urllib.parse.urlsplit(server).hostname)
        except (OSError, ValueError, AttributeError):
            raise DnsFailure("Cannot load private OVH credentials: expected acme.ovh_file when configured, otherwise integrations/ovh-dns beside deploy.toml. Sensitive details omitted.") from None

    def call(self, method, path, body=None, *, signed=True):
        url = self.endpoint + path
        payload = "" if body is None else json.dumps(body, separators=(",", ":"))
        headers = {"Content-Type": "application/json", "Accept": "application/json"}
        if signed:
            timestamp = str(int(time.time()) + self.time_delta)
            signing = "+".join((self.secret, self.consumer, method, url, payload, timestamp))
            headers.update({"X-Ovh-Application": self.application, "X-Ovh-Consumer": self.consumer,
                "X-Ovh-Timestamp": timestamp,
                "X-Ovh-Signature": "$1$" + hashlib.sha1(signing.encode()).hexdigest()})
        request = urllib.request.Request(url, method=method, headers=headers,
            data=None if body is None else payload.encode())
        try:
            with self.opener.open(request, timeout=10) as response:
                raw = response.read(4 * 1024 * 1024 + 1)
            if len(raw) > 4 * 1024 * 1024:
                raise DnsFailure("OVH response exceeds the size limit.")
            return None if not raw.strip() else json.loads(raw)
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
            raise OvhFailure(status) from None
        except (OSError, ValueError, http.client.HTTPException, urllib.error.URLError):
            raise DnsFailure("OVH DNS transport/JSON failure; write outcome may be unknown. Preserve experiment state.") from None

    def optional(self, path):
        try:
            return self.call("GET", path)
        except OvhFailure as error:
            if error.status == 404:
                return None
            raise

    def records(self, plan):
        query = urllib.parse.urlencode({"subDomain": plan["subDomain"]})
        ids = self.call("GET", f"/domain/zone/{plan['zone']}/record?{query}")
        if not isinstance(ids, list) or len(ids) > 1000 or len(set(ids)) != len(ids):
            raise DnsFailure("Invalid/incomplete OVH DNS listing; refusing mutation.")
        rows = []
        for record_id in ids:
            row = self.call("GET", f"/domain/zone/{plan['zone']}/record/{positive(record_id)}")
            if not isinstance(row, dict) or row.get("id") != record_id:
                raise DnsFailure("Invalid OVH record metadata.")
            if row.get("subDomain") == plan["subDomain"]:
                rows.append(row)
        return rows

    @staticmethod
    def matches(row, plan):
        return (isinstance(row, dict) and row.get("fieldType") == "CNAME"
            and row.get("zone") == plan["zone"] and row.get("subDomain") == plan["subDomain"]
            and isinstance(row.get("target"), str)
            and row["target"].rstrip(".").lower() == plan["target"].rstrip(".").lower()
            and row.get("ttl") == plan["ttl"])

    def validate_plan(self, plan):
        name, zone = domain(plan.get("name")), domain(plan.get("zone"))
        if (not re.fullmatch(r"homestack-test-[a-f0-9]{16}\..+", name)
                or not name.endswith("." + zone) or plan.get("provider") != "ovh"
                or plan.get("subDomain") != name[:-len(zone) - 1]
                or plan.get("fieldType") != "CNAME" or plan.get("target") != self.target
                or plan.get("ttl") != 60 or self.target.rstrip(".") == name):
            raise DnsFailure("DNS ownership plan is inconsistent; refusing modification/deletion.")

    def inspect(self, plan, *, allow_missing=False):
        self.validate_plan(plan)
        if plan.get("id") is not None:
            record_id = positive(plan["id"])
            row = self.optional(f"/domain/zone/{plan['zone']}/record/{record_id}")
            if row is None and allow_missing:
                return None
            if not self.matches(row, plan) or row.get("id") != record_id:
                raise DnsFailure("Recorded DNS record is missing/changed; refusing overwrite or deletion.")
            return row
        rows = self.records(plan)
        if plan.get("pending"):
            if len(rows) != 1 or not self.matches(rows[0], plan):
                raise DnsFailure("DNS creation outcome is unresolved; no automatic retry or deletion of uncertain records.")
            plan["id"] = positive(rows[0].get("id"))
            plan["pending"] = False
            return rows[0]
        if rows:
            raise DnsFailure("DNS name already has an unrecorded record; it will not be adopted, overwritten or deleted.")
        return None

    def prepare(self, name, base_domain, existing=None):
        domain(name)
        domain(base_domain)
        if not name.endswith("." + base_domain):
            raise DnsFailure("Test hostname is outside its selected domain.")
        zone = None
        labels = base_domain.split(".")
        for index in range(len(labels) - 1):
            candidate = ".".join(labels[index:])
            info = self.optional("/domain/zone/" + candidate)
            if info is not None:
                if not isinstance(info, dict):
                    raise DnsFailure("Invalid OVH zone response.")
                zone = candidate
                break
        if zone is None:
            raise DnsFailure("No accessible OVH zone contains the selected Pangolin domain; no approval offered.")
        plan = {"provider": "ovh", "zone": zone, "name": name, "fieldType": "CNAME",
                "subDomain": name[:-len(zone) - 1], "target": self.target, "ttl": 60,
                "id": None, "pending": False}
        self.validate_plan(plan)
        if existing is not None:
            if not isinstance(existing, dict) or any(existing.get(key) != plan[key]
                    for key in ("provider", "zone", "name", "fieldType", "subDomain", "target", "ttl")):
                raise DnsFailure("DNS plan changed since the experiment was recorded; refusing retargeting.")
            plan = dict(existing)
            if plan.get("deleted") or plan.get("deletion_pending"):
                raise DnsFailure("DNS cleanup has started; finish cleanup before another experiment.")
        self.inspect(plan)
        return plan

    def ensure(self, plan, save):
        row = self.inspect(plan)
        save()
        if row is None:
            plan["pending"] = True
            save()
            body = {key: plan[key] for key in ("fieldType", "subDomain", "target", "ttl")}
            result = self.call("POST", f"/domain/zone/{plan['zone']}/record", body)
            if not isinstance(result, dict):
                raise DnsFailure("OVH returned no record ID; preserve pending recovery state.")
            plan["id"] = positive(result.get("id"))
            plan["pending"] = False
            save()
            self.inspect(plan)
        self.call("POST", f"/domain/zone/{plan['zone']}/refresh")
        plan["published"] = True
        save()

    def remove(self, plan, save):
        row = self.inspect(plan, allow_missing=True)
        save()
        if plan.get("id") is None:
            plan["deleted"] = True
            save()
            return
        if row is not None:
            plan["deletion_pending"] = True
            save()
            self.call("DELETE", f"/domain/zone/{plan['zone']}/record/{positive(plan['id'])}")
        plan["deleted"] = True
        save()
        self.call("POST", f"/domain/zone/{plan['zone']}/refresh")
        plan["deletion_pending"] = False
        save()
