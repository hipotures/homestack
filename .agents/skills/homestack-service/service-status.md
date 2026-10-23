# HomeStack service telemetry, version 1

This is HomeStack's application-side wire contract, independent of language
or web framework. It does not extend manifest schema 1. HTTP services serve
`GET /.well-known/homestack/status` on the same HTTP port referenced by
`runtime.healthcheck`. Services without HTTP have no telemetry endpoint;
their measurements remain unavailable. No arbitrary URL or extra listener
is inferred from a TCP healthcheck or port name.

## HTTP and JSON

Return `200`, `Content-Type: application/json`, `Cache-Control: no-store`.
The endpoint is read-only and requires no deployment credentials. It reports
process liveness, not dependency readiness; `/health/ready` remains the
application readiness authority. A live process can return `status: pass`
while disk measurements fail. `fail` is available when the application can
answer but considers its process unhealthy. Measurements must never mutate
files, create directories, initialize databases or read application records.

This is a private management-network endpoint, not authorization to expose
telemetry publicly. Keep it on the existing listener and prevent public
proxy routing to this path when public application exposure is configured.
Never return credentials, environment values, file contents, volume/device
names, NFS server addresses, stack traces or detailed OS error messages.

Example (illustrative values):

```json
{
  "schema": 1,
  "status": "pass",
  "uptime_seconds": 1760,
  "sample_age_seconds": 2,
  "load_average": null,
  "filesystems": [
    {
      "mount": "/",
      "filesystem": "ext4",
      "mounted": true,
      "total_bytes": 4143677440,
      "used_bytes": 219787264,
      "free_bytes": 3923890176,
      "available_bytes": 3692367872,
      "error": null
    },
    {
      "mount": "/home/app",
      "filesystem": "ext4",
      "mounted": true,
      "total_bytes": 8350298112,
      "used_bytes": 2449408,
      "free_bytes": 8347848704,
      "available_bytes": 7901577216,
      "error": null
    }
  ],
  "container": {
    "cpu": {
      "capacity_cpus": 2.0,
      "interval_seconds": 5.001,
      "usage_percent": 0.08,
      "capacity_percent": 0.04,
      "user_percent": 0.05,
      "system_percent": 0.03,
      "throttling": { "periods": 0, "throttled_periods": 0, "throttled_seconds": 0.0 }
    },
    "memory": {
      "usage_bytes": 120451072,
      "limit_bytes": null,
      "anon_bytes": 40960000,
      "file_bytes": 70000000,
      "process_rss_bytes": 61640704,
      "events": { "high": 0, "max": 0, "oom": 0, "oom_kill": 0 }
    },
    "pressure": {
      "cpu": {
        "some": { "avg10": 0.0, "avg60": 0.0, "avg300": 0.0, "total_usec": 123 },
        "full": { "avg10": 0.0, "avg60": 0.0, "avg300": 0.0, "total_usec": 45 }
      },
      "memory": null,
      "io": null
    },
    "io": {
      "interval_seconds": 5.001,
      "read_bytes_per_second": 0.0,
      "write_bytes_per_second": 819.2,
      "read_ops_per_second": 0.0,
      "write_ops_per_second": 0.2
    },
    "error": null
  },
  "error": null
}
```

- `schema`: integer `1`. Consumers ignore additional fields but reject an
  unsupported version or malformed fields. This is separate from YAML schema.
- `status`: `pass` or `fail`, supplied by the application.
- `uptime_seconds`: nonnegative integer seconds since this application
  process started, using a monotonic clock. Not host uptime or CT lifetime.
  Initialize the sampler once during process startup; never once per request.
- `sample_age_seconds`: nonnegative integer age of the oldest measurement,
  measured from the beginning of collection with a monotonic clock; `null`
  before the first completed sample. Consumers discard measurements older
  than 30 seconds. Never reset the age just because an HTTP response is sent.
- `load_average`: always `null`. Without LXCFS load virtualization,
  `/proc/loadavg` in a Proxmox LXC container is the host's load, and an idle
  container then reports the host's numbers. The field stays for schema 1
  compatibility; container load is reported in `container`. Never substitute
  host boot time for process uptime.
- `container`: resource usage of the cgroup v2 the service process runs in,
  described under "Container measurements". It is an object even when
  unavailable: then all its members are `null` and `error` is
  `cgroup_unavailable` (no usable cgroup v2) or `host_cgroup` (the process is
  in the real hierarchy root, whose counters cover the whole host).
- `filesystems`: array of unique absolute mount paths, at most 128 entries.
  Include `/` and `/home/app`, then other mounted data directories. Do not
  recursively enumerate directory contents. Exclude technical filesystems
  and file bind mounts such as `/etc/hosts`; do not filter for ext4 alone.
  Include mounted data types such as ext4, XFS, Btrfs, ZFS, NFS/NFS4, CIFS or
  FUSE where the OS supplies measurements. Do not sum these entries: mounts
  can share the same backing filesystem or storage pool.
- Each entry includes the filesystem type, `mounted`, four nullable integer
  byte counts and `error`. A successful measurement has `mounted: true`,
  non-null counts and `error: null`. Use OS values without rounding to GiB.
- Missing `/home/app`: emit its entry with `mounted: false`, null type and
  byte counts, and `error: not_mounted`. Never measure its containing root
  filesystem and label those numbers as a separate persistent home.
