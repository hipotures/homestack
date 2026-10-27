# HomeStack PVE agent

`crates/hs-pve-agent` builds `homestack-pve-agent`, a standalone Linux application
installed on each PVE node. It reads local VM/LXC cgroup v2 counters, LXC
filesystems through `/proc`, and VM filesystems through QEMU Guest Agent (QGA).
It does not require the HomeStack API, run Perl or shell commands, or add a
resource-monitoring process inside containers. VM filesystem data requires the
existing QEMU Guest Agent enabled and running in the guest.

The current output is JSON. The daemon atomically replaces a local snapshot;
it does not yet export OTLP or expose a metrics endpoint. Installing this agent
does not change Proxmox's native metric-server configuration.

## Build a static application

On the development machine:

```sh
rustup target add x86_64-unknown-linux-musl
deploy/pve-agent/build-static.sh
file target/x86_64-unknown-linux-musl/release/homestack-pve-agent
```

The script uses the repository Cargo wrapper and Rust's bundled linker. Its
target/linker flags apply only to this command; other workspace builds retain
their normal target. The resulting x86-64 musl ELF does not require shared
libraries or a Rust installation on PVE. A regular local build is also available:

```sh
python3 scripts/cargo.py build --locked -p hs-pve-agent --bin homestack-pve-agent
```

## Install on each PVE node

Copy the static executable and this directory to the node. In the directory
containing `install.sh`, run as root:

```sh
./install.sh /root/homestack-pve-agent
```

The installer validates configuration before changing installed files, installs
the application and unit, enables boot startup, and starts/restarts the service.
Repeated installation updates the binary and unit while preserving an existing
configuration. A default configuration is installed only when none exists.

| Purpose | Installed path |
| --- | --- |
| Application | `/usr/local/sbin/homestack-pve-agent` |
| Configuration | `/etc/homestack/pve-agent.toml` |
| systemd unit | `/etc/systemd/system/homestack-pve-agent.service` |
| Latest snapshot | `/run/homestack-pve-agent/snapshot.json` |

The service runs as root to inspect container processes, mount views and QGA
sockets. systemd makes the filesystem read-only except the runtime directory;
configuration/runtime permissions are restricted. Changing `snapshot_path` to
another directory also requires adapting `ReadWritePaths` and creating that
directory with suitable permissions. Do not enable namespace restrictions that
hide `/proc`, PVE files, host cgroups or QGA sockets.

```sh
systemctl status homestack-pve-agent
journalctl -u homestack-pve-agent -n 50
cat /run/homestack-pve-agent/snapshot.json
```

`DESTDIR=/absolute/staging/path ./install.sh PATH_TO_BINARY` stages the same
files without root or systemd actions. This is intended for packaging and
installation checks, not a second runtime location.

To uninstall, run `./uninstall.sh` as root. It stops/disables the service and
removes the unit and executable, retaining the operator's configuration.

## Configuration and modes

The supplied TOML configuration has these defaults:

```toml
cgroup_root = "/sys/fs/cgroup"
snapshot_path = "/run/homestack-pve-agent/snapshot.json"
interval_seconds = 5
lxc_filesystem_interval_seconds = 30
qga_interval_seconds = 60
qga_parallel = 4
debug = false
```

Unknown keys and invalid values are rejected. Intervals accept 1..86400 seconds;
`qga_parallel` must be positive. Config is read at startup; restart after editing.
Flags override the corresponding TOML settings. Daemon and config-check modes
default to `/etc/homestack/pve-agent.toml`; a one-shot invocation uses built-in
defaults unless `--config` is supplied.

```sh
homestack-pve-agent --check-config --config /etc/homestack/pve-agent.toml
homestack-pve-agent --daemon --config /etc/homestack/pve-agent.toml
homestack-pve-agent --vmid 102008
homestack-pve-agent --qga-parallel 2 --debug > snapshot.json 2> debug.log
homestack-pve-agent --path /sys/fs/cgroup/lxc/102008
```

