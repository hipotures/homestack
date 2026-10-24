#!/usr/bin/env python3
"""Phase 16 live resize matrix, restricted to the paused disposable E2E lab.

Uses only VMIDs 240 and 241 in homestack-test. All resize operations go
through a separate local HomeStack server with a token scoped to that pool.
"""

import argparse
import hashlib
import json
import os
import pathlib
import shlex
import socket
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
import uuid

REPO = pathlib.Path(__file__).resolve().parent.parent
GUEST = (REPO / "scripts/e2e-resize-guest.py").read_text()
POOL = "homestack-test"
VMID = 240
BASE = 241
MIB = 1024**2
GROWTH_MIB = 256
PASSWORD = "hs-resize-fixture-passphrase"
LAYOUTS = ("ext4-whole", "ext4-partition", "lvm", "luks-key-lvm", "luks-pass-lvm",
           "btrfs-subvolume", "btrfs-luks")
REFUSALS = {"btrfs-multi": "single-device btrfs", "lvm-multi": "one logical volume",
            "partition-not-last": "not the last partition"}


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


class Matrix:
    def __init__(self, report, output, reuse_base=False):
        self.report = report
        self.reuse_base = reuse_base
        self.run_id = str(uuid.UUID(report["run_id"]))
        self.work = pathlib.Path(report["temporary_directory"])
        self.output = output.resolve()
        self.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.owner = "HomeStack E2E resize " + self.run_id
        self.node_files = "/var/lib/vz/hst-resize-" + self.run_id
        self.token_name = "hst-resize-" + self.run_id[:8]
        self.role_name = "HSTResize" + self.run_id[:8].replace("-", "")
        self.server = None
        self.tunnel = None
        self.results = []
        self.jobs = []
        self.owned = set()
        self.created_pool = False
        self.created_role = False
        self.created_token = False
        self.preserved = {}
        self.node = self.node_command("hostname", "-s").decode().strip()
        check(self.node == "pve1", "Unexpected E2E node")
        self.guest_key = self.output / "guest-key"
        self.api_token = "phase16-api-" + uuid.uuid4().hex
        self.version = None

    def node_command(self, *args, data=None, timeout=900):
        # Every node operation verifies the bootstrap's run marker first.
        guard = ("import json,subprocess,sys; "
                 f"assert json.load(open('/etc/homestack-e2e.json'))['run_id']=={self.run_id!r}; "
                 "p=subprocess.run(sys.argv[1:],input=sys.stdin.buffer.read(),capture_output=True); "
                 "sys.stdout.buffer.write(p.stdout);sys.stderr.buffer.write(p.stderr);sys.exit(p.returncode)")
        command = ["/usr/bin/ssh", "-F", self.report["ssh_config"], "-o", "BatchMode=yes",
                   "pve-e2e", shlex.join(["python3", "-c", guard, *map(str, args)])]
        result = subprocess.run(command, input=data if data is not None else b"", capture_output=True, timeout=timeout)
        if result.returncode:
            # No credentials are included in failed command descriptions.
            raise RuntimeError(f"E2E node command {args[0]} failed: {result.stderr.decode(errors='replace')[-3000:]}")
        return result.stdout

    def pve(self, verb, path, *args):
        output = self.node_command("pvesh", verb, path, "--output-format", "json", *args)
        return json.loads(output) if output.strip() else None

    def qm(self, *args):
        return self.node_command("qm", *map(str, args))

    def config(self, vmid):
        return self.pve("get", f"/nodes/{self.node}/qemu/{vmid}/config")

    def assert_owned(self, vmid):
        check(vmid in (VMID, BASE) and vmid in self.owned, "Unowned VM operation refused")
        check(self.config(vmid).get("description", "").split("\n", 1)[0] == self.owner, "VM ownership marker changed")

    def guest(self, vmid, command, timeout=900):
        self.assert_owned(vmid)
        result = json.loads(self.qm("guest", "exec", vmid, "--timeout", timeout,
                                    "--", "nsenter", "--mount=/proc/1/ns/mnt", "--", "/bin/sh", "-lc", command))
        check(result.get("exitcode") == 0, f"Guest command failed ({result.get('exitcode')}): "
              f"{result.get('err-data', '')[-2000:]} {result.get('out-data', '')[-2000:]}")
        return result.get("out-data", "")

    def guest_helper(self, action, *args, vmid=VMID):
        text = self.guest(vmid, shlex.join(["python3", "-c", GUEST, action, *args]))
        return json.loads(text.splitlines()[-1])

    def boot(self, vmid):
        self.assert_owned(vmid)
        self.qm("start", vmid)
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            try:
                self.guest(vmid, "true", timeout=5)
                return
            except (RuntimeError, subprocess.TimeoutExpired):
                time.sleep(2)
        raise RuntimeError(f"VM {vmid} guest agent did not become ready")

    def stop(self, vmid):
        self.assert_owned(vmid)
        if self.qm("status", vmid).decode().strip() == "status: running":
            self.qm("stop", vmid)

    def destroy(self, vmid):
        self.assert_owned(vmid)
        self.stop(vmid)
        self.qm("destroy", vmid, "--purge", "1", "--destroy-unreferenced-disks", "1")
        self.owned.remove(vmid)

    def prepare(self):
        print(json.dumps({"stage": "prepare-base"}), flush=True)
        resources = self.pve("get", "/cluster/resources", "--type", "vm")
        if self.reuse_base:
            fixtures = [vm for vm in resources if 240 <= vm["vmid"] <= 249]
            check(any(vm["vmid"] == BASE for vm in fixtures), "Reusable base is missing")
            check(all(vm["vmid"] in (BASE, VMID) and vm.get("pool") == POOL for vm in fixtures),
                  "Test range contains a foreign fixture")
            for vm in fixtures:
                description = self.config(vm["vmid"]).get("description", "")
                check(description.split("\n", 1)[0] == self.owner, "Fixture ownership marker changed")
            check(self.config(BASE).get("template") == 1, "Reusable base is not a template")
            self.owned.update(vm["vmid"] for vm in fixtures)
            if VMID in self.owned:
                self.destroy(VMID)
        else:
            check(not any(240 <= vm["vmid"] <= 249 for vm in resources), "Test VM range is occupied")
        for vm in resources:
            if vm["vmid"] in (100, 101):
                kind = "qemu" if vm["type"] == "qemu" else "lxc"
                path = f"/nodes/{self.node}/{kind}/{vm['vmid']}/config"
                self.preserved[path] = self.pve("get", path)
        pools = self.pve("get", "/pools")
        self.created_pool = any(pool["poolid"] == POOL and pool.get("comment") == self.owner for pool in pools)
        if not any(pool["poolid"] == POOL for pool in pools):
            self.pve("create", "/pools", "--poolid", POOL, "--comment", self.owner)
            self.created_pool = True
        if self.reuse_base:
            for name in ("vmlinuz", "initrd"):
                self.node_command("test", "-s", self.node_files + "/" + name)
        else:
            self.node_command("mkdir", "-m", "700", self.node_files)
        self.node_command("pveum", "role", "add", self.role_name, "--privs",
                          "Pool.Audit,VM.Audit,VM.Config.Disk,VM.Config.Options,VM.GuestAgent.Audit,"
                          "VM.GuestAgent.FileRead,VM.GuestAgent.Unrestricted,Datastore.AllocateSpace")
        self.created_role = True
        token = self.pve("create", f"/access/users/root@pam/token/{self.token_name}", "--privsep", "1")
        self.created_token = True
        token_id = "root@pam!" + self.token_name
        self.pve("set", "/access/acl", "--path", "/pool/" + POOL, "--tokens", token_id,
                 "--roles", self.role_name, "--propagate", "1")
        for path in ("/nodes", "/storage"):
            self.pve("set", "/access/acl", "--path", path, "--tokens", token_id,
                     "--roles", "PVEAuditor", "--propagate", "1")
        self.pve("set", "/access/acl", "--path", "/storage/local", "--tokens", token_id,
                 "--roles", self.role_name, "--propagate", "1")
        self.token_file = self.output / "pve-token"
        self.token_file.write_text(token_id + "=" + token["value"] + "\n")
        self.token_file.chmod(0o600)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.guest_key)], check=True)
        public = self.guest_key.with_suffix(".pub").read_text().strip()
        key_path = self.node_files + "/guest.pub"
        self.node_command("tee", key_path, data=(public + "\n").encode())
        if not self.reuse_base:
            self.prepare_base()
        config = (f'version = 1\n[node]\nname = "{self.node}"\n'
                  '[storage_layouts]\nhomestack-storage-1 = ["local"]\n'
                  '[root]\ndisk = "scsi0"\n[home]\ndisk = "scsi1"\ndefault_size = "1G"\n'
                  '[network]\nprefix = "10.77.0"\ncidr = 24\ngateway = "10.77.0.1"\ndns = ["10.77.0.1"]\n'
                  '[cloud_init]\nsnippet_storage = "local"\nsnippet_dir = "/var/lib/vz/snippets"\n'
                  '[user]\nname = "user"\nuid = 1000\ngid = 1000\n')
        (self.output / "server.toml").write_text(config)
        (self.output / "server.toml").chmod(0o600)
        (self.output / "api-token").write_text(self.api_token)
        (self.output / "api-token").chmod(0o600)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.proxy_port = probe.getsockname()[1]
        self.tunnel_log = open(self.output / "tunnel.log", "ab")
        self.tunnel = subprocess.Popen([
            "/usr/bin/ssh", "-F", self.report["ssh_config"], "-N", "-o", "BatchMode=yes",
            "-o", "ExitOnForwardFailure=yes", "-D", f"127.0.0.1:{self.proxy_port}", "pve-e2e"],
            stdout=subprocess.DEVNULL, stderr=self.tunnel_log)
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", self.proxy_port), timeout=1):
                    break
            except OSError:
                check(self.tunnel.poll() is None, "SSH SOCKS tunnel exited")
                time.sleep(.1)
        else:
            raise RuntimeError("SSH SOCKS tunnel did not start")
        self.start_server()

    def prepare_base(self):
        public = self.guest_key.with_suffix(".pub").read_text().strip()
        key_path = self.node_files + "/guest.pub"
        self.qm("clone", 100, BASE, "--full", "1", "--name", "hst-resize-base", "--pool", POOL)
        # Register only a newly allocated VM; existing VMIDs were refused above.
        self.owned.add(BASE)
        self.qm("set", BASE, "--description", self.owner, "--tags", "homestack-ws", "--memory", "768",
                "--scsi1", "local:1,format=qcow2,serial=HS_HOME_241", "--ciuser", "user",
                "--ipconfig0", "ip=10.77.0.241/24,gw=10.77.0.1", "--sshkeys", key_path,
                "--delete", "cicustom")
        self.qm("cloudinit", "update", BASE)
        print(json.dumps({"stage": "boot-base"}), flush=True)
        self.boot(BASE)
        print(json.dumps({"stage": "install-layout-tools"}), flush=True)
        baseline = self.guest_helper("baseline", public, vmid=BASE)
        (self.output / "baseline.json").write_text(json.dumps(baseline, indent=2) + "\n")
        kernel = baseline["kernel"]
        print(json.dumps({"stage": "capture-boot-files"}), flush=True)
        # Transfer the fixture kernel/initrd through the authenticated guest connection.
        proxy = shlex.join(["/usr/bin/ssh", "-F", self.report["ssh_config"], "pve-e2e", "-W", "%h:%p"])
        for source, name in ((f"/boot/vmlinuz-{kernel}", "vmlinuz"),
                             (f"/boot/initrd.img-{kernel}", "initrd")):
            command = ["/usr/bin/ssh", "-F", "/dev/null", "-o", "ProxyCommand=" + proxy,
                       "-o", "BatchMode=yes", "-o", "PasswordAuthentication=no",
                       "-o", "StrictHostKeyChecking=accept-new", "-o", "UserKnownHostsFile=" + str(self.output / "known_hosts"),
                       "-o", "GlobalKnownHostsFile=/dev/null", "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
                       "-i", str(self.guest_key), "root@10.77.0.241", shlex.join(["cat", source])]
            result = subprocess.run(command, capture_output=True, timeout=180)
            if result.returncode:
                (self.output / "guest-ssh.log").write_bytes(result.stderr + result.stdout)
            check(result.returncode == 0, "Cannot read fixture boot files over isolated SSH: " + result.stderr.decode())
            self.node_command("dd", "of=" + self.node_files + "/" + name, "bs=1M", "status=none", data=result.stdout)
        self.stop(BASE)
        self.qm("template", BASE)

    def start_server(self):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        env = {"PATH": os.environ["PATH"], "HOMESTACK_LISTEN": f"127.0.0.1:{port}",
               "HOMESTACK_API_TOKEN_FILE": str(self.output / "api-token"),
               "HOMESTACK_PVE_ENDPOINTS": "https://pve.e2e.homestack.app:8006",
               "HTTPS_PROXY": f"socks5h://127.0.0.1:{self.proxy_port}",
               "HOMESTACK_PVE_CA_FILE": str(pathlib.Path(self.report["artifacts"]) / "pve-certificate.pem"),
               "HOMESTACK_PVE_TOKEN_FILE": str(self.token_file),
               "HOMESTACK_CONFIG_FILE": str(self.output / "server.toml"),
               "HOMESTACK_PVE_POOL": POOL, "HOMESTACK_LOG": "warn",
               "HOMESTACK_STATE_DIR": str(self.output / "state")}
        self.server_log = open(self.output / "server.log", "ab")
        self.server = subprocess.Popen([str(REPO / "target/debug/homestack-server")], env=env,
                                       stdout=self.server_log, stderr=self.server_log)
        for _ in range(100):
            try:
                status, _ = self.api("GET", "/health/live")
                if status == 200:
                    status, version = self.api("GET", "/v1/config/version")
                    check(status == 200, f"Configuration version failed: {version}")
                    self.version = version["config_version"]
                    return
            except (OSError, urllib.error.URLError):
                pass
            check(self.server.poll() is None, "Test server exited; inspect server.log")
            time.sleep(.1)
        raise RuntimeError("Test server did not start")

    def stop_server(self):
        if self.server:
            self.server.kill()
            self.server.wait(timeout=15)
            self.server = None
            self.server_log.close()

    def api(self, method, path, payload=None, headers=None):
        body = json.dumps(payload).encode() if payload is not None else None
        request = urllib.request.Request(self.url + path, data=body, method=method,
                                         headers={"Authorization": "Bearer " + self.api_token,
                                                  "Content-Type": "application/json", **(headers or {})})
        try:
            response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            data = response.read()
            return response.status, json.loads(data) if data else None

    def plan(self, role, size):
        return self.api("POST", "/v1/plans", {"operation": "workspace.resize", "parameters": {
            "vmid": VMID, role + "_size": f"{size}M"}})

    def apply(self, plan):
        payload = {"digest": plan["digest"]}
        if "luks_passphrase" in plan["summary"].get("secrets_required", []):
            payload["secrets"] = {"luks_passphrase": PASSWORD}
        status, job = self.api("POST", f"/v1/plans/{plan['id']}/apply", payload,
                               {"homestack-config-version": self.version})
        check(status == 202, f"Apply refused: {job}")
        return job

    def finish(self, job):
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline:
            _, state = self.api("GET", "/v1/jobs/" + job["id"])
            if state["state"] in ("succeeded", "failed"):
                self.jobs.append(state)
                (self.output / "jobs.json").write_text(json.dumps(self.jobs, indent=2) + "\n")
                check(state["state"] == "succeeded", f"Resize job failed: {state}")
                return state
            time.sleep(.25)
        raise RuntimeError("Resize job timed out")

    def clone(self, role, layout):
        self.qm("clone", BASE, VMID, "--name", "hst-resize-" + layout, "--pool", POOL)
        self.owned.add(VMID)
        self.qm("set", VMID, "--description", self.owner, "--tags", "homestack-ws",
                "--ipconfig0", "ip=10.77.0.240/24,gw=10.77.0.1")
        config = self.config(VMID)
        if role == "home":
            self.qm("set", VMID, "--delete", "scsi1")
            self.qm("set", VMID, "--scsi1", "local:1,format=qcow2,serial=HS_HOME_240")
        else:
            home = config["scsi1"].split(",")[0]
            self.qm("set", VMID, "--scsi1", home + ",serial=HS_HOME_240")
        self.qm("cloudinit", "update", VMID)
        self.boot(VMID)
        cloud_init = self.guest_helper("cloud-init-ready")
        (self.output / "cloud-init.json").write_text(json.dumps(cloud_init, indent=2) + "\n")
        if role == "root":
            self.guest_helper("relabel-home")
        self.guest(VMID, "sed -i s/HS_HOME_241/HS_HOME_240/g /etc/fstab; systemctl daemon-reload")
        if role == "root":
            self.qm("set", VMID, "--scsi2", "local:8,format=qcow2,discard=on,serial=HS_ROOT_240")
            self.guest(VMID, "udevadm settle")
        public = self.guest_key.with_suffix(".pub").read_text().strip()
        self.guest(VMID, "timeout 180 sh -c 'until test -f /run/cloud-init/result.json; do sleep 1; done'")
        self.guest(VMID, shlex.join(["python3", "-c", GUEST, "authorize", public]))
        host_key = self.guest(VMID, "cat /etc/ssh/ssh_host_ed25519_key.pub").strip()
        self.variant_known_hosts = self.output / "variant-known-hosts"
        self.variant_known_hosts.write_text("hst-resize-240 " + host_key + "\n")
        info = json.loads(self.guest_ssh(shlex.join([
            "nsenter", "--mount=/proc/1/ns/mnt", "--", "python3", "-c", GUEST, "prepare", role, layout
        ])).decode().splitlines()[-1])
        if role == "root" and info["encrypted"]:
            initrd = self.guest_ssh('cat /boot/initrd.img-$(uname -r)')
            self.node_command("tee", self.node_files + "/initrd", data=initrd)
        if role == "root":
            self.stop(VMID)
            target = self.config(VMID)["scsi2"].split(",")[0]
            self.qm("set", VMID, "--delete", "scsi0,scsi2")
            self.qm("set", VMID, "--scsi0", target)
            append = (f"root={info['root']} rootfstype={info['filesystem']} rootflags={info['rootflags']} "
                      f"rw console=ttyS0 hst.crypt={int(info['encrypted'])}")
            args = shlex.join(["-kernel", self.node_files + "/vmlinuz", "-initrd", self.node_files + "/initrd",
                               "-append", append])
            self.qm("set", VMID, "--args", args)
            self.boot(VMID)
        mount = "/" if role == "root" else "/home/user"
        marker = role + ":" + layout
        self.guest(VMID, f"printf %s {shlex.quote(marker)} > {mount.rstrip('/')}/hst-resize-marker; sync")
        return info

    def guest_ssh(self, command):
        self.assert_owned(VMID)
        proxy = shlex.join(["/usr/bin/ssh", "-F", self.report["ssh_config"], "pve-e2e", "-W", "%h:%p"])
        result = subprocess.run([
            "/usr/bin/ssh", "-F", "/dev/null", "-o", "ProxyCommand=" + proxy, "-o", "BatchMode=yes",
            "-o", "IdentityAgent=none", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "HostKeyAlias=hst-resize-240", "-o", "UserKnownHostsFile=" + str(self.variant_known_hosts),
            "-o", "GlobalKnownHostsFile=/dev/null", "-i", str(self.guest_key), "root@10.77.0.240", command],
            input=b"", capture_output=True, timeout=900)
        check(result.returncode == 0, "Fixture SSH failed: " + result.stderr.decode(errors="replace")[-3000:])
        return result.stdout

    def snapshot(self, role):
        value = self.guest_helper("snapshot", role)
        value["config_digest"] = self.config(VMID)["digest"]
        return value

    def case(self, role, layout, refusal=None):
        print(json.dumps({"role": role, "layout": layout, "stage": "prepare"}), flush=True)
        result = {"role": role, "layout": layout}
        try:
            info = self.clone(role, layout)
            before = self.snapshot(role)
            result["before"] = before
            size = before["disk_bytes"] // MIB + GROWTH_MIB
            status, plan = self.plan(role, size)
            if refusal:
                check(status >= 400 and refusal in json.dumps(plan), f"Expected {refusal!r}, got {status}: {plan}")
                after = self.snapshot(role)
                check(before == after, "Refused plan changed VM configuration or disk/filesystem state")
                result.update(status="passed", refusal=plan, before=before, after=after)
                return
            check(status == 201, f"Resize plan refused: {plan}")
            status, shrink = self.plan(role, before["disk_bytes"] // MIB - 1)
            check(status == 409 and "Shrinking is not supported" in json.dumps(shrink),
                  f"Shrink did not return the documented refusal: {status}: {shrink}")
            check(before == self.snapshot(role), "Refused shrink changed disk or guest state")
            result["shrink_refusal"] = shrink
            self.finish(self.apply(plan))
            grown = self.snapshot(role)
            result["grown"] = grown
            check(grown["disk_bytes"] == size * MIB, "Incorrect grown disk size")
            check(grown["filesystem_bytes"] > before["filesystem_bytes"] + GROWTH_MIB * MIB * .8, "Filesystem did not grow")
            check(grown["marker"] == before["marker"], "Data marker changed")
            for field in ("filesystem_uuid", "marker_identity"):
                check(grown[field] == before[field], f"Resize changed {field}")
            print(json.dumps({"role": role, "layout": layout, "stage": "grown"}), flush=True)
            status, plan = self.plan(role, size)
            check(status == 201, f"Repeated resize plan refused: {plan}")
            self.finish(self.apply(plan))
            repeated = self.snapshot(role)
            result["repeated"] = repeated
            check(grown["disk_bytes"] == repeated["disk_bytes"] and
                  grown["filesystem_bytes"] == repeated["filesystem_bytes"], "Same-size resize changed capacity")
            print(json.dumps({"role": role, "layout": layout, "stage": "same-size-passed"}), flush=True)
            self.guest_helper("barrier", "install", info["filesystem"])
            status, plan = self.plan(role, size + GROWTH_MIB)
            check(status == 201, f"Interruption plan refused: {plan}")
            self.apply(plan)
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                if self.guest(VMID, "if test -s /run/hst-resize-blocked; then echo blocked; fi").strip() == "blocked":
                    break
                time.sleep(.5)
            else:
                raise RuntimeError("Filesystem barrier was not reached")
            self.stop_server()
            interrupted = self.snapshot(role)
            result["interrupted"] = interrupted
            check(interrupted["disk_bytes"] == (size + GROWTH_MIB) * MIB, "Interruption happened before disk growth")
            check(interrupted["filesystem_bytes"] == grown["filesystem_bytes"], "Filesystem already grew before interruption")
            self.guest_helper("barrier", "remove", info["filesystem"])
            self.start_server()
            print(json.dumps({"role": role, "layout": layout, "stage": "resume"}), flush=True)
            status, plan = self.plan(role, size + GROWTH_MIB)
            check(status == 201, f"Resume plan refused: {plan}")
            self.finish(self.apply(plan))
            resumed = self.snapshot(role)
            check(resumed["disk_bytes"] == interrupted["disk_bytes"], "Resume enlarged the disk again")
            check(resumed["filesystem_bytes"] > interrupted["filesystem_bytes"] + GROWTH_MIB * MIB * .8, "Resume did not finish filesystem growth")
            check(resumed["marker"] == before["marker"], "Data marker changed after resume")
            for field in ("filesystem_uuid", "marker_identity"):
                check(resumed[field] == before[field], f"Recovery changed {field}")
            result.update(status="passed", resumed=resumed)
        except Exception as error:
            result.update(status="failed", error=str(error))
            raise
        finally:
            self.results.append(result)
            print(json.dumps({"role": role, "layout": layout, "status": result.get("status")}), flush=True)
            try:
                if VMID in self.owned:
                    self.destroy(VMID)
            except Exception as error:
                result["cleanup_error"] = str(error)
                if result.get("status") != "failed":
                    raise
            finally:
                (self.output / "results.json").write_text(json.dumps(self.results, indent=2))

    def cleanup(self, retain_base=False):
        self.stop_server()
        if self.tunnel:
            self.tunnel.terminate()
            self.tunnel.wait(timeout=15)
            self.tunnel_log.close()
        retain_base = (retain_base and BASE in self.owned and self.config(BASE).get("template") == 1)
        errors = []
        for vmid in (VMID, BASE):
            if vmid in self.owned and not (retain_base and vmid == BASE):
                try:
                    self.destroy(vmid)
                except Exception as error:
                    errors.append(str(error))
        if self.created_token:
            try:
                token = "root@pam!" + self.token_name
                for path, role in (("/pool/" + POOL, self.role_name), ("/nodes", "PVEAuditor"),
                                   ("/storage", "PVEAuditor"), ("/storage/local", self.role_name)):
                    self.pve("set", "/access/acl", "--path", path, "--tokens", token, "--roles", role, "--delete", "1")
                self.pve("delete", f"/access/users/root@pam/token/{self.token_name}")
            except Exception as error:
                errors.append(str(error))
        if self.created_role:
            try:
                self.node_command("pveum", "role", "delete", self.role_name)
            except Exception as error:
                errors.append(str(error))
        if self.created_pool and not self.owned:
            try:
                self.pve("delete", "/pools/" + POOL)
            except Exception as error:
                errors.append(str(error))
        if not self.owned:
            self.node_command("rm", "-rf", self.node_files)
        preserved = {}
        for path, before in self.preserved.items():
            after = self.pve("get", path)
            digest = lambda value: hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()
            preserved[path] = {"before_sha256": digest(before), "after_sha256": digest(after)}
            if before != after:
                errors.append(f"Existing VM/container configuration changed: {path}")
        resources = self.pve("get", "/cluster/resources", "--type", "vm")
        remaining = [vm["vmid"] for vm in resources if vm["vmid"] in (VMID, BASE)]
        if remaining != ([BASE] if retain_base else []):
            errors.append(f"Fixture VMs remain: {remaining}")
        (self.output / "cleanup.json").write_text(json.dumps({"preserved": preserved,
            "remaining_fixture_vmids": remaining, "retained_base": retain_base, "errors": errors}, indent=2))
        check(not errors, "Fixture cleanup failed: " + "; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--reuse-base", action="store_true", help="Reuse only this run's owned template after a failed matrix")
    parser.add_argument("--keep-base-on-failure", action="store_true", help="Retain the owned template for an explicit retry")
    parser.add_argument("--role", choices=("root", "home"), action="append")
    parser.add_argument("--layout", choices=(*LAYOUTS, *REFUSALS), action="append")
    args = parser.parse_args()
    profile = pathlib.Path.home() / ".config/homestack/profiles/e2e/run.toml"
    binding = tomllib.loads(profile.read_text())
    report = json.loads((pathlib.Path(binding["artifacts"]) / "report.json").read_text())
    check(report["run_id"] == binding["run_id"] and report["status"] == "paused" and report["phase"] == "verify",
          "Profile is not bound to a paused verified E2E lab")
    check(pathlib.Path(report["control_socket"]).is_socket(), "E2E controller is not present")
    ssh_config = pathlib.Path(report["ssh_config"]).read_text()
    check("HostName 127.0.0.1" in ssh_config and f"Port {int(report['ssh_port'])}" in ssh_config,
          "E2E SSH forwarding is not pinned to loopback")
    matrix = Matrix(report, args.output, args.reuse_base)
    failed = True
    try:
        matrix.prepare()
        for role in args.role or ("home", "root"):
            for layout in args.layout or (*LAYOUTS, *REFUSALS):
                matrix.case(role, layout, REFUSALS.get(layout))
        failed = False
    finally:
        matrix.cleanup(retain_base=failed and args.keep_base_on_failure)
    print(json.dumps({"status": "passed", "cases": len(matrix.results), "cleanup": "complete"}), flush=True)


if __name__ == "__main__":
    main()
