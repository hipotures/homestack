# To do

## Next priority: Persistent home for application LXC containers

Extend HomeStack's disposable-root / persistent-home model from workspace VMs to **stateful** application LXC containers. Persistent home must be **optional per application**: stateless services need only a disposable root filesystem and must not receive an unnecessary home volume. Start with the first stateful application, then make the behavior reusable.

**Invariant:** Replacing an application container replaces its entire root filesystem (the OCI application image). When persistent home is enabled, replacement must retain and reattach the **same persistent home**; "preserve home" must mean the same thing for workspace VMs and stateful application LXCs. When disabled, replacement is fully disposable.

### Requirements

- Support an explicit, per-application home policy (for example `none` or `persistent`); the default for stateless applications should not allocate or mount any home volume. Do not infer persistence from the image or create home during deployment unless the application explicitly opts in.
- When home is enabled, give the application LXC a dedicated persistent **home volume**, separate from the disposable LXC root filesystem. Treat it as part of the application's stable identity, not as part of an image or deployment.
- When enabled, expose this volume at a consistent path under `/home` (proposed default: `/home/app`). The final service account (`app` or `root`) and permissions can be decided during implementation; the home concept must not depend on which account runs the service.
- For stateful applications, keep **all durable application state** in home: SQLite databases (including WAL/SHM files), application configuration, uploads, persistent logs, and any other files that must survive an update. Do not use `/var` or the disposable root for durable application data. Application images/configuration must direct stateful paths into home.
- For stateful applications, have Proxmox attach the same home volume to each replacement container before the application starts (prefer a Proxmox-managed LXC mount point rather than guest `fstab` or startup hooks). The application must never start with a missing, empty, wrong, or accidentally newly created home when an existing one is expected.
- Keep CTID, IP, and MAC stable across deployment/recreation, as with the existing HomeStack server replacement flow. An update should not require DNS or firewall changes.
- For an application with persistent home enabled, a normal deploy, image update, restart, or container root replacement **must never delete, format, overwrite, or reinitialize home**. A failed or interrupted deployment must leave the persistent data intact and recoverably associated with its application.
- For stateful applications, deleting the persistent home must be a **separate, explicit, deliberate operation**, with verification of application/CTID, PVE node, storage, volume identity, and a clear data-loss confirmation. Removing an LXC or its root must not implicitly remove its home.
- Respect per-node storage availability and unprivileged-LXC UID/GID ownership mapping. Avoid assumptions that all nodes expose the same storage.

### Implementation safety

The current `hs-deploy` flow is specialized for disposable HomeStack API server containers: it stops and destroys the old LXC, including its volumes (`purge=1`, `destroy-unreferenced-disks=1`), then creates a new one. **Do not reuse that deletion behavior unchanged for stateful application LXCs.** Stateless application LXCs may continue to use disposable root-only replacement without a home volume. Implement a verified preserve/detach-and-reattach path (or an equivalent safe storage design) that guarantees the home volume is excluded from all destructive cleanup.

Keep existing HomeStack API server replacement behavior unchanged unless explicitly required. Do not repurpose workspace VM lifecycle semantics without accounting for Proxmox LXC mount-point and volume handling.

### Acceptance criteria

- Create a stateless service LXC from an OCI image without allocating a persistent home; redeploy it and verify that the root is replaced without home-related mounts or cleanup actions.
- Create a stateful service LXC from an OCI image, initialize its persistent home once, and mount it at the same path on every start.
- Write sample SQLite data, configuration, and a log into home; deploy a new root/image with the same CTID, IP, and MAC; verify the files and SQLite contents remain unchanged and usable.
- Repeat replacement, including an interrupted/failed deployment and a subsequent retry; confirm that no operation silently replaces or deletes home.
- Exercise stop/start and full LXC removal/recreation while retaining the original home; check that incorrect or missing home volumes fail safely before application startup.
- Add regression tests for both home modes, volume identity, mount verification, ownership, storage constraints, and protection from destructive Proxmox volume removal; document the new service-home contract and deployment behavior.
