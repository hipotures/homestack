# HomeStack service manifest: proposed schema 2

**Status: design contract, NOT implemented by the current HomeStack parser.**
The production parser in `crates/hs-cli/src/services/manifest.rs` currently
accepts `schema: 1` only and rejects unknown fields. The deployed schema-1
contract remains [manifest-schema.md](manifest-schema.md). This document is
the authoring target for the later HomeStack Deployment reconciliation phase,
not permission to break existing service discovery today.

When preparing an application for the future format, propose a schema-2 file
and review its runtime/build implications. Do **not** replace an existing,
deployable `.homestack/*.yaml` with schema 2 until HomeStack accepts it, unless
the owner expressly accepts that current deployment will stop recognizing
the service. A draft may live in documentation outside `.homestack/*.yaml`
until the parser and deployment consumer are upgraded.

## Ownership and meaning

- The application repository owns the *portable service declaration* and
  publishes the actual OCI image. Its author/agent chooses and tests exact
  initial CPU, memory, swap, disposable-root and persistent-volume sizes.
- The HomeStack operator owns deployment decisions: selected release digest,
  Proxmox nodes, storage, CTIDs, addresses, credentials, proxy exposure,
  resource overrides and individual approved actions. Do not embed these
  environment-specific values in the manifest.
- Every size is a **single proposed deployment value**. There are no
  `minimum`/`recommended` resource tiers and no dynamic sizing implied.
  HomeStack initially fills the Deployment UI from the manifest, then uses
  the operator's approved values exactly.
- The future Deployment screen compares the desired deployment (manifest
  plus explicitly saved operator overrides) with observed Proxmox state.
  The operator can select or deselect each proposed correction. Nothing is
  applied merely by reading or refreshing a manifest. Resource-only updates
  do not rebuild or republish the OCI image.
- A saved override is distinct from skipping a discrepancy once. A lasting
  override affects future desired-state comparisons; a skipped action only
  omits it from that reviewed apply.
- Missing desired production instances are candidates for creation; an
  independently created test instance is not an unwanted production replica.
  Do not infer ownership from names alone, and do not automatically delete
  extra or foreign guests.

## Proposed portable fields

The eventual parser must reject unknown fields, invalid values and
unsupported versions, as schema 1 does. These names and semantics are
**proposed**, not claims about currently implemented validation.

| Field | Type | Proposed semantics |
| --- | --- | --- |
| `schema` | integer | Exactly `2`. |
| `id` | string | Stable service identity; retain schema-1 ID rules. |
| `name` | string | Display label, not instance identity. |
| `image.repository` | string | Public OCI repository without tag/digest. HomeStack picks a verified numeric release and resolves the digest. |
| `runtime.resources.cores` | positive integer | Exact default vCPU count for each created instance. |
| `runtime.resources.memory` | size string | Exact default memory limit. |
| `runtime.resources.swap` | size string | Exact default swap limit; `0M` disables swap. |
| `runtime.resources.root` | size string | Exact size of the disposable LXC rootfs. |
| `runtime.ports` | list | Same named ports and TCP rules as schema 1. |
| `runtime.healthcheck` | mapping | Same HTTP/TCP rules as schema 1. HTTP uses the actual listener and readiness path. |
| `persistence.mode` | string | Required: `none` or `persistent`. |
| `persistence.mount` | string | Required only when persistent: `/home/app`. |
| `persistence.size` | size string | Required only when persistent; exact new persistent volume size. |
| `persistence.directories` | list | With persistence: relative durable paths under the mount, as in schema 1. |
| `persistence.mappings` | list | With persistence: current schema-1 mapping semantics, for review only. |
| `deployment.instances` | positive integer | Desired number of production instances initially reconciled by a single Deployment review. **Not** a global instance limit. |
| `deployment.placement` | string | `single_node` or `distinct_nodes`: placement policy, not PVE node names. |
| `deployment.hostname_prefix` | string | Optional stable generated prefix; suffixes are `-1`, `-2`, etc. |
| `deployment.state_model` | string | `stateless`, `local`, `independent_local` or `external`, defined below. |

Sizes use explicit `M` (MiB) or `G` (GiB) suffixes, interpreted as
binary units; no implicit bare-number unit or floating-point value.
`0M` is only valid for swap. Root and memory must be positive, as must
the persistent size when enabled. The future parser/deployer must validate
against supported Proxmox constraints, available storage and overflow; the
skill does not assert that a particular platform can allocate every
syntactically valid size.

## Instance policy is not high availability

- `instances: 1` creates a single **production** instance by default,
  without prohibiting additional, deliberately isolated test deployments.
- `instances: 3` with `placement: distinct_nodes` means one reviewed
  deployment plans three production instances on three different eligible
  Proxmox nodes. The operator still selects/approves actual placements
  and storages. Fail if insufficient eligible distinct nodes; do not silently
  co-locate instances.
- `hostname_prefix: example` proposes names `example-1`, `example-2`,
  `example-3`. Identity must remain stable across updates; do not
  repurpose an unrelated instance just because its name matches.
- `max_instances` is intentionally absent. The manifest does not forbid
  the owner from creating an additional, separately isolated development
  or test instance on another node or with another image.
