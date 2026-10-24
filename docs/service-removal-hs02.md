# HS-02: reviewed service LXC removal

The previous service uninstall path reread a container on its old node without
pool information, compared only MAC and home volume, and then requested both
purging and unreferenced-disk destruction. A different root, service identity,
mount configuration or additional storage could therefore be deleted under an
old review. Pangolin removal also ran before a complete LXC safety check.

## Authorization and checks

Removal now captures a separate, private authorization snapshot. Ordinary
maintenance and replacement inventory retain their existing behavior.
The operator still selects instances, reviews the operation and confirms the
selected container names. The review lists the exact root and home volume IDs,
storages, sizes and home mount configuration, or explicitly states that no home
exists.

Before any selected Site or LXC is changed, all removal targets are revalidated.
Each target is revalidated again before its own Site removal, after Site removal
before stopping, and after stopping immediately before DELETE. Stopped targets
also receive the final revalidation.

Each complete read checks:

- A unique LXC CTID in cluster inventory, the configured authorized service pool,
  explicit membership in that pool on the same node, and presence in the node's
  own LXC inventory. Missing or conflicting placement is rejected. A visible
  replacement holder, including a holder found only in the node inventory,
  blocks removal.
- A complete configuration object with PVE digest and hostname; unprivileged,
  unlocked, unprotected and non-template state.
- Exactly one nonempty service ID, repository, image and image digest, and one
  managed ownership marker. Holder and unfinished replacement metadata are
  rejected. The reviewed service ID and repository must match the selected
  source before apply can make changes.
- Only rootfs and optional mp0 as storage attachments; mp0 must mount at
  `/home/app`. Bind mounts, extra mount points, devices, unused entries, hooks,
  custom low-level LXC configuration and extra networks are refused. The small
  set of OCI runtime LXC settings already recognized by image update is allowed.
- No pending settings or pending deletion, no snapshots beyond PVE's synthetic
  `current` entry, known unmanaged HA state, and no replication jobs.
- Storage configuration plus rootdir content for every visible, enabled storage
  applicable to the node, with enabled/active status required before listing.
  Root and optional home must be distinct exact volume
  IDs with authoritative owner VMID equal to the selected CTID. The storage
  inventory, rather than volume names or capacities, establishes ownership.
  Missing ownership, missing volumes, malformed responses and failed reads are
  refused. Additional volumes owned by this CTID are refused, including detached
  volumes outside the container configuration.
- Placement a second time, unchanged power/HA state during the read, and an
  identical final configuration read.

Apply compares the complete configuration and its digest, node, complete visible
storage configuration and volume attributes with the reviewed snapshot. This
covers hostname, service metadata, root identity/options/storage, home identity/
options/storage or explicit absence, environment and network identity. Volume
`used` bytes are excluded because they change during normal operation; all other
returned volume attributes, including any creation time, size, parent and format,
must match. A configuration difference reports the changed field and requires a
fresh review. The plan never adopts a new root or home automatically.

Before shutdown, the reviewed power state must match. Only this operation's
running-to-stopped transition is allowed at the final check; configuration and
volume comparisons remain identical. A running guest is stopped using the
existing stop task, whose success is awaited. DELETE requires stopped state.

## DELETE scope and Proxmox limits

The request is:

```text
DELETE /nodes/{reviewed-node}/lxc/{reviewed-ctid}
purge=0
destroy-unreferenced-disks=0
```

No force or skiplock option is sent. Proxmox still deletes the referenced root
and home and removes the guest's ACL/firewall entries. Purge is unnecessary for
that operation; disabling it avoids changing backup, replication and HA
configuration as an incidental uninstall step. Existing backup-job references
are not purged. HA and replication are refused before Site or LXC changes.

