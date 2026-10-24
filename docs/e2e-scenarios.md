# Functional E2E scenarios

The controller supports a stable concrete registry, sequential execution,
per-scenario deadlines, independent PVE/guest observations and guarded cleanup.
The catalogue is implemented in `crates/hs-e2e/src/scenarios.rs`; it contains no
plugin system, workflow engine or scenario language.

## Commands

Build the actual executables first:

```sh
python3 scripts/cargo.py build --locked -p hs-e2e -p hs-cli -p hs-deploy -p hs-server
target/debug/homestack-e2e list-scenarios
```

Use a server image with OCI label `org.opencontainers.image.revision` naming its
full source commit. Prefer an image built from the clean commit being tested.
An existing release is also accepted when Git independently confirms that all
server sources, dependency declarations, lockfile and image/build inputs are
identical to the tested revision and current working tree. Harness-only or
documentation commits therefore do not require republishing unchanged API code.
The report records both revisions and the compared paths. Push a new image to a
registry accessible through the existing E2E registry proxy. Use an explicit tag
and its expected `sha256:` manifest digest. For example, an owner can use the
existing API image workflow or build the repository `Containerfile` target `node`
with that label. Ordinary harness-only branch pushes do not publish API releases.
Never create a numeric API release merely to test harness changes.

The controller resolves the tag, requires its expected digest, records the
linux/amd64 platform digest, reads its OCI revision label through the node's `skopeo`, verifies source
provenance with Git, and checks that the tag did not change during installation. The current
product deployer uses tagged OCI archives, so these checks detect tag changes;
they do not add digest-reference support to the product. There is no automatic
selection of the newest published image for scenario runs. Existing infrastructure
and manual inspection runs retain their explicit preparation behavior.

```sh
# Set these to an actual image built from the checked-out commit.
E2E_IMAGE=ghcr.io/hipotures/homestack-server:YOUR_TEST_TAG
E2E_DIGEST=sha256:YOUR_64_HEX_MANIFEST_DIGEST

target/debug/homestack-e2e run --nodes 1 --suite smoke \
  --disk-gib 32 --memory-mib 8192 --timeout-seconds 7200 \
  --server-image "$E2E_IMAGE" --server-image-digest "$E2E_DIGEST" \
  --artifacts /tmp/homestack-e2e-one

target/debug/homestack-e2e run --nodes 3 --suite full \
  --disk-gib 32 --memory-mib 8192 --timeout-seconds 7200 \
  --server-image "$E2E_IMAGE" --server-image-digest "$E2E_DIGEST" \
  --artifacts /tmp/homestack-e2e-three

target/debug/homestack-e2e run --nodes 3 --scenario workspace.migrate \
  --disk-gib 32 --memory-mib 8192 --timeout-seconds 7200 \
  --server-image "$E2E_IMAGE" --server-image-digest "$E2E_DIGEST" \
  --artifacts /tmp/homestack-e2e-migration
```

Use a new artifact directory for every run. Supply the existing `--local-dns`,
`--tsig-file`, `--spaceship-file`, and `--fixtures` options where needed by the
execution host. API2/API3 DNS aliases use the persistent ownership journal.
The isolated client also routes the deployer's bounded single-packet `ping`
through pinned SSH to the owned primary Proxmox node, checking its run marker
before sending real ICMP to the guest. No host routes or additional privileges
are required. All nodes, including the primary, have the uniform
`pve-e2e-pveN` SSH alias used by independent observers.
File provisioning also requires `rsync` on the execution host and guest.
The bootstrap timeout covers infrastructure phases; scenario timeouts are
separate, so a full resize matrix can take substantially longer.

`--scenario` includes installation, installation confirmation and the effective
permission audit automatically. `smoke` selects installation, authorization,
read/refusal API checks, workspace lifecycle and three-node inventory.
`full` selects the entire registry, including explicit coverage gaps. Therefore
**full is not yet a passing-completeness gate**: manifest-service scenarios below
remain `BLOCKED` until their terminal driver and purpose-built image fixture exist.
There is no destructive suite or cluster-membership product feature.

Omitting both `--suite` and `--scenario` preserves infrastructure-only runs.
Existing `--pause-after`, `--pause-on-failure`, `resume`, `prepare`, `profile`,
Gold inspection and standalone scripts remain available. Scenario runs have no
implicit pause or interactive installation prompt.

## Current-feature coverage inventory

All rows refer to current source, including `hs-cli/src/args.rs`,
`hs-cli/src/tui/state.rs`, `hs-cli/src/services/`, `hs-server/src/routes.rs`,
`hs-server/src/lifecycle.rs`, `hs-install/src/`, `hs-core/src/` and `hs-deploy/src/`.
A scenario registration means executable coverage exists, not that it has passed
on a live host. See the actual run report for evidence.

