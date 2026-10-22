# Container image updates

API servers, Pangolin connectors and deployed services update their image the
same way, through `hs_deploy::recreate` and the scoped Proxmox API token only.
The container keeps its CTID, so the CTID stays tied to its IP address.

- A container without mount-point volumes is stopped, destroyed and made again
  from the new image.
- A container with mount-point volumes (a service's `/home/app`) first gets a
  temporary holder container on the same node, always CT 999999 named
  `hs-holder-<ctid>`, so only one such update runs in the cluster at a time. The
  holder is created from `alpine:3.24` with a 1 GiB root while the service still
  runs, is never started and has no network. The configuration the new container keeps
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

After a successful update HomeStack removes the images it pulled and no longer
needs from the template storage: the service's `hs-<ctid>-*` archives, older
`homestack-server_*` and older `homestack-pangolin-site_*` images. The deploy
token's `HomeStackDeployTemplates` role on that storage includes
`Datastore.Allocate` for this. A failed removal is reported and never fails the
update.

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

## Deployed services

Proxmox alone records what is deployed: each service container's description
names the service (`HomeStack service <id>`), its repository, manifest revision,
image and digest, and the API's status reports it for every container. Nothing
about a deployment is kept on the desktop. Each deployment (`d`) makes new,
independent instances, as many as asked for on each chosen node and storage,
named `<id>`, `<id>-2`… with the first free CTIDs.

An image update is reviewed first: the selected release and digest, the CTID,
node, pool, network identity, root storage and the home volume. Nothing
changes during review. Then, for each container:

1. Pull the release into an archive named after the CTID and digest unless it
   is there. PVE 9.2 accepts tagged pull references; the registry's index and
   amd64 digests are verified before and after the pull and changed tags are
   refused.
2. If selected, back up the container including its home (`vzdump`, stop mode).
3. Replace the container as described above, with a graceful shutdown that never
   forces a stop. The holder's description also carries the service's identity
   and the container's settings (name, CPU, memory, root, pool, `net0`, power
   state), so the update can finish while the old container is gone. Verify the
   new container's description, home under the original mount options, and
   original network and MAC.
4. Start the service when it ran before or starts on boot, and check the
   manifest's health probe. Remove the pulled archive.

An interrupted update is finished by updating again: every step starts from
what Proxmox shows. While only the holder is left, the holder appears in the
list as the service, and Enter on it updates the container it holds.

Maintenance (`m`) grows a container's root or home through the Proxmox API, or
removes containers together with their home volumes after their names are typed.

PVE renames a volume when it moves, so the home's volume ID changes between
`vm-<ctid>-disk-N` names of the same container. The container overview reads its IP from `net0` and home capacity from
`mp0`. Home free space comes from fresh in-container HTTP telemetry when the
service implements the
[service status contract](../.agents/skills/homestack-service/service-status.md);
otherwise it stays unknown, because PVE's `disk`/`maxdisk` counters describe the
root filesystem.

API configuration and task evidence verify the volume moves, not guest file
contents. Application health alone does not prove existing data was read.
Restoring an old image does not undo database migrations; a data-changing
upgrade requires a consistent backup, and restoring it is a separate action.

## Moving a service container

Migrate (`g`) on a service container moves it to another node with
the deploy token (`VM.Migrate` in its `HomeStackDeploy` role): a restart
migration of root and home onto one storage of the target node. The form offers
only storages of the type the root is on now, since Proxmox moves volumes only
between storages of the same type. The CTID, address and MAC stay. A running
service starts again on the target node and its health check must pass. A move
made in Proxmox itself needs nothing from HomeStack: the container is where
Proxmox shows it.
