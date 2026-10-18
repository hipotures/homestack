#!/usr/bin/env python3
"""Exercise migration and capacity refusals in the selected three-node E2E lab."""

import argparse
import importlib.util
import json
import pathlib
import shlex
import subprocess
import time
import tomllib

spec = importlib.util.spec_from_file_location("resize_fixture", pathlib.Path(__file__).with_name("e2e-resize.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Migration(fixture.Matrix):
    def __init__(self, report, output):
        super().__init__(report, output)
        self.owner = "HomeStack E2E migration " + self.run_id
        self.node_files = "/var/lib/vz/hst-migrate-" + self.run_id
        self.token_name = "hst-migrate-" + self.run_id[:8]
        self.role_name = "HSTMigrate" + self.run_id[:8]
        self.storage = "hst-migrate-" + self.run_id[:8]
        self.target = report["nodes"][1]["name"]
        self.mounted = self.storage_created = self.storage_acl = self.peer_directory = False
        self.evidence = []

    def remote(self, node, *args):
        if node == self.node:
            return self.node_command(*args)
        guard = ("import json,subprocess,sys; "
                 f"assert json.load(open('/etc/homestack-e2e.json'))['run_id']=={self.run_id!r}; "
                 "sys.exit(subprocess.call(sys.argv[1:]))")
        fixture.check(any(entry["name"] == node for entry in self.report["nodes"]), "Unknown E2E node")
        result = subprocess.run([
            "/usr/bin/ssh", "-F", self.report["ssh_config"], "-o", "BatchMode=yes",
            "pve-e2e-" + node, shlex.join(["python3", "-c", guard, *map(str, args)])
        ], input=b"", capture_output=True, timeout=900)
        fixture.check(result.returncode == 0,
                      "E2E peer command failed: " + result.stderr.decode(errors="replace")[-3000:])
        return result.stdout

    def placement(self, vmid=fixture.VMID):
        deadline = time.monotonic() + 30
        while True:
            vm = next((vm for vm in self.pve("get", "/cluster/resources", "--type", "vm")
                       if vm["vmid"] == vmid), None)
            if vm is not None:
                return vm
            fixture.check(time.monotonic() < deadline, f"VM {vmid} is absent from the cluster inventory")
            time.sleep(1)

    def config(self, vmid):
        node = self.placement(vmid)["node"]
        return self.pve("get", f"/nodes/{node}/qemu/{vmid}/config")

    def qm(self, *args):
        vmid = args[2] if args[0] in ("guest", "cloudinit") else args[1]
        node = self.placement(int(vmid))["node"]
        return self.remote(node, "qm", *args)

    def prepare_base(self):
        self.node_command("pveum", "role", "modify", self.role_name, "--append", "1",
                          "--privs", "VM.Migrate,VM.PowerMgmt,Datastore.Audit")
        self.remote(self.target, "mkdir", "-m", "700", self.node_files)
        self.peer_directory = True
        self.remote(self.target, "mkdir", self.node_files + "/storage")
        self.remote(self.target, "mount", "-t", "tmpfs", "-o", "size=64M", "tmpfs", self.node_files + "/storage")
        self.mounted = True
        self.pve("create", "/storage", "--storage", self.storage, "--type", "dir",
                 "--path", self.node_files + "/storage", "--content", "images", "--nodes", self.target)
        self.storage_created = True
        self.pve("set", "/access/acl", "--path", "/storage/" + self.storage,
                 "--tokens", "root@pam!" + self.token_name, "--roles", self.role_name)
        self.storage_acl = True

    def start_server(self):
        path = self.output / "server.toml"
        text = path.read_text()
        for index, entry in enumerate(self.report["nodes"][1:], 2):
            storages = '["local", "' + self.storage + '"]' if entry["name"] == self.target else '["local"]'
            text = text.replace('[root]', f'homestack-storage-{index} = {storages}\n[root]')
        path.write_text(text)
        super().start_server()

    def prepare_workspace(self):
        # The clone is full, with no source-node snippet or backing-file dependency.
        self.node_command("qm", "clone", "100", fixture.VMID, "--full", "1", "--pool", fixture.POOL,
                          "--name", "hst-migrate")
        self.owned.add(fixture.VMID)
        self.qm("set", fixture.VMID, "--description", self.owner, "--tags", "homestack-ws",
                "--delete", "cicustom", "--ciuser", "root", "--sshkeys", self.node_files + "/guest.pub",
                "--ipconfig0", "ip=10.77.0.240/24,gw=10.77.0.1", "--scsi1", "local:1,format=qcow2,serial=HS_HOME_240")
        self.qm("cloudinit", "update", fixture.VMID)
        self.boot(fixture.VMID)
        self.guest(fixture.VMID, "timeout 180 sh -c 'until test -f /run/cloud-init/result.json; do sleep 1; done'")
        public = self.guest_key.with_suffix(".pub").read_text().strip()
        self.guest(fixture.VMID, shlex.join(["python3", "-c", fixture.GUEST, "authorize", public]))
        host_key = self.guest(fixture.VMID, "cat /etc/ssh/ssh_host_ed25519_key.pub").strip()
        self.variant_known_hosts = self.output / "variant-known-hosts"
        self.variant_known_hosts.write_text("hst-resize-240 " + host_key + "\n")
        self.guest(fixture.VMID, "printf '\\nLABEL=HS_HOME_240 /home/user ext4 defaults 0 2\\n' >> /etc/fstab; systemctl daemon-reload")
        self.guest_ssh(shlex.join(["nsenter", "--mount=/proc/1/ns/mnt", "--", "python3", "-c", fixture.GUEST,
                                  "prepare", "home", "ext4-whole"]))
        self.guest(fixture.VMID, "printf %s migration-marker > /home/user/hst-resize-marker; sync")

    def migration_plan(self, target, storage="local"):
        return self.api("POST", "/v1/plans", {"operation": "workspace.migrate", "parameters": {
            "vmid": fixture.VMID, "target_node": target, "target_storage": storage}})

    def capacity_refusals(self):
        before = self.snapshot("home")
        status, jobs_before = self.api("GET", "/v1/jobs")
        fixture.check(status == 200, str(jobs_before))
        content_path = f"/nodes/{self.target}/storage/{self.storage}/content"
        fixture.check(not self.pve("get", content_path), "Capacity fixture storage is not empty")
        status, body = self.migration_plan(self.target, self.storage)
        fixture.check(status == 409 and "Not enough free space" in body["error"]["message"], str(body))
        fixture.check(before == self.snapshot("home") and self.placement()["status"] == "running",
                      "Refused migration changed or shut down the source VM")
        fixture.check(self.api("GET", "/v1/jobs") == (200, jobs_before)
                      and not self.pve("get", content_path), "Refused plan created a job or target disk")
        self.evidence.append({"case": "capacity-plan", "status": "passed", "message": body["error"]["message"]})
        self.remote(self.target, "mount", "-o", "remount,size=16G", self.node_files + "/storage")
        status, plan = self.migration_plan(self.target, self.storage)
        fixture.check(status == 201, str(plan))
        self.remote(self.target, "mount", "-o", "remount,size=15G", self.node_files + "/storage")
        status, still_admissible = self.migration_plan(self.target, self.storage)
        fixture.check(status == 201 and still_admissible["digest"] == plan["digest"],
                      "An admissible free-space change invalidated the migration plan")
        self.remote(self.target, "mount", "-o", "remount,size=1M", self.node_files + "/storage")
        status, body = self.api("POST", f"/v1/plans/{plan['id']}/apply", {"digest": plan["digest"]},
                                {"homestack-config-version": self.version})
        fixture.check(status == 409 and "Not enough free space" in body["error"]["message"], str(body))
        fixture.check(before == self.snapshot("home") and self.placement()["status"] == "running",
                      "Capacity change shut down or changed the source VM")
        fixture.check(self.api("GET", "/v1/jobs") == (200, jobs_before)
                      and not self.pve("get", content_path), "Refused apply created a job or target disk")
        self.evidence.append({"case": "capacity-apply", "status": "passed", "message": body["error"]["message"],
                              "digest_stable_while_admissible": True})

    def migrate(self, target, state):
        print(json.dumps({"stage": "migrate", "target": target, "state": state}), flush=True)
        source = self.placement()["node"]
        fixture.check(self.pve("get", f"/nodes/{source}/qemu/{fixture.VMID}/status/current")["status"] == state,
                      "Source power state does not match the test precondition")
        status, plan = self.migration_plan(target)
        fixture.check(status == 201, str(plan))
        fixture.check(plan["summary"]["status"] == state, "Migration plan used an outdated power state")
        job = self.finish(self.apply(plan))
        deadline = time.monotonic() + 30
        while True:
            vm = self.placement()
            if vm["node"] == target and vm["status"] == state:
                break
            fixture.check(time.monotonic() < deadline, str(vm))
            time.sleep(1)
        if state == "running":
            deadline = time.monotonic() + 120
            while True:
                try:
                    marker = self.snapshot("home")["marker"]
                    fixture.check(marker == "migration-marker", "Persistent home marker changed")
                    break
                except RuntimeError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(1)
        self.evidence.append({"case": "migration", "status": "passed", "node": target, "power_state": state,
                              "home_marker_verified": state == "running", "task_kinds": [t["kind"] for t in job["tasks"]]})
        (self.output / "results.json").write_text(json.dumps(self.evidence, indent=2) + "\n")

    def verify(self):
        status, inventory = self.api("GET", "/v1/status")
        fixture.check(status == 200, str(inventory))
        nodes = inventory["summary"]["nodes"]
        fixture.check(sorted(node["node"] for node in nodes) == ["pve1", "pve2", "pve3"]
                      and all(node["online"] for node in nodes), str(nodes))
        self.evidence.append({"case": "homestack-node-inventory", "status": "passed", "nodes": nodes})
        self.prepare_workspace()
        self.capacity_refusals()
        for entry in [*self.report["nodes"][1:], self.report["nodes"][0]]:
            self.migrate(entry["name"], "running")
        self.stop(fixture.VMID)
        self.migrate(self.target, "stopped")
        self.migrate(self.node, "stopped")
        self.boot(fixture.VMID)
        fixture.check(self.snapshot("home")["marker"] == "migration-marker", "Stopped migrations lost persistent home")
        self.evidence.append({"case": "stopped-marker", "status": "passed"})
        (self.output / "results.json").write_text(json.dumps(self.evidence, indent=2) + "\n")

    def cleanup(self):
        self.stop_server()
        errors = []
        actions = [
            (self.storage_acl, lambda: self.pve("set", "/access/acl", "--path", "/storage/" + self.storage,
                "--tokens", "root@pam!" + self.token_name, "--roles", self.role_name, "--delete", "1")),
            (self.storage_created, lambda: self.pve("delete", "/storage/" + self.storage)),
            (self.mounted, lambda: self.remote(self.target, "umount", self.node_files + "/storage")),
            (self.peer_directory, lambda: self.remote(self.target, "rmdir", self.node_files + "/storage", self.node_files)),
            (True, super().cleanup),
        ]
        for enabled, action in actions:
            if enabled:
                try:
                    action()
                except Exception as error:
                    errors.append(str(error))
        (self.output / "migration-cleanup.json").write_text(json.dumps({"errors": errors}, indent=2) + "\n")
        fixture.check(not errors, "Migration cleanup failed: " + "; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binding = tomllib.loads((pathlib.Path.home() / ".config/homestack/profiles/e2e/run.toml").read_text())
    report = json.loads((pathlib.Path(binding["artifacts"]) / "report.json").read_text())
    fixture.check(report["run_id"] == binding["run_id"] and report["status"] == "paused" and report["phase"] == "verify",
                  "E2E must be paused after verification")
    fixture.check(len(report["nodes"]) == 3, "Cross-node tests require E2E --nodes 3")
    fixture.check(pathlib.Path(report["control_socket"]).is_socket(), "E2E controller is missing")
    ssh = pathlib.Path(report["ssh_config"]).read_text()
    fixture.check("HostName 127.0.0.1" in ssh and f"Port {report['ssh_port']}" in ssh, "E2E SSH is not pinned to loopback")
    matrix = Migration(report, args.output)
    try:
        matrix.prepare()
        matrix.verify()
    finally:
        matrix.cleanup()
    print(json.dumps({"status": "passed", "cases": len(matrix.evidence), "cleanup": "complete"}))


if __name__ == "__main__":
    main()