| Current capability and public interface | Scenario | Topology / fixtures | Independent observation |
|---|---|---|---|
| Installer confirmation refusal: `homestack --json install` without `--yes` | `installation.confirmation` | Both; prepared inputs | Exit 3, confirmation flag, unchanged users/roles/ACLs/Gold/guest inventory |
| Complete install: `homestack install --yes` | `installation.complete` | Both; explicit revision image, cached wildcard certificate, Gold 100 | Actual running API CTs 101–103, pool/node placement, installed client URLs and `status`, HTTPS, snippet source checksums |
| Runtime/deploy authorization | `authorization.effective` | Both; separated tokens, foreign VM 243, authorized fixture VM 244 | Effective user and token rights at exact VM/storage/node paths, absence of administrative grants, real PVE 403 with unchanged foreign config |
| Repeat permission install: `homestack install --no-servers --yes` | `installation.idempotency` | Both; installed baseline | Zero applied steps; unchanged PVE roles/ACLs/token metadata |
| Installer TUI access/cancel | `installation.cancel` | Both; actual binary and isolated PTY | Terminal journey, unchanged authorization, guest inventory and API jobs |
| Uninstall/reinstall: `homestack-deploy remove`, `homestack uninstall/install` | `installation.reinstall`; final uninstall phase | Both; installed baseline | Guest/user/ACL absence, fresh HTTPS and client readiness |
| Configuration version, capabilities, discovery, networks, storage, orphans: public GET routes | `api.reads` | Both; Gold and API baseline | PVE/API node inventories agree; actual Gold config; memory telemetry matches the guest-visible cgroup limit (null when `memory.max` is `max`) |
| Missing bearer, invalid inputs and foreign destroy refusal: `POST /v1/plans` | `api.refusals` | Both; foreign VM 243 | HTTP 401 without bearer, 422 for invalid inputs, and 404 for a foreign VM hidden from the runtime token; unchanged config, stable inventory, volume identities and jobs |
| CLI create/status and API start/stop/destroy | `workspace.lifecycle` | Both; new workspace 242 | PVE identity, node/pool/power state, QGA, Cloud-Init, pinned guest SSH, persistent marker, final VM/volume absence |
| Refresh: `workspace.refresh` plans/jobs | `workspace.refresh` | Both; disposable root and persistent home | Root-only marker removed; same home volume, filesystem UUID, content, owner and mode |
| Add-key: `workspace.addkey` plans/jobs | `workspace.addkey` | Both; synthetic temporary key | New key absent before addition, then present in authorized_keys; actual SSH read using only the new disposable identity through the isolated jump |
| Stale-plan refusal | `workspace.stale-plan` | Both; run-owned VM name mutation | HTTP 409, same root/home disks and persistence marker |
| Setup and file copy: `homestack setup` | `workspace.setup-files` | Both; synthetic `.gitconfig`, isolated client SSH alias | Guest bytes, owner and mode; no personal source files |
| Create admission before/after free-space change | `workspace.capacity` | Both; existing capacity fixture and tiny tmpfs storage | No VM, disk or accepted job after refusal |
| Root/home resize and same-size recovery | `storage.resize.{home,root}.{layout}` | Both; existing layout fixtures and local current server binary | PVE disk size, filesystem growth, UUID, marker and ownership/mode; interruption barrier and recovery; shrink refusal |
| Unsupported resize layouts | Same IDs with `btrfs-multi`, `lvm-multi`, `partition-not-last` | Both; existing refusal fixtures | Documented refusal, unchanged PVE and guest snapshot |
| Running/stopped workspace migration and target storage admission | `workspace.migrate` | Three; existing migration fixture and owned target storage | PVE placement/power, persistent home marker, task outcomes, no allocation on capacity refusal |
| Cluster inventory/quorum | `cluster.inventory` | Three; owned nested cluster | `pvesh /cluster/status`, per-node PVE versions and storage inventory |
| API multiple instances and one-instance failure | `service.instance-recovery` | Three; installed API CTs | Actual CT stopped, authenticated status from both peers, CT restart and HTTPS recovery |
| Node unavailability and recovery | `cluster.node-recovery` | Three; owned pve3 QEMU process | Offline node with surviving quorum, available API, recovered three-node membership; guarded resume on failure |
| OCI API service deployment/readiness/removal | Installation and uninstall phases | Both; real selected server image | Actual CT placement/configuration, HTTPS and final guest absence |

Resize layouts are `ext4-whole`, `ext4-partition`, `lvm`, `luks-key-lvm`,
`luks-pass-lvm`, `btrfs-subvolume`, `btrfs-luks`, plus the three refusal layouts.
Each root/home/layout case is independent. The Rust controller invokes the same
`Matrix`, `Migration` and `Capacity` implementations as the standalone scripts.
Those fixtures deliberately use a separate current compiled local server and
scoped PVE token, including server-kill recovery; this differs from the installed
HTTPS API scenarios and is recorded in their evidence.

## Explicit coverage gaps and absent product features

These currently supported workflows have **no executable functional scenario** in
this implementation. Their registry entries are `BLOCKED` with category
`coverage_gap`, so a full run cannot incorrectly claim success:

| Scenario | Missing functional coverage |
|---|---|
| `service.manifest` | Actual TUI manifest deployment/reconciliation, multiple manifest instances, health/telemetry and removal |
| `service.resources` | TUI CPU, memory, root and persistent disk resource reconciliation |
| `service.image-persistence` | TUI image replacement with actual CT identity and `/home/app` persistence checks; purpose-built versioned service image fixture |
| `service.migrate` | Actual TUI service/container migration and target-storage refusal |

