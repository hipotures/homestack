# Isolated end-to-end laboratory

The implemented laboratory is a Rust controller in `crates/hs-e2e`. It provisions
one or three real nested Proxmox nodes, prepares Gold, and optionally installs
HomeStack and runs functional scenarios. It uses direct rootless QEMU, private
Unix sockets and run-owned disk files. It does not use libvirt, a simulator, an
existing Proxmox cluster, or production credentials.

The [bootstrap guide](e2e-bootstrap.md) describes provisioning, DNS/TLS fixture
reuse, filtering, node topology, manual inspection, profiles and cleanup.
The [scenario guide and coverage inventory](e2e-scenarios.md) describes the
implemented functional layer and its remaining gaps. The old proposed catalogue
has been replaced by this current implementation inventory.

## Trust and ownership

The Linux execution host must provide accessible `/dev/kvm` and nested
virtualization. The controller verifies real nested KVM inside Proxmox before
running product scenarios. There is no software-emulation substitute.

Every run creates a UUID, temporary private directory, synthetic SSH identities,
pinned loopback SSH/API forwards, private backbone when needed, and node-local
`/etc/homestack-e2e.json` markers. Scenario PVE/guest commands verify those markers
before performing operations. Workers additionally require the active controller's
private scenario socket and a process group allocated by that controller.

Only the disposable `10.77.0.0/24` workload subnet is accessible through guest
SSH. The existing external Internet boundary blocks the runner, other private
networks and metadata endpoints. No host bridge, TAP, host route, `sudo`, personal
private-key copy, or weakened password authentication is needed.

Within a new run, the product installer creates its normal pools, roles, users,
ACLs, separated tokens and snippets. These are laboratory resources even though
their names match product defaults. A matching VMID or name alone never proves
ownership. Scenario deletion additionally checks the submitted plan, established
VM identity or explicit run marker. An ambiguous clone result is retained for
lab-level destruction rather than guessed guest cleanup.

## Execution and failure containment

Scenarios execute sequentially. Each result records its preconditions, topology,
timeout, observations, original error and independent cleanup outcome. An
ordinary assertion failure permits the next independent scenario. Unsafe or
unconfirmed fixture cleanup blocks scenarios that require the installed baseline.
Infrastructure observations can still execute. A timeout never establishes that
an asynchronous job did not mutate state.

The catalogue reuses the resize, migration and capacity Python fixtures instead
of implementing a second layout engine. Their standalone paused-lab entry points
remain available. Their credentials and local server state live only in the
private run directory when invoked by the controller. Only sanitized observations,
job/task identifiers and cleanup evidence are retained.

Normal teardown first attempts public HomeStack removal and independently checks
PVE state. The outer controller always terminates and reaps the owned QEMU nodes
before deleting disks and credentials, including after bootstrap or product
failures. A cleanup failure fails the run even if functional assertions passed.
Original and cleanup errors are recorded separately.

DNS records and cached public certificates persist across ordinary runs.
`cleanup --all` is a separate explicit operation and is never called by a suite.
The API fixture owns journaled `api1`, `api2` and `api3` names as required by the
selected topology; pre-existing matching unowned DNS records are left unchanged.
Conflicting records are refused, never overwritten.

## Current evidence

The implementation must be built with `scripts/cargo.py`. Listing scenarios,
syntax checks, Clippy and prerequisite-refusal checks validate the harness only.
A live product pass requires executing the real laboratory and inspecting its
report. Initial sandboxed attempts could not see `/dev/kvm`; a subsequent
unsandboxed check confirmed usable host KVM and the existing private QEMU cache.
Both topology retries outside the sandbox passed host preflight and stopped at
the retained fixture's requirement for `--local-dns 192.168.100.1`. Neither retry
started a Proxmox VM or ran a product scenario. The scenario guide identifies
remaining manifest-service coverage gaps separately from execution prerequisites.
