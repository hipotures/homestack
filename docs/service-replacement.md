# Container image updates

API servers, Pangolin connectors and registered services update their image the
same way, through `hs_deploy::recreate` and the scoped Proxmox API token only.
The container keeps its CTID, so the CTID stays tied to its IP address.

- A container without mount-point volumes is stopped, destroyed and made again
  from the new image.
- A container with mount-point volumes (a service's `/home/app`) first gets a
  temporary holder container on the same node, named `hs-holder-<ctid>`. The
  holder is created from the same image while the service still runs, is never
  started and has no network. The configuration the new container keeps
  (environment, features, resource options) is copied into the holder; its
  description then ends with `holder ready`. Runtime values therefore stay in
  PVE and never reach the local journal or logs.
- The old container is shut down, its volumes move to the holder
  (`move_volume`), the old container (now only its own root) is destroyed with
  `purge=0` and `destroy-unreferenced-disks=0`, the new container is created
  under the same CTID with the original `net0` including its MAC, the volumes
  move back with their mount options, the kept configuration is merged over the
  new image's own entries and the holder is removed.
- Bind mounts, network mounts, devices and unused volumes are refused; they are
  not supported yet.

Every run continues from what Proxmox shows: the CT and its description (the
new container's description identifies this update), the holder and whether it
is ready, and where the volumes are. A locked container stops the run until its
task ends. An interrupted update therefore resumes by reopening Update image.
At no point does a volume belong to no container, and a container is destroyed
only when it holds nothing but its own root.

Measured on pve3 (LVM-thin, 2026-10-22): creating the holder takes about 1 s and
removing it 0.3 s, both outside the downtime. Moving a 1 GiB home takes about
0.5 s. Shutdown, move out, destroy, create from a 60 MB service image, move back
and start took 5.5 s, before the application's own start-up and health check.

## Registered services

The operator reviews the selected release and digest, the CTID, node, pool, tags,
network identity, root storage and the home volume. Nothing changes during
review. The service journal (`services.toml`, profile operation lock) records
each step and task:

1. Pull the release into an operation-specific OCI archive. PVE 9.2 accepts
   tagged pull references; the registry's index and amd64 digests are verified
   before and after the pull and changed tags are refused.
2. If selected, back up the container including its home (`vzdump`, stop mode).
3. Replace the container as described above, with a graceful shutdown that never
   forces a stop. Verify the new root, the home under the original mount
   options and the original network and MAC.
4. Restore the tags, then verify ownership, IP, MAC and volumes against the
   registry.
5. Restore the original power state, check the manifest's health probe for a
   running service, then restore the original autostart setting.
6. Record the new root and home volume IDs and the release in the registry.

PVE renames a volume when it moves, so the home's volume ID changes; HomeStack
labels the home with the manifest service ID and `/home/app` and records the new
ID. The container overview reads its IP from `net0` and home capacity from
`mp0`. Home free space comes from fresh in-container HTTP telemetry when the
service implements the
[service status contract](../.agents/skills/homestack-service/service-status.md);
otherwise it stays unknown, because PVE's `disk`/`maxdisk` counters describe the
root filesystem.

API configuration and task evidence verify the volume moves, not guest file
contents. Application health alone does not prove existing data was read.
Restoring an old image does not undo database migrations; a data-changing
upgrade requires a consistent backup, and restoring it is a separate action.