`--path` inspects an arbitrary cgroup without guessing identity, so only cgroup
metrics are returned in that mode. `--vmid` filters discovered instances.
Neither selector is accepted in daemon mode. `--root` can select another
cgroup v2 root. The daemon does not stream snapshots to the journal; it logs
startup and fatal errors, plus detailed reads when debug is enabled.

Each daemon cycle refreshes local inventory, names and cgroup counters. Cached
filesystem reports are reused until their interval expires. A changed cgroup
device/inode invalidates the instance's cache; vanished instances are removed.
Errors are cached for the same interval and explicitly reported, not replaced
with zero. `filesystem_report.sampled_at_unix_ms` and `sample_age_seconds`
describe the age of the actual filesystem measurement attempt. Cache is in
memory; a process restart samples everything again.

Snapshots are atomically renamed in the output directory, mode 0600, so readers
see a whole document. A directory lock prevents multiple daemons from writing
there simultaneously. The runtime directory is disposable and recreated on
boot. On a fatal collection/output error the process exits for systemd to
restart it. Per-file or per-VM errors stay in the report. Collection overruns
delay the next cycle; the daemon never overlaps its own cycles.

## What the measurements mean

Discovery recognizes `lxc/VMID`, top-level `lxc.payload.VMID`, and
`qemu.slice/VMID.scope`, preferring the outer LXC group over its descendants.
It does not include LXC monitor processes or double-count child cgroups. Only
running instances have cgroups; this is not a cluster-wide inventory of stopped
guests. Optional names come from the local `/etc/pve/lxc` and `qemu-server`
configuration. Unexpected layouts need inspection rather than guessed IDs.

Cgroup output includes CPU usage/user/system/throttling, quota and cpuset,
memory current/peak/limits/anon/cache/events/OOM/swap, full PSI, per-device I/O
byte/operation counters, PID counts and population. Counters are cumulative;
rates require successive samples and reset handling. Local limit values do not
account for a stricter ancestor. VM memory is host-side QEMU consumption, not
guest application RAM usage. Missing files are null with a read error.

LXC filesystem measurements use a nested namespace init PID found in the cgroup
subtree. `/proc/PID/mountinfo` supplies actual mounts, filesystem type and source.
`statvfs` reports total/used/free/available bytes and inodes for data directory
mounts, including `/` and an independently mounted `/home/app`. Kernel/runtime
mounts are excluded. Missing `/home/app` is explicitly `mounted: false`. A bind
mount reports backing filesystem capacity, not the size of the directory.
Guest paths are confined to a pinned root using `openat2` with `IN_ROOT` and
`NO_MAGICLINKS`. No guest executable or namespace entry is required.

QGA requests synchronize with a unique token, discard stale/parser-flush replies,
request `guest-get-fsinfo`, then promptly close the connection. Connections are
not held between cycles, to avoid monopolizing Proxmox's agent channel. At most
`min(qga_parallel, due_VM_count)` queries run concurrently. Local reads precede
the QGA phase; commands within one VM connection remain sequential. Results
retain VMID order. QGA response reads have a 3-second deadline per VM and a
4 MiB bound. QGA does not list network filesystems. Raw QGA entries are retained;
older agents without privileged capacity leave `total_bytes`/`free_bytes` null
and report their available capacity separately.

Debug diagnostics go to stderr: source paths, values, file byte counts, scalar
counts, errors and durations. Worker counters are merged into the summary.
`local_collection_usec` and `guest_agent_collection_usec` split per-instance
work; `collection_usec` on an instance sums those durations without queue wait.
Top-level `collection_usec` measures full wall time. Debug logging affects all
these times. Counts include metadata/protocol fields, not just numeric metrics.

Filesystem syscalls can block on unresponsive storage, especially hard NFS.
The prototype's reads are not isolated in timed workers. A snapshot consists of
sequential measurements, not an atomic kernel snapshot. No live installation,
reboot verification or production PVE benchmark is implied by local checks.

Upstream protocol and implementation references:

- [QEMU Guest Agent protocol](https://www.qemu.org/docs/master/interop/qemu-ga-ref.html)
- [PVE LXC filesystem collection](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC.pm)
- [PVE QGA connection handling](https://github.com/proxmox/qemu-server/blob/master/src/PVE/QMPClient.pm)
