# PVE metrics: operation, release and node updates

## How metrics flow

```text
PVE node: homestack-pve-agent (systemd)
  → OTLP/HTTP JSON → local otel-N Collector LXC:4318/v1/metrics
  → OTLP/HTTP → Prometheus:9090/api/v1/otlp/v1/metrics
```

The agent reads running VM/LXC cgroup v2 counters: CPU, RAM/cache, swap, OOM,
throttling, PSI, I/O and PID counts. It reads LXC filesystem capacity through
the host's `/proc` and VM filesystems through QEMU Guest Agent, which must be
enabled and running in the VM. No monitoring process is installed inside LXC.

Defaults: cgroups and export every 5 seconds, LXC filesystems every 30 seconds,
VM filesystems every 60 seconds. Cached filesystem values retain their actual
measurement timestamps. Missing readings are omitted, not reported as zero.
The latest diagnostic snapshot is `/run/homestack-pve-agent/snapshot.json`.
The agent has no persistent export queue; the Collector queues accepted data
on disk, surviving an ordinary restart of the same Collector container.

Agent metrics use `hs_pve_`, with `node`, `vmid`, `type` (`lxc`/`qemu`) and
optional `name`; filesystem metrics also have `mount` and `filesystem`.
Native Proxmox metric-server data remains separate under `proxmox_`.
The HomeStack API reads LXC telemetry for `GET /v1/status` from Prometheus with
one PromQL query per request, so clients never query Prometheus themselves.
Enter the Prometheus URL once in the TUI under Settings → Integrations →
Prometheus; it is kept as `[server] prometheus_url` in `deploy.toml`, and API
initialization and updates pass it to every instance as
`HOMESTACK_PROMETHEUS_URL`. Without it status has no service telemetry. Values
measure the whole LXC; samples older than 30 seconds are omitted. Uptime is
the container's (`proxmox_vm_uptime_seconds`) and process RSS is not reported;
application health remains the service health check.

## Release from the development machine

For agent application/package changes, build, review, commit and push, then tag:

```sh
deploy/pve-agent/build-static.sh
git diff --check
git diff
git add <task-files>
git commit -m "Describe the agent change"
git push
python3 scripts/tag-pve-agent.py --dry-run
python3 scripts/tag-pve-agent.py
```

The build uses the Cargo wrapper and produces
`target/x86_64-unknown-linux-musl/release/homestack-pve-agent`
(requires the `x86_64-unknown-linux-musl` Rust target). The tag helper requires
a clean, already pushed HEAD and selects the next `pve-agent-X.Y.Z` patch.
Never move a release tag or overwrite a published package.

Wait for GitHub Actions checks, Debian package publication in GitHub Releases,
and the signed APT repository deployment to finish. The package is available at
`https://hipotures.github.io/homestack/apt/` only after that deployment.
A branch push alone does not publish a package. Documentation needs no release.
Collector image changes use the independent
`python3 scripts/tag-image.py --service otel` release procedure; see the
[Collector documentation](../deploy/otel/README.md).

## Install once on each PVE node

Run as root (curl, GnuPG and CA certificates required):

```sh
curl -fsSLo /tmp/homestack-apt-setup.sh https://hipotures.github.io/homestack/apt/setup.sh
bash /tmp/homestack-apt-setup.sh
apt-get install homestack-pve-agent
```

The package installs the binary, systemd unit and
`/etc/homestack/pve-agent.toml`. Set its initially empty `otlp_endpoint`:

| Node | Endpoint |
| --- | --- |
| pve1 | `http://otel-1.homestack.app:4318/v1/metrics` |
| pve2 | `http://otel-2.homestack.app:4318/v1/metrics` |
| pve3 | `http://otel-3.homestack.app:4318/v1/metrics` |

`localhost` points to PVE itself, not the Collector LXC. After editing:

```sh
homestack-pve-agent --check-config --config /etc/homestack/pve-agent.toml
systemctl restart homestack-pve-agent
```

For a previous manual/SCP installation, first follow the
[manual-installation replacement steps](../deploy/pve-agent/README.md#install-on-each-pve-node-through-apt)
to remove the old binary and overriding unit while preserving configuration.

## Update existing nodes and verify

Run on each PVE node after APT publication:

```sh
apt-get update
apt-get install --only-upgrade homestack-pve-agent
dpkg-query -W homestack-pve-agent
systemctl show homestack-pve-agent -p ActiveState -p MainPID -p ExecMainStartTimestamp
journalctl -u homestack-pve-agent -n 30 --no-pager
```

APT replaces the binary/unit, reloads systemd and restarts a configured service,
respecting Debian service policy and a previously disabled unit. The restart
does not print a separate APT message. Operator config edits are preserved as a
dpkg conffile; conflicting template changes can prompt. No PVE or guest reboot
is needed. Restart manually after configuration edits. Updates are installed
only when requested unless the administrator configures automatic updates.

In Prometheus (`http://prom.homestack.app:9090`), check after about a minute:

```promql
time() - timestamp(hs_pve_agent_instances)
hs_pve_agent_export_success_ratio
hs_pve_filesystem_read_success_ratio{type="lxc",mount="/"}
hs_pve_filesystem_used_bytes{type="lxc"}
rate(hs_pve_cpu_usage_seconds_total[5m])
```

Expect recent agent samples from each node, export/read success `1`, and actual
filesystem sizes. Export success describes the previous delivery. For read
failures inspect the local snapshot's per-mount `error`. Version `0.1.1` fixed
LXC `Function not implemented (os error 38)` by removing systemd's
`RestrictSUIDSGID`, which blocked the required `openat2` call. Keep that option
disabled, including in local drop-ins.

Full references: [agent configuration and metric semantics](../deploy/pve-agent/README.md),
[APT signing and publication](../deploy/apt/README.md).
