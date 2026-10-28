#!/usr/bin/env python3
"""Check disk admission refusals against the paused, isolated one-node E2E lab.

A dedicated empty tmpfs storage changes capacity without filling the lab disk.
The resize harness supplies run guards, scoped credentials and server startup.
"""

import argparse
import importlib.util
import json
import pathlib
import tomllib

spec = importlib.util.spec_from_file_location("resize_fixture", pathlib.Path(__file__).with_name("e2e-resize.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Capacity(fixture.Matrix):
    def __init__(self, report, output):
        super().__init__(report, output)
        self.owner = "HomeStack E2E capacity " + self.run_id
        self.node_files = "/var/lib/vz/hst-capacity-" + self.run_id
        self.token_name = "hst-capacity-" + self.run_id[:8]
        self.role_name = "HSTCapacity" + self.run_id[:8]
        self.storage = "hst-capacity-" + self.run_id[:8]
        self.mounted = self.storage_created = self.gold_acl = self.storage_acl = self.bridge_acl = False

    def prepare_base(self):
        # No VM is cloned: every apply must fail before the first allocation.
        self.node_command("pveum", "role", "modify", self.role_name, "--append", "1",
                          "--privs", "VM.Clone,Datastore.Audit,SDN.Use")
        token = "root@pam!" + self.token_name
        self.pve("set", "/access/acl", "--path", "/vms/100", "--tokens", token, "--roles", self.role_name)
        self.gold_acl = True
        self.pve("set", "/access/acl", "--path", "/sdn/zones/localnetwork/vmbr0", "--tokens", token,
                 "--roles", self.role_name)
        self.bridge_acl = True
        self.node_command("mkdir", self.node_files + "/storage")
        self.node_command("mount", "-t", "tmpfs", "-o", "size=16G", "tmpfs", self.node_files + "/storage")
        self.mounted = True
        self.pve("create", "/storage", "--storage", self.storage, "--type", "dir",
                 "--path", self.node_files + "/storage", "--content", "images", "--nodes", self.node)
        self.storage_created = True
        self.pve("set", "/access/acl", "--path", "/storage/" + self.storage,
                 "--tokens", token, "--roles", self.role_name)
        self.storage_acl = True

    def start_server(self):
        config = self.output / "server.toml"
        text = config.read_text().replace('name = "pve1"', 'name = "pve1"\ngold_vmid = 100')
        text = text.replace('homestack-storage-1 = ["local"]', f'homestack-storage-1 = ["{self.storage}"]')
        config.write_text(text)
        super().start_server()

    def create_plan(self, size):
        return self.api("POST", "/v1/plans", {"operation": "workspace.create", "parameters": {
            "vmid": fixture.VMID, "name": "hst-capacity", "home_size": size}})

    def verify(self):
        status, rejected = self.create_plan("64G")
        fixture.check(status == 409 and "Not enough free space" in rejected["error"]["message"],
                      f"Oversized create was not refused: {status}: {rejected}")
        status, accepted = self.create_plan("256M")
        fixture.check(status == 201, f"Create with sufficient capacity was refused: {status}: {accepted}")
        self.node_command("mount", "-o", "remount,size=1M", self.node_files + "/storage")
        status, refused = self.api("POST", f"/v1/plans/{accepted['id']}/apply",
                                   {"digest": accepted["digest"]},
                                   {"homestack-config-version": self.version})
        fixture.check(status == 409 and "Not enough free space" in refused["error"]["message"],
                      f"Create did not recheck capacity before accepting apply: {status}: {refused}")
        resources = self.pve("get", "/cluster/resources", "--type", "vm")
        fixture.check(not any(vm["vmid"] == fixture.VMID for vm in resources), "Create allocated a VM before refusal")
        content = self.pve("get", f"/nodes/{self.node}/storage/{self.storage}/content")
        fixture.check(not content, "Create allocated disk content before refusal")
        evidence = {"status": "passed", "lab_run_id": self.run_id, "oversized_plan": rejected,
                    "sufficient_capacity_plan": accepted["summary"], "capacity_changed_apply": {"status": status, "error": refused["error"], "job_created": False},
                    "vm_allocated": False, "storage_content": content}
        (self.output / "results.json").write_text(json.dumps(evidence, indent=2) + "\n")

    def cleanup(self):
        failures = []

        def attempt(name, action):
            try:
                action()
            except Exception as error:
                failures.append(RuntimeError(f"{name}: {error}"))

        attempt("stop server", self.stop_server)
        token = "root@pam!" + self.token_name
        for owned, name, path in [
            (self.gold_acl, "Gold ACL", "/vms/100"),
            (self.bridge_acl, "bridge ACL", "/sdn/zones/localnetwork/vmbr0"),
            (self.storage_acl, "storage ACL", "/storage/" + self.storage),
        ]:
            if owned:
                attempt(name, lambda path=path: self.pve(
                    "set", "/access/acl", "--path", path, "--tokens", token,
                    "--roles", self.role_name, "--delete", "1"))
        if self.storage_created:
            attempt("storage object", lambda: self.pve("delete", "/storage/" + self.storage))
        if self.mounted:
            attempt("tmpfs mount", lambda: self.node_command("umount", self.node_files + "/storage"))
        attempt("fixture cleanup", super().cleanup)
        if failures:
            raise ExceptionGroup("Capacity cleanup failed", failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binding = tomllib.loads((pathlib.Path.home() / ".config/homestack/profiles/e2e/run.toml").read_text())
    report = json.loads((pathlib.Path(binding["artifacts"]) / "report.json").read_text())
    fixture.check(report["run_id"] == binding["run_id"] and report["status"] == "paused"
                  and report["phase"] == "verify", "E2E profile is not a paused verified lab")
    fixture.check(pathlib.Path(report["control_socket"]).is_socket(), "E2E controller is missing")
    ssh = pathlib.Path(report["ssh_config"]).read_text()
    fixture.check("HostName 127.0.0.1" in ssh and f"Port {int(report['ssh_port'])}" in ssh,
                  "E2E forwarding is not pinned to loopback")
    matrix = Capacity(report, args.output)
    matrix.node_command("test", "!", "-e", matrix.node_files)
    fixture.check(not any(s["storage"] == matrix.storage for s in matrix.pve("get", "/storage")),
                  "Fixture storage already exists")
    try:
        matrix.prepare()
        matrix.verify()
    except BaseException as verification_error:
        try:
            matrix.cleanup()
        except Exception as cleanup_error:
            raise BaseExceptionGroup("Capacity verification and cleanup failed",
                                     [verification_error, cleanup_error]) from None
        raise
    else:
        matrix.cleanup()
    print(json.dumps({"status": "passed", "checks": 3, "cleanup": "complete"}))


if __name__ == "__main__":
    main()
