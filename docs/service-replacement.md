# Registered service container replacement

Update prepares a new container on the original node using only the scoped
Proxmox API token. No SSH connection or command in a guest or on a PVE host is
part of replacement or its recovery. Legacy internal services keep their existing
handlers until separately adopted.

The operator reviews the selected release and digests, old/new CTIDs, node, pool,
all tags, network identity, root storage and the existing home volume. Nothing
changes during review. The new CTID is independent of the service IP address.
HomeStack labels the home with the manifest service ID and `/home/app`; the
technical PVE volume ID remains visible in details. PVE owns the backend name
and may change its CTID component during reassignment.
The container overview reads its IP from `net0` and home capacity from `mp0`.
Home free space comes from fresh in-container HTTP telemetry when the service
implements the [service status contract](../.agents/skills/homestack-service/service-status.md).
Otherwise it stays unknown: PVE's LXC `disk`/`maxdisk` counters describe the
root filesystem and must not be used as measurements of `/home/app`. Telemetry
does not replace the PVE ownership and volume checks below.

1. Pull the selected release into a new operation-specific OCI archive. PVE 9.2
   accepts tagged pull references; verify the registry's index and amd64 digests
   before and after the pull. Refuse changed tags. Do not reuse an unverified
   cached archive. This relies on the service's immutable release contract; it
   does not provide an independent checksum of the archive through PVE's API.
2. Create the replacement container stopped, without network or home and with
   autostart disabled. Preserve the hostname, pool, all tags, resources and
   application environment. Runtime values stay in PVE and are excluded from the
   local journal and logs.
3. Disable autostart on the original container, request graceful shutdown and
   wait. A failed shutdown does not trigger a forced stop. If selected, complete
   a stopped backup including home before proceeding.
4. Call `POST /nodes/{node}/lxc/{source_ctid}/move_volume` with `volume=mp0`,
   `target-vmid=<replacement_ctid>`, `target-volume=mp0` and both config
   digests. The API checks storage rename support and reassignment permissions.
   Refuse snapshots, drift, unexpected devices/mounts, HA management and
   occupied target slots.
5. Wait for the recorded task and verify both configurations: the original
   container lost its home, the replacement received it under `/home/app` with
   the same mount options and capacity, and both root volumes remain unchanged.
   Record the old/new volume IDs and task.
6. Remove the original container's network and assign its exact network
   configuration to the replacement. Restore the original power state. Check the
   manifest's health probe for a running service, then restore the original
   autostart setting on the replacement container.
7. Atomically replace the registry's active CTID/root/home while keeping
   service, node, hostname, IP, MAC, pool and tags. Keep the old CT/root and
   transfer evidence in the instance history. Local favorites and selection
   follow the new CTID.

The original container remains stopped without network, home or autostart.
No container, old root or data volume is automatically deleted. The replacement
is another generation of the same logical instance, including for a single-node
service.

Every transition and returned task ID is journaled under the profile operation
lock. Recovery requires a new review and verifies the recorded instances before
continuing. Missing responses, failed tasks and ambiguous storage state stop for
inspection; they never trigger another empty home allocation, guessed adoption
or disk cleanup. Completed nodes remain recorded when another node fails.

Reopen the existing image selector to resume an interrupted update. It offers
only the recorded release and requires confirmation before continuing with the
saved CTIDs and task IDs. Power-state decisions read the node's
`/nodes/{node}/lxc/{ctid}/status/current`, because cached cluster inventory can
omit the state of a newly created container.

API configuration and task evidence verify reassignment, not guest file contents
or filesystem UUIDs. Application health alone does not prove existing data was
read. File markers and application data inspection are additional verification.
Restoring an old image does not undo database migrations. A data-changing upgrade
requires a consistent backup; restoring that backup is a separate explicit action.

The manual pve3 smoke check used Tutor 0.1.2 on both generations: CT 102004 to
CT 102005 and back, followed by a separately authorized permanent switch to CT 102005.
PVE confirmed LVM rename and both applications answered `/alive` with HTTP 200.
The owner checked the content after the permanent switch. The production Tutor
on pve2 was not changed. This check is evidence for the API sequence, not a live
execution of the Rust implementation or a version/database migration test.