Proxmox's destroy implementation traverses current and pending volume references,
including unused entries, even with sweeping disabled. This is why unused
entries, pending changes and snapshots are blockers, rather than permission to
clean them up. Unreferenced-disk sweeping would scan enabled storages for disks
with the CTID, which exceeds the operator's authorization. It is explicitly off.
See the upstream [DELETE implementation](https://github.com/proxmox/pve-container/blob/master/src/PVE/API2/LXC.pm)
and [volume deletion implementation](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC.pm).

PVE inventories are filtered by the token's existing permissions. The guard
requires the exact destructive volumes to be visible with matching ownership and
rejects incomplete relevant responses; it cannot certify that no unrelated disk
exists on inaccessible or disabled storage. Such disks are outside the DELETE
scope and are never swept. No permissions or credentials are changed.

PVE DELETE has no configuration digest or exact-volume precondition. The last
config read is followed directly by comparison and DELETE, but the read sequence
is not a transaction. Another writer can still change configuration after the
last read or before PVE's worker acquires its lock. Also, the API does not expose
a universal immutable container/storage incarnation identifier: an object
recreated with every exposed identity and attribute identical cannot be
reliably distinguished. Matching CTID/MAC/home alone never authorizes removal;
any observable replacement, changed volume identity/attributes or unresolved
replacement state invalidates the review. This change does not claim atomicity
or an absolute guarantee against concurrent privileged writers.

## Pangolin ordering and recovery

Read-only Site removal validation follows successful LXC revalidation. Existing
verified organization, journal, credential and owned-Site checks remain in use;
completed-Site readiness is not substituted for removal validation, so recovery
with an already absent Site or journal remains possible.

The verified Site is removed before the LXC as before. A Site validation/removal
failure leaves the LXC intact. LXC identity is checked again after the Pangolin
operation, before shutdown and again before DELETE. If an external writer changes
the LXC during Pangolin removal, the LXC is retained and a fresh review is
required. A Site already removed is not rolled back. This preserves repeatable
recovery without a distributed transaction claim.

## Focused non-destructive verification

A temporary Rust diagnostic used the real `lifecycle::plan` and `lifecycle::apply`
with an in-memory implementation of `Pve` and a fake `SiteReady`. It issued no
network requests. Volume IDs deliberately did not contain the CTID, checking that
ownership did not depend on naming conventions. The diagnostic recorded every
PVE DELETE path/argument and shutdown count.

```text
python3 scripts/cargo.py test --locked -p hs-cli --lib hs02_ -- --nocapture
2 passed; 0 failed
```

| Controlled case | Observed result |
| --- | --- |
| Unchanged running/stopped, with/without mp0 (4 combinations); normal OCI runtime settings and changing used bytes | One DELETE each; exact reviewed node/CTID; both flags `0`; stop only when running |
| Changed root or home, with corresponding ownership inventory updated | Zero DELETE; rootfs/mp0 configuration change |
| Changed home options or removed home | Zero DELETE |
| Changed repository, service, ownership marker or ambiguous identity metadata | Zero DELETE |
| Extra mount, unused volume, device, low-level LXC mount or hook | Zero DELETE |
| Snapshot or detached volume appears | Zero DELETE |
| Different node or pool | Zero DELETE |
| Pending setting/deletion or malformed pending/snapshot/storage/content inventory | Zero DELETE |
| Missing/wrong volume owner, changed volume creation time/storage backend, inactive storage or missing storage status | Zero DELETE |
| Changed hostname/network, holder, replacement metadata, lock, HA or replication | Zero DELETE |
| Unexpected power change | Zero DELETE |
| Stop succeeds, then root identity and ownership inventory change | One stop; zero DELETE |
| Stale LXC before Site removal | Zero Site calls, zero PVE DELETE |
| Site removal validation fails | Read-only Site call only; zero stop/DELETE |
| Verified Site removal fails | Zero stop/DELETE |
| LXC changes during Site removal | Zero stop/DELETE; Site operation already ran |
| Verified Site removal or already-absent recovery response | One PVE DELETE each |

The diagnostic exercised 38 rejected LXC cases and six Site scenarios (four
rejected, two accepted), in addition to the four accepted power/home combinations.
Temporary diagnostic source and registration were removed from the repository;
no permanent tests or QA infrastructure were added. Full live E2E and production
destructive testing were not run.

The affected binary is built with:

```text
python3 scripts/cargo.py build --locked -p hs-cli --bin homestack
```

Only CLI service removal, its review integration and documentation changed.
The running API and Pangolin images are unaffected; no image release or deployment
is required.