- An unreadable mount retains its path/type and `mounted: true`, with null
  byte counts and `error: measurement_unavailable`. Other stable codes are
  `invalid_statistics`, and top-level `pending`,
  `mount_inventory_unavailable` or `too_many_mounts`. With unavailable mount
  inventory, report required paths as unconfirmed (`mounted: false`) with
  `error: mount_inventory_unavailable`. Never turn unavailable into zero.

## Native measurements

On Linux read bounded `/proc/self/mountinfo` in the service's own mount
namespace. Decode its escaped paths and deduplicate visible mount points.
Exclude `/proc`, `/sys`, `/dev`, `/run` and their descendants, and pseudo
filesystems (including tmpfs); always retain exact `/` and `/home/app` mounts
regardless of type so an unexpected home filesystem remains visible. Retain
an unreadable data mount as a failed measurement instead of dropping it.
Confirm `/home/app` is an exact mount, not merely an existing directory.

Use `statvfs` (or the language's equivalent) for each data mount:

```text
total_bytes     = f_blocks * f_frsize
free_bytes      = f_bfree  * f_frsize
available_bytes = f_bavail * f_frsize
used_bytes      = (f_blocks - f_bfree) * f_frsize
```

Check arithmetic overflow and require `available <= free <= total`.
Available space excludes blocks reserved from an unprivileged user; it is
HomeStack's FREE column. Thus `total - used` need not equal available.
For quotas, shared Btrfs/ZFS pools, NFS and thin provisioning, these are the
filesystem's reported values, not a promise of dedicated backing capacity.
Container root still reports `f_bavail` for consistent semantics.

Do not execute `df`, `mount`, `uptime`, a shell or SSH from the endpoint. Rust can use `rustix::fs::statvfs`
plus the standard library; Python can use `os.statvfs`, and
other languages can use their maintained native OS bindings.

## Container measurements

Find the process's cgroup from the `0::` line of `/proc/self/cgroup` and a
`cgroup2` mount in `/proc/self/mountinfo`: the cgroup directory is the mount
point plus the cgroup path relative to the mount's root. Both are relative to
the cgroup namespace, so in a Proxmox LXC container the container's own cgroup
appears as `/`. Reject paths outside the mount root and `..` components.
Refuse a directory without `cgroup.type`: only the real hierarchy root lacks
it. Resolve once and again only when the directory stops answering.

- `cpu`: from `cpu.stat`. `usage_percent` (100 = one logical CPU busy),
  `user_percent` and `system_percent` are deltas of `usage_usec`,
  `user_usec` and `system_usec` over the monotonic time between two samples;
  `capacity_percent` divides by `capacity_cpus`. Capacity is the size of
  `cpuset.cpus.effective` (otherwise the process's available parallelism),
  lowered by the smallest `cpu.max` quota on the cgroup or a visible ancestor.
  `throttling` holds the deltas of `nr_periods`, `nr_throttled` and
  `throttled_usec` over the interval, `null` without the cpu controller.
  Interval fields are `null` on the first sample, after a counter decreases,
  and for intervals under 0.5 s (the older counters are then kept). Never
  infer CPU usage from load average.
- `memory`: `null` without `memory.current`. `usage_bytes` is
  `memory.current` (page cache included); `limit_bytes` the smallest numeric
  `memory.max` on the cgroup or a visible ancestor, `null` for `max`;
  `anon_bytes` and `file_bytes` from `memory.stat`; `process_rss_bytes` the
  service process's own `VmRSS`; `events` the cumulative `high`, `max`, `oom`
  and `oom_kill` from `memory.events`.
- `pressure`: `cpu`, `memory` and `io` from `<resource>.pressure`, each with
  `some` and, where the kernel reports it, `full`: `avg10`, `avg60`, `avg300`
  (percent of wall time stalled) and cumulative `total_usec`. PSI measures
  stalls caused by contention, not utilization; never combine it with CPU
  usage into a load figure. A missing or unreadable file is `null`, not zero.
- `io`: rates from `io.stat` deltas summed over devices (device numbers are
  not reported), `null` without the io controller or before two samples.
  Filesystem capacity is not I/O activity.

Limits a container manager sets above the cgroup namespace (Proxmox sets
`memory.max` and `cpu.max` on the parent of the container's namespace root)
are invisible from inside: `limit_bytes` is then `null`, and `capacity_cpus`
reflects only the cpuset. Read only these interface files; never write to the
cgroup, run commands or substitute host-wide values when one is unavailable.

Collect in one bounded background worker, at most once every five seconds,
and return cached measurements immediately. A hung NFS/FUSE call must not
block the HTTP handler, create more workers per request, or make old data
appear fresh. Keep the response at most 64 KiB. Inventory/parse failures
are telemetry failures, not application readiness failures.

## HomeStack consumption

New common-managed deployments record `status-port <number>` in non-secret
PVE ownership metadata from the manifest's HTTP healthcheck port. The fixed
path is not a YAML field. Read from the actual configured container IP, not
an address calculated from CTID. No probing every port, redirects, proxies,
credential forwarding or TLS-verification bypasses. Old records without
that metadata remain unavailable until an explicitly approved deployment
operation; discovery does not rewrite their configurations.

HomeStack bounds timeout, response size and mount count, checks version,
types, freshness, unique paths and arithmetic, and uses `/home/app` only if
PVE also has `mp0` mounted there. It exposes the report as `service_status`
in its authenticated `/v1/status` response. A missing, unsupported or stale
endpoint leaves FREE unknown. The root filesystem and other mounts remain
available in the JSON report. These self-reported measurements do not
verify volume UUID, expected storage, data persistence, or ownership;
replacement still reconciles the actual Proxmox configuration and task.
