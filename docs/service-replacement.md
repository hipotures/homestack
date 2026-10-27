# Container image updates

Application services and the canonical API and Pangolin Site manifests deploy
through the common service browser. The installer also retains its own API
bootstrap path. See [internal services](internal-services.md) for their current
initialization, credentials and ownership boundaries.

## Optional persistent home

The manifest decides whether a service has a home:

- Empty `persistence` with `state_model: stateless` creates only a disposable
  root filesystem. No home volume is allocated or mounted. The current
  `.homestack/homestack-api.yaml` uses this mode; its configuration, certificate
  and PVE token are supplied again from the desktop after replacement.
- Non-empty `persistence` attaches a separate Proxmox-managed `mp0` at
  `/home/app` before first start. A service with `state_model: local` requires
  it. All durable application files, including SQLite databases and their
  WAL/SHM files, must live there. The current Pangolin Site manifest uses this
  mode for its Site credential and data.

Replacement reassigns the original persistent home to the new root. It never
allocates a substitute home or initializes a new one for an existing service.
Explicit service deletion has different semantics, described below.

## Same-CTID replacement

API servers, Pangolin connectors and deployed services update their image the
same way, through `hs_deploy::recreate` and the scoped Proxmox API token only.
The container keeps its CTID, so the CTID stays tied to its IP address.

- A root-only container is stopped, destroyed and made again from the new
  image, without allocating or moving a home. Common service updates also use
  a holder to retain settings even when there are no mount-point volumes.
- When volumes or settings must be retained, replacement first creates a
  temporary holder container on the same node, always CT 999999 named
  `hs-holder-<ctid>`, so only one such update runs in the cluster at a time. The
  holder is created from `alpine:3.24` with a 1 GiB root while the service still
  runs, is never started and has no network. The configuration the new container keeps
  (environment, features, resource options) is copied into the holder; its
  description then ends with `holder ready`. The replacement journal is PVE
  metadata, not a desktop file.
- The old container is shut down, its volumes move to the holder
  (`move_volume`), the old container (now only its own root) is destroyed with
  `purge=0` and `destroy-unreferenced-disks=0`, the new container is created
  under the same CTID with the original `net0` including its MAC, the volumes
  move back with their mount options, the kept configuration is merged over the
  new image's own entries and the holder is removed.
- The common update review refuses extra mount points or networks, bind/network
  mounts, devices, unused volumes, hooks, unsupported custom LXC configuration,
  snapshots, custom firewall configuration, HA and replication jobs.

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
image and digest, and the API's status reports it for every container. There is
no desktop deployment registry; deployment configuration and Pangolin
credential/recovery files still live on the desktop. Each deployment (`d`) makes new, independent instances,
one on each chosen node, with the first free CTIDs unless explicitly selected.
Names use the reviewed hostname (defaulting to `deployment.hostname`, then the
manifest ID): a first single instance uses `<hostname>`, while multiple or
additional instances use free `<hostname>-1`, `<hostname>-2`… names.

An image update is reviewed first: the selected release and digest, the CTID,
node, pool, network identity, root storage and the home volume. Nothing
changes during review. Then, for each container:

1. Pull the release into an archive named after the CTID and digest unless it
   is there. PVE 9.2 accepts tagged pull references; the registry's index and
   amd64 digests are verified before and after the pull and changed tags are
   refused.
2. Replace the container as described above, with a graceful shutdown that never
   forces a stop. The holder's description also carries the service's identity
   and the container's settings (name, CPU, memory, root, pool, `net0`, power
   state), so the update can finish while the old container is gone. Verify the
   new container's description, home under the original mount options, and
   original network and MAC.
3. Start the service when it ran before or starts on boot, and check the
   manifest's health probe. A stateless API is bootstrapped again with the
   desktop's configuration, certificate and token and authenticated HTTPS
   verification; an originally stopped API without autostart is stopped again
   afterward. Restore the reviewed autostart setting and remove the pulled
   archive after success. A deliberate reinstall of the same release also
   replaces the root; PVE operation metadata distinguishes it from a resume.

The current interface does not select a pre-update `vzdump` backup. See
[Proxmox permissions](proxmox-permissions.md) for the deploy role requirements,
template-storage deletion authority and permission repair procedure.

Pangolin Site image replacement follows this same path: its credential and
data stay on the original home volume, and its HTTP readiness endpoint checks
the tunnel. No SSH connection, Pangolin API operation or desktop Site journal
is needed to replace the root image.

An interrupted update is finished by updating again: every step starts from
what Proxmox shows. While only the holder is left, the holder appears in the
list as the service, and Enter on it updates the container it holds.

## Explicit service deletion

Maintenance (`m`) can grow an existing root or home and change resources or
editable environment settings. To remove selected instances, the operator must
type their container names. Review lists each CTID, node, address, root and
home volume (when present). Apply stops running containers and deletes them
with `purge=1` and `destroy-unreferenced-disks=1`: both root and attached home
are deliberately removed. There is no separate retain-home uninstall action
in this interface. Image replacement uses the root-only deletion described
above instead.

For a managed Pangolin Site, removal first verifies and deletes its Pangolin
Site and local credential/recovery files, then removes the container and home.
It requires the verified Pangolin context. See [internal services](internal-services.md).
`homestack uninstall` removes HomeStack infrastructure settings and credentials
but never removes a VM, container or volume. Standalone
`homestack-deploy remove --only NAME --yes` is a separate explicit container
removal command for its configured instances.

## Observing home preservation

PVE renames a volume when it moves, so the home's volume ID changes between
`vm-<ctid>-disk-N` names as ownership moves through the holder and back. The
container overview reads its IP from `net0` and home capacity from `mp0`.
Home free space comes from the PVE agent's fresh `/home/app` measurement in
Prometheus; otherwise it stays unknown, because PVE's `disk`/`maxdisk`
counters describe the root filesystem.

API configuration and task evidence verify the volume moves, not guest file
contents. Application health alone does not prove existing data was read.
Restoring an old image does not undo database migrations; a data-changing
upgrade requires a consistent backup, and restoring it is a separate action.

## Moving a service container

Migrate (`g`) on a service container moves it to another node with
the deploy token (`VM.Migrate` in its `HomeStackDeploy` role, disk configuration
on the container and allocation on the selected target storage): a restart
migration of root and home onto one storage of the target node. The form offers
only storages of the type the root is on now, since Proxmox moves volumes only
between storages of the same type. The CTID, address and MAC stay. A running
service starts again on the target node and its health check must pass. A move
made in Proxmox itself needs nothing from HomeStack: the container is where
Proxmox shows it.