These are harness implementation gaps, **not absent product capabilities**.
Manifest services are managed by desktop/TUI code; the current HTTP API exposes
workspace operations rather than manifest-deployment routes. Calling service
implementation functions from E2E would bypass that public workflow and is not
used as a substitute. Pangolin and external GitHub/repository integrations also
need isolated service-side fixtures; no real account or production connector is
used. The present setup scenario covers file provisioning rather than repository
key rotation or application installation.

Adding/removing Proxmox cluster members is infrastructure administration, not a
HomeStack product operation. No cluster-membership scenario is registered.
Workspace recovery includes the existing resize interruption path; complete
refresh failure-injection/restart recovery remains uncovered. The API read checks
do not cover every configuration-write or secret-rotation route.

## Reports and result semantics

Each run writes atomic `report.json` and `scenarios.json`, plus `summary.txt`.
Per-scenario `scenarios/ID/evidence.json` retains sanitized CLI output, API
responses, job IDs, task UPIDs, PVE configuration and guest observations.
Original operation and cleanup failures have separate fields. Private fixture
logs, bearer/PVE tokens, TLS private keys and guest private keys stay under the
private lab directory and are removed with it.

The machine report contains tested Git commit, dirty-worktree indicator, expected
and resolved image digests, OCI revision, topology, host versions, bootstrap
phases, installation, scenarios, uninstall, infrastructure cleanup and totals.
Unavailable PVE/image versions remain absent when preflight prevents provisioning;
they must never be inferred from a selected image name.

- `PASSED`: the attempted operation and independent assertions succeeded, and
  scenario cleanup succeeded.
- `FAILED`: an attempted operation/assertion failed, or cleanup failed. Unexpected
  missing permissions are failures. A correct authorization refusal passes only
  with independent unchanged-state evidence.
- `BLOCKED`: required baseline/dependency is unavailable, or an explicit coverage
  gap prevents execution. Baseline damage blocks dependent mutations.
- `OUT_OF_SCOPE`: selected topology cannot support the registered scenario.

Independent scenarios continue after failures. Bootstrap may abort meaningful
execution, but pending applicable cases become blocked and cleanup still runs.
Exit is nonzero for failed/blocked required scenarios or incomplete cleanup.
A host preflight failure also creates reports without allocating lab resources.

Initial sandboxed attempts could not see `/dev/kvm`. This was a restriction of
the execution session, not an absence of KVM on the host: an unsandboxed check
successfully opened the device and found the existing private QEMU cache.
Both unsandboxed topology retries passed host preflight, then stopped because
the retained DNS fixture requires `--local-dns 192.168.100.1`. No Proxmox VM or
product scenario started. The original commands were:

```sh
target/debug/homestack-e2e run --nodes 1 --suite full --artifacts /tmp/homestack-e2e-check-one
target/debug/homestack-e2e run --nodes 3 --suite full --artifacts /tmp/homestack-e2e-check-three
```

Run from an environment with access to host KVM and supply the DNS server
recorded by the retained fixture. For this host, add `--local-dns 192.168.100.1`
to both commands. Complete functional runs also require the explicit
revision-matched image and expected digest described above. Use new artifact
directories for each retry. Routine runs never invoke `cleanup --all`.

Inventory comparisons sort unordered Proxmox collections and omit live usage
counters. Identity, placement, power state, configured allocations, volume IDs
and exact foreign VM configuration remain part of refusal checks.

Installer discovery can address cluster peers by their private IPs. The isolated
SSH inventory pins both peer names and `10.77.0.1` through `10.77.0.3` to the
owned nodes' loopback forwards, using the existing synthetic keys and host pins.

Cluster bootstrap also pins the seed-injected public host keys inside every
disposable PVE node and verifies authenticated peer SSH in both directions.
The installer uses this node-to-node SSH to provision snippets. Host-key
checking stays enabled; no personal private key is copied into the lab.

Workspace cleanup queries image content explicitly and checks both native owner
VMID and the reviewed disk volume IDs. Proxmox's `vmid` query filter does not
exclude unrelated snippets and templates from an unrestricted content listing.
Sanitized root PVE observations are retained in each scenario's evidence.

Resize fixtures resolve the newly allocated home/root preparation disks by
unique QEMU serial, refuse the active root ancestry and reject a nonblank new
baseline home. Guest device names such as `/dev/sdb` are not disk identity.
Cloud-Init completes before fixture mutation; encrypted root boot uses its LUKS
UUID. The cancellation driver recognizes the installer access screen and the
client history uses `$XDG_STATE_HOME/homestack/tui.log` (default
`~/.local/state/homestack/tui.log`), within the isolated E2E profile.

Node-outage checks wait for two stable observations of surviving quorum.
Recovery remains pending until two observations show all three nodes quorate
and the installed APIs/snippets are ready. Failed recovery blocks subsequent
mutations. Capacity, migration and resize workers require the verified Gold
and safe infrastructure; their independently scoped local API does not depend
on the suite API containers remaining installed after a reinstall failure.
