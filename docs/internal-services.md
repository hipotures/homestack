# Internal service manifests: future installations only

Phase 5 adds two portable schema-1 definitions to this repository. Discovery
shows HomeStack API and Pangolin Site independently of the number of PVE nodes.
This does not adopt or authorize changes to any existing internal container.

## Implementation map and compatibility inventory

| Reusable mechanism | Integration for a new managed instance | Remaining gap |
| --- | --- | --- |
| `services/github.rs::discover`, `manifest.rs::parse` | `.homestack/homestack-api.yaml` and `pangolin-site.yaml` | None for discovery; image versions still come from numeric OCI releases |
| `services/deploy.rs::{plan,run}` | Scoped deploy token, chosen CTID/address/resources, separate `mp0` before start, common `description` identity | Authenticated internal initialization is unavailable in this flow; review and apply both refuse before mutation |
| `hs_deploy::deploy::{apply,instances_client}` | Existing API certificate, bootstrap environment, token/config upload and validated HTTPS verification | Bootstrap is embedded in legacy replacement; it cannot be invoked as an initializer for a generic-created CT without replacing its ownership metadata |
| `tui/pangolin_sites.rs::{api_key,node_plan,deploy_node,create_site,health}` | Step 7 key, per-node private credential/recovery journal, secure stdin installation and Site online verification | The generic worker has no live Step 5 tunnel, verified Step 7 context or authenticated root session; Step 8 also creates its own legacy CT identity |
| `services/instance.rs`, `hs_core::registry::deployed_service` | PVE description is the only common instance identity | No desktop registry or adoption operation is added |
| `services/update.rs`, `hs_deploy::recreate` | Sequential replacement preserving CTID/network and the original home volume; environment retained | Internal readiness hooks must be connected before advertising complete internal lifecycle verification |
| `services/{migrate,lifecycle}.rs`, `tui/service_lifecycle.rs` | Existing migration, resizing and explicit maintenance/remove flows | API HTTPS and Site online checks must accompany disruptive internal operations; TCP/no probe cannot establish readiness |

The inspected upstream is `8ff94cc`, two commits after the task baseline
`a8d7c7f`: TUI dimming and batched status reads. Neither changes initialization,
manifest discovery, state paths or image replacement. Phase 5 preserves both.
During implementation, concurrent commits `50a2f31` and `df9a414` advanced the
workspace. The latter included the tracked Phase 5 runtime, installation guards
and API ownership edits together with unrelated TUI update polling; this task
preserves that history and commits only its remaining files.

### API runtime

`deploy/lxc/service/homestack/run` binds the API to loopback port 8080;
`deploy/lxc/service/caddy/run` serves certificate-validated TLS on port 443.
`deploy/lxc/Caddyfile` uses the state directory selected by both runit services.
The manifest TCP probe means connectivity only. It does not invoke plaintext
HTTP on TLS, publish port 8080 or claim API readiness. Generic telemetry is not
inferred from this TCP declaration.

`hs-server/src/settings.rs::from_env` places `server.toml`, `secrets/pve-token`
and `tls/{cert,key}.pem` under `HOMESTACK_STATE_DIR`. The authenticated routes in
`routes.rs` and `secrets.rs` persist configuration/secrets and reload Caddy.
Secret files use private directories and mode 0600; Caddy runs as container root
and can read the homestack account's TLS files.

The node image now accepts the explicit non-secret selector
`HOMESTACK_MANAGED_HOME=1`. `deploy/lxc/service/state-path` requires an exact
`/home/app` mount, rejects symlinks and creates `/home/app/homestack` owned by
homestack with mode 0700. Both Caddy and the server use that directory;
`HOMESTACK_CONFIG_FILE` is fixed to its `server.toml` in this mode. The mount
itself stays empty in the image. With the selector absent (or 0), the legacy
`/var/lib/homestack` behavior remains. No data is copied or relocated.

The manifest uses `local`, `multi_node: false`: local operational settings,
TLS and secrets are instance state. Existing multiple APIs do not establish a
replication contract for arbitrary new manifest instances.

### Site runtime

`deploy/pangolin-site/service/site/run` is an outbound connector with no health
listener. A missing manifest healthcheck remains unverified. Its specific check
is a running CT plus its own Site's `online: true` from the Integration API.
The existing legacy updater verifies credential restoration and connector
running, but does not independently prove Pangolin online (see integrations.md).

The same explicit selector makes the image and credential receiver use
`/home/app/pangolin-site/site.json`; the directory is owned by site, mode 0700,
and the atomic credential receiver keeps the file at 0600. The CLI's HOME is
that directory too. `env -i`, `DISABLE_SSH=true` and the existing pinned CLI
invocation remain intact; no self-update mechanism is added. Without the
selector, `/var/lib/pangolin-site/site.json` remains the path.

A future initializer must pass the selector explicitly to the credential
receiver as well as set it in the LXC boot environment: `pct exec` must not be
assumed to inherit PID 1's environment. For example, its authenticated guest
command is `env HOMESTACK_MANAGED_HOME=1 homestack-site-credential`, with the
credential only on standard input. This is initialization/recovery, never part
of the generic rootfs replacement. Digest verification uses the same selector.
Each instance has its own credential; no identity is shared between nodes.
The manifest conservatively uses local state and one placement per deployment.