- These are application replica-count and placement declarations, not
  Proxmox HA, database replication, shared storage, automatic restart
  elsewhere or a background self-healing controller.
- On initial Deployment, reconcile an empty observed state into the
  requested set. Later, propose the specific missing instances and other
  drift corrections. A batch review is **not** an atomic multi-node
  transaction: the implementation must report partial success and support
  narrow, safe resumption.

### State model

- `stateless`: no irreplaceable local data; the runtime must bootstrap
  entirely from its image and inputs supplied again on deployment.
- `local`: state belongs to one production instance, e.g. SQLite in
  a dedicated `/home/app`. Another new instance with a different empty
  volume is **not** a replica of the same data. The default production count
  should be 1 unless the application explicitly supports independent
  instances with separate data.
- `independent_local`: each instance has its own local/bootstrap state,
  yet multiple instances can serve the same logical service independently
  because required configuration/credentials can be re-injected or otherwise
  reconstructed per instance. This **does not** imply disk sharing, state
  replication or safe simultaneous writes to the same SQLite database.
- `external`: shared durable state is provided by an explicitly supported
  external service; the application author must verify concurrent instances
  are safe and can access that state. No external credentials or addresses
  belong in this manifest.

`state_model` is not permission to create arbitrary numbers of replicas.
The skill must assess real application semantics and document why the
selected `deployment.instances` and placement are safe.

## Persistence and replacement

- `mode: none`: do not create or mount `mp0`. The root filesystem is
  disposable; all required configuration and credentials must be
  reproducibly supplied at (re)initialization. Temporary copies on the
  root do not by themselves make the application persistent.
- `mode: persistent`: create a dedicated mount at `/home/app`. The app
  never starts with a missing/wrong home, never initializes an expected
  existing volume as though it were empty, and never overwrites its durable
  contents during a root-image replacement. Keep database/WAL/SHM files
  together.
- A missing stateful instance is not automatically recreated with an
  *empty* persistent volume. Reconciliation must identify ownership and
  preservation/restore requirements before proposing any change.
- Never shrink persistent storage as a routine reconcile action. Growing,
  recovery, backups, and intentional data loss require the appropriate
  explicit review and separate safety validation.
- Choosing `mode: none` for an existing app may require real entrypoint,
  initializer and lifecycle changes. Do not claim it is valid just because
  a desired future architecture can re-inject secrets.

## Network and authentication boundary

- For new HTTP services in this environment, use ordinary **HTTP inside the
  trusted LAN**; Pangolin terminates public HTTPS and performs remote
  authentication. The application image should not bundle its own TLS
  certificate, key, renewal or TLS-only proxy for that purpose.
- Direct local access is controlled by the owner's network policy (VLANs and
  firewall); do not introduce an additional login just for HomeStack
  deployment. An app's intrinsic authorization/data model is separate.
- An existing TLS-only implementation must actually be adapted and tested
  before its manifest advertises an HTTP readiness endpoint. Do not claim
  that changing a YAML port rewrites the service protocol.
- No Pangolin domain, certificate, DNS target, PVE credentials, tokens,
  private keys, passwords or other secrets appear in the manifest.
  Integration and secret delivery are handled independently at deployment.

## Example: independent service API

Illustrative values only, **not measurements** and not a statement that
any existing HomeStack API image currently supports this configuration.

```yaml
schema: 2
id: example-api
name: Example API
image:
  repository: ghcr.io/example/example-api
runtime:
  resources:
    cores: 1
    memory: 256M
    swap: 0M
    root: 512M
  ports:
    - name: http
      port: 8080
      protocol: tcp
  healthcheck:
    type: http
    port: http
    path: /health/ready
    expected_status: 200
persistence:
  mode: none
deployment:
  instances: 3
  placement: distinct_nodes
  hostname_prefix: example-api
  state_model: independent_local
```

## Example: single-instance stateful service

This is a *shape example*; resource values must be measured against the
particular built image and representative workload.

```yaml
schema: 2
id: example-vault
name: Example Vault
image:
  repository: ghcr.io/example/example-vault
runtime:
  resources:
    cores: 1
    memory: 384M
    swap: 0M
    root: 1G
  ports:
    - name: web
      port: 80
      protocol: tcp
  healthcheck:
    type: http
    port: web
    path: /health/ready
    expected_status: 200
persistence:
  mode: persistent
  mount: /home/app
  size: 1G
  directories:
    - data
deployment:
  instances: 1
  placement: single_node
  state_model: local
```

An additional isolated test instance may run with its own CTID, address and
new volume. It is not a replica of the production database.

## Phase separation

1. **Skill and application preparation:** assess base image, implement
   container runtime, execute local build/run preflight, and prepare
   schema-2 proposals without making live schema-1 services unreadable.
2. **HomeStack implementation:** introduce schema-2 parsing, preserve the
   application/service identity, and unify the existing Deploy/Manage entry
   points as **Deployment** with discover/diff/select/review/apply/verify.
   Resource-only changes must not create a new OCI release.
3. **Explicit manifest migration:** once the upgraded HomeStack reader is
   available, migrate each service's `.homestack/*.yaml` and verify
   its real deployment and preservation behavior.

No application agent working only from this draft is authorized to modify
HomeStack's CLI, Proxmox guests or cluster resources.
