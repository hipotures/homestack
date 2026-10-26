# Schema-3 service scenario

`service.schema3` in the existing E2E registry describes this isolated-lab
scenario for both one-node and three-node topologies. The automated service
TUI driver is still absent, as for the existing schema-1 manifest scenarios;
the registry reports BLOCKED, never PASSED. Execute the following through the
actual compiled TUI when the owner authorizes a disposable lab. This change
does not run the scenario or connect to production.

## Fixtures and observations

Use `.homestack/services/otel/` at one immutable commit, a published
`ghcr.io/hipotures/homestack-otel:X.Y.Z` image and its resolved digest, a
disposable Prometheus-compatible OTLP HTTP receiver, and a private DNS zone
with an owner-created shared round-robin record. Save the initial shared
record set, PVE inventory and configuration outside the repository. Never
record credentials or private input values in committed evidence.

1. Discover this repository. API and Pangolin schema-1 manifests remain
   separate services. The three OTel YAML files appear as exactly one service.
   Repeat with a missing member, a symlink, oversize file, unknown key,
   invalid type, conflicting service IDs and a schema-1 ID collision.
   Every invalid definition must refuse Review/Apply.
2. Select one node and one instance, then three instances. Repeat on three
   nodes, including a count greater than the selected node count. Verify
   operator resource/storage/network overrides and individual names `otel-1`,
   `otel-2`, `otel-3`. No instance may use the unnumbered shared name.
3. Leave the destination empty; enter malformed URLs and disallowed schemes.
   Try queue sizes 99, 100, 10000, 10001, fractional values and text. Verify
   field errors, required handling, typed integer storage and Review validation.
   Exercise an authoring fixture with string, number, boolean and enum inputs,
   regex and length constraints, duplicate IDs/targets and JSON Pointer leaves.
   Secret/password inputs must refuse unprotected PVE delivery.
4. Review and Apply valid choices. Verify the reviewed index/platform digests,
   OCI ENTRYPOINT/CMD, retained PATH, single-line PVE environment, nonroot
   UID/GID 10001:10001 and queue directory mode 0700. No `/home/app` volume,
   hook or privileged directory preparation may be created.
5. Verify OTLP HTTP and gRPC reception, a metrics-only pipeline and
   `GET /health/live` on 13133. No HomeStack telemetry endpoint is required.
   Pause the disposable receiver, ingest metrics and inspect file-backed
   queue growth. Restart the same CT: queued data remains and drains after
   restoring the receiver. The release image validation separately exercises
   an unreachable destination and a same-container restart using Docker.
6. Restart the desktop application. Verify its private owner-only selections
   are restored. Hide its local selections file: maintenance/update must
   explicitly report missing configuration rather than using public defaults.
   Reconcile a changed destination/queue size and verify the PVE environment
   and private file agree after the CT restart.
7. Review same-CTID image replacement. Its warning must state that the
   disposable rootfs queue is lost. Verify configuration and image command
   survive replacement, CTID/name/IP remain stable, and schema-1 replacements
   retain their existing behavior. Attempt a changed image tag between Review
   and Apply: replacement must refuse it before removing the old container.
8. Move an instance using the existing service move workflow. Verify its
   instance number and individual DNS identity do not acquire a node suffix.
   Compare the shared DNS A-record set byte-for-byte with its initial state
   after deployment, maintenance, replacement and removal of owned instances.

Clean up only scenario-owned guests, individual records and local fixtures
through the existing E2E isolation/cleanup protections. Report each observed
result and every unexecuted step. DNS round-robin does not establish HA or
automatic client failover.