## Read-only plan for completing a new-install handoff

The current browser can discover and review these services. `Source::initialization_blocker`
rejects either internal image repository, even under another manifest id or a
fork. `plan` reports the missing prerequisites; `run` checks again before image
pull, CT creation, environment writes or start. Manually entering environment
variables cannot bypass this guard. This phase does not claim successful new
managed installations. Both paths remain blocked pending these handoffs:

1. API: validate operator choices and free CTIDs through the common planner.
   Reuse `deploy::certificate`, token parsing and existing bootstrap logic in
   `deploy::apply` as a callable initialization step, without invoking its
   replacement loop. Retain the trusted-desktop installer and its first-API
   capability (`install_flow.rs`, `servers.rs::RealServers::deploy`); no running
   HomeStack API may be required. Use the stored valid certificate; do not
   change DNS, renew certificates or provision public exposure in this flow.
2. Before starting a new API, set the managed selector, unique instance name,
   bearer token hash, PVE endpoints and pool settings, and temporary TLS values
   through the existing scoped PVE bootstrap. Common creation attaches `mp0`
   first. Use `instances_client` with the chosen address, certificate hostname
   and configured CA roots; keep TLS verification and API authentication on.
3. Reuse `wait_health` for authenticated HTTPS `/health/live`; remove temporary
   TLS environment values, reboot if pending, confirm their absence, upload
   the PVE token and `server.toml` through `configure`, and verify the stored
   configuration version. Additional private CA files or explicit token-file
   overrides must be persisted/restored too; generic environment entry is not
   a secret transport. Report completion only after these checks.
4. Site: supply the current Step 5 private verified tunnel, independently
   verified Step 7 permanent least-privilege organization key, selected node
   and authenticated root channel to the existing Step 8 initializer. The
   temporary Root bootstrap key is never a substitute. Separate its existing
   CT creation from `create_site`/journal/credential/online operations, retaining
   recovery of uncertain outcomes without duplicate Site creation or rotation.
5. Allocate only a new Site identity, save its private per-node credential
   before creation, and bind its recovery evidence to the new CT. Create the
   CT through common deployment with managed mode and `mp0`, inject its own
   credential using secure stdin, compare digests privately, and require both
   CT running and Pangolin online. Do not reuse a functioning legacy identity.
6. Keep the common `HomeStack service <id>` description, repository, manifest
   revision, version and digest throughout initialization. Put no secrets in
   metadata or progress. A failed initialization remains failed; never create
   another home to hide a missing mount or fabricate readiness.
7. Connect the same HTTPS/online checks to common Update/Migrate/restarts.
   Image replacement itself stays solely in `hs_deploy::recreate`, one instance
   at a time, with its observed-state recovery and exact home-volume safeguards.
   The preserved non-secret managed selector selects the same state after a
   replacement. No root prompt or credential reinjection is needed on ordinary
   managed updates once the home contains the initialized state.

This plan can be assessed from code without reading live secrets or issuing
PVE mutations. Real initialization, restart and rootfs-swap verification needs
separately authorized isolated targets. Mount metadata proves neither stored
application records nor that the application has read them.

## Ownership, releases and the later adoption task

Common ownership takes priority in `tui/reinstall.rs::common_images`. Legacy
API discovery/replacement additionally requires its exact `HomeStack server
<name>.<domain>` description and `made by homestack-deploy` marker with the API
image repository. Pool membership or an arbitrary `image` line is insufficient.
Legacy Site ownership remains checked by pool, node, tags, description, private
journal and credential. Tutor and arbitrary service containers gain no API
identity. Existing six internal containers are not eligible for managed mode
merely because a new image supports it.

API releases keep numeric Git tags via `scripts/tag-api.py`; Site releases keep
`pangolin-site-X.Y.Z`. Both publish numeric OCI versions. The API runtime change
requires an API image release under AGENTS.md. A new Site image supporting the
selector is required before enabling new Site installations; this phase leaves
Site publication pending isolated runtime verification and a release decision.
Existing image versions remain immutable. Publishing authorizes no deployment.

Later adoption requires separate owner authorization and an instance-by-instance
inventory of ownership, rootfs state, mount/volume ownership, certificates,
credentials and private journals. It needs an approved plan to preserve existing
API configuration/TLS/tokens and each Site identity, explicit persistent-volume
preparation/data transfer and application-specific verification. None of those
steps, metadata rewrites or production restarts are performed here. Finish and
verify the new-install handoffs above before treating adoption as supported.

## Local verification and limits

The Cargo wrapper built `homestack`, `homestack-deploy` and `homestack-server`.
A temporary Rust executable exercised the actual schema parser and GitHub
discovery against local GitHub-response fixtures containing both manifests. It
also verified both apply guards before any PVE call, truthful TCP/unverified
probe text, and positive/negative legacy API ownership cases. No permanent
tests were added. Shell checks covered syntax, unchanged legacy selection,
invalid selectors and rejection of an absent mount. Managed selection and
private-directory creation arguments were checked with mount detection and
installation intercepted; no real mounted-volume lifecycle was exercised.

No production credentials were read and no live PVE/Pangolin calls or container
changes were made. Real empty/reused-volume startup, secret retention across a
rootfs swap, validated HTTPS bootstrap and Site online verification remain
isolated-environment requirements before enabling the blocked install paths.
