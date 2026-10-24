# HS-16: reviewed workspace VM destruction

The previous destroy path compared only the derived `HS_HOME_<VMID>` serial
after shutdown. A replacement home could retain that serial; root disks,
attachments and placement could also change after confirmation. QEMU DELETE
would then act on the current configuration rather than the reviewed resources.
The DELETE task also used lock retries without renewing destructive ownership.

## Review and validation

The destroy plan now contains a `destructive_identity` snapshot. The ordinary
confirmation workflow remains in place. Both CLI and TUI plan rendering list
the authorized pool, role, PVE digest and exact disk configurations, including
root, home, Cloud-Init, EFI and TPM volume IDs and any serials. Additional data
disks and external CD-ROMs require manual handling instead of expanding destroy
authorization.

Each complete identity read requires:

- The VMID is not the configured Gold. The VM has the native workspace role,
  no Gold/alien/refresh-lock role, and no unknown role tags.
- Unique cluster placement in the authorized pool, explicit QEMU membership
  in that pool on the same node, and presence in node-local QEMU inventory.
- A nonempty name and digest, full PVE configuration, exact distinct root/home
  mappings and volume IDs, and the expected home serial. Disk options and serials
  are retained exactly. Matching names, capacities or serials do not establish
  ownership.
- No lock, protection, template, hookscript, custom QEMU arguments, saved VM
  state, snapshot parent, pending changes, pending deletions or `unusedN` entry.
  Snapshot enumeration must contain no real snapshots; only the synthetic
  `current` entry is accepted. Its runtime metadata is not a snapshot identity.
- Known running/stopped power state, explicitly unmanaged HA, and no replication
  jobs. Failed reads or malformed relevant inventories abort review.
- Enabled, active storage on the actual node. All visible, enabled image stores
  applicable to that node are inspected, including stores outside the configured
  HomeStack layout. Every reviewed volume must appear with authoritative owner
  VMID equal to the target; ownership is not inferred from its filename.
- Storage backend configuration and all returned volume attributes except
  dynamic `used` bytes. Unexpected owned volumes, including detached volumes and
  visible fleecing leftovers, are rejected. Missing volumes or ownership are
  rejected.
- Placement, pool membership, node inventory, storage configuration, power/HA
  state and VM configuration are checked again at the end of the read.

Execution compares this entire identity with the reviewed snapshot before any
shutdown, then rereads and compares it immediately before DELETE. The final read
requires stopped state. Only this operation's running-to-stopped transition is
allowed; configuration digest, volume IDs, disk options, storage configuration
and volume attributes must remain identical. Redundant plan display fields must
also agree with the authorization snapshot. Old or incomplete plans require a
new review. No changed resource is silently adopted.

## QEMU DELETE semantics and recovery

The request is:

```text
DELETE /nodes/{reviewed-node}/qemu/{reviewed-vmid}
purge=0
destroy-unreferenced-disks=0
```

No force or skiplock option is sent. In the upstream
[QEMU DELETE endpoint](https://github.com/proxmox/qemu-server/blob/master/src/PVE/API2/Qemu.pm),
purge removes references from backup, replication and HA configuration. It is
unnecessary to delete the workspace and home. Disabling it preserves backup-job
references; HA and replication are rejected before shutdown. VM-specific ACL
and firewall removal remains part of QEMU DELETE.

The upstream
[volume deletion implementation](https://github.com/proxmox/qemu-server/blob/master/src/PVE/QemuServer.pm)
traverses current and pending volume references, unused disks and saved-state
volumes; Cloud-Init disks are included, whereas ordinary ISO media is skipped.
It checks storage-reported ownership before freeing volumes. Current upstream
also attempts fleecing-image cleanup. These semantics motivate rejecting
pending changes, snapshots, saved state, unknown attachments and unexpected
owned storage. Unreferenced-disk sweeping defaults to zero and is explicitly
disabled, so a matching VMID elsewhere is never authorization for broad cleanup.

Graceful shutdown is sent at most once, with the existing 90-second timeout and
no forced stop. A failed or ambiguous shutdown aborts. DELETE is also sent at
most once, including lock failures, unusable UPIDs, lost responses, failed task
polling and task lock errors. The task error directs the operator to inspect
the VM, storage and task result before reviewing another plan. There is no
automatic destructive recovery. The existing server plan store marks an applied
plan consumed even when its job fails; applying that plan again is refused.

The HTTP client's endpoint failover is limited to connection failures before a
request is sent. Sent requests with lost or unusable responses are not retried.
After a successful DELETE task, HomeStack checks VM absence and storage content
for absence of every reviewed volume. A disk left behind is an explicit recovery
error, rather than a false successful persistent-home deletion report.

## Focused verification

A temporary Rust diagnostic called the actual `plan_destroy` and
`destroy_workspace` functions with an in-memory `Pve` implementation. It counted
shutdown and DELETE requests and asserted exact DELETE parameters. It made no
network requests. Its source was removed from the repository after verification;
this task adds no permanent test suite.

```text
python3 scripts/cargo.py test --locked -p hs-core \
  --test hs16_diagnostics --test lifecycle -- --nocapture
Temporary diagnostic: 114 controlled checks passed
Existing lifecycle safety checks: 2 passed
```

| Controlled condition | Result |
| --- | --- |
| Unchanged stopped workspace | One DELETE; reviewed volumes confirmed absent |
| Unchanged running workspace | One graceful shutdown, then one DELETE |
| Home replaced while retaining `HS_HOME_240` | Zero DELETE before or after shutdown |
| Root replaced; root/home storage changed; disk mapping or serial changed | Zero DELETE before or after shutdown |
| Additional disk, `unusedN`, detached owned volume or visible fleecing volume | Zero DELETE |
| Node/pool changed; workspace role lost; Gold/alien/refresh-lock role | Zero DELETE |
| Digest, storage backend or volume metadata changed after shutdown | Zero DELETE |
| Pending changes/deletion, snapshots, lock, template, protection, saved state or hook | Zero DELETE; unsafe initial plans also refused |
| Missing digest/volume/owner, foreign owner, unknown state, malformed inventories | Zero DELETE |
| HA-managed/unknown HA state or replication | Zero DELETE |
| Lost DELETE response, API lock failure, invalid UPID, task lock error or lost task status | Exactly one DELETE; no retry |
| DELETE completes but disk remains | Explicit recovery error; no retry |
| Shutdown lock failure or VM still running | Zero DELETE; one shutdown attempt |
| Old plan without destructive identity | Zero DELETE |

Wrapped compilation produces `target/debug/homestack-server` and
`target/debug/homestack`. Production VMs, services and API instances were not
modified or restarted. Create, refresh, migration, SSH, hardware authentication
and service LXC removal semantics are unchanged.

## Proxmox limits

Inventories are filtered by existing token permissions. Missing reviewed
resources abort destruction, but the API cannot prove absence of unrelated
volumes on inaccessible or disabled storage. Such volumes are not swept. No
permissions are expanded by this change.

QEMU DELETE accepts neither a digest nor an exact-volume precondition. Its worker
acquires its config lock after HomeStack's last read and rereads configuration.
The sequence is therefore not atomic against an external privileged writer.
Proxmox also exposes no universal immutable incarnation ID for a VM or storage
volume recreated with every exposed attribute identical. This fix rejects every
observable identity/configuration change and never replays an ambiguous DELETE;
it cannot provide an absolute transactional guarantee against concurrent
administrators or an indistinguishable recreation. That stronger guarantee
requires a PVE-side conditional destroy operation under its own lock.
