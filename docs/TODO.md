# To do

Optional persistent home for application LXC services is implemented. A manifest
with empty `persistence` deploys a root-only stateless service; a manifest with
persistence gets a separate `/home/app` volume. Same-CTID image replacement
reassigns that original volume through a temporary holder and can resume after
interruption. See [service replacement](service-replacement.md) for the current
contract, including explicit removal of a container together with its home.
The core deployment and replacement work in the former checklist is complete;
remaining verification and gaps are listed below.

## Remaining verification

- Complete a separately authorized isolated live verification of common Pangolin
  Site deployment, interrupted initialization/resume, image replacement and
  removal. The recorded console-transfer check does not prove a real Site's
  creation or online transition. See [internal services](internal-services.md#remaining-isolated-live-verification).
- Verify application-specific data after stateful upgrades. PVE mount metadata
  and a passing health check alone do not prove SQLite records, configuration
  or uploads survived and were read. The [2026-10-23 API verification](managed-api-live-verification-2026-10-23.md)
  is dated evidence for the tested persistent API instance, not every service.

## Current limitations

Bind/network mounts, extra disks/networks, devices, hooks, snapshots, custom
firewall configuration, HA and replication need separate review; the common
image-update path refuses unsupported configurations. Supporting them requires
separate work, not bypassing the current checks. No automatic data rollback is
provided by service maintenance.

The former checklist's separate retain-home uninstall/data-deletion operation
remains unimplemented. Current explicit removal deletes the container and home
together; changing that contract requires a separate decision and implementation.
