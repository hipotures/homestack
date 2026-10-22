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
  "load_average": {
    "scope": "system",
    "one": 0.04,
    "five": 0.02,
    "fifteen": 0.01
  },
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
- `load_average`: object as above, or `null` when unavailable. All three
  values must be finite, nonnegative numbers. `scope: system` explicitly
  means the kernel/procfs view and makes no claim of container isolation.
  Linux LXC may show host or virtualized load depending on procfs/LXCFS.
  Load average is not a percentage. Never substitute host boot time for
  process uptime or CPU percentage for load average.
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

Use native load APIs or bounded `/proc/loadavg`. Do not execute `df`, `mount`,
`uptime`, a shell or SSH from the endpoint. Rust can use `rustix::fs::statvfs`
plus the standard library; Python can use `os.statvfs`/`os.getloadavg`, and
other languages can use their maintained native OS bindings.

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
