# Phase 17: reject disk allocation before changing a VM

## Behavior

Migration planning now reads `GET /nodes/{source}/qemu/{vmid}/migrate?target=…` to identify the local volumes Proxmox will move. It counts each volume once, omits shared volumes, and reads virtual disk capacities from source storage content rather than estimating space from guest filesystem usage or sparse image allocation. Root, home, Cloud-Init, EFI and TPM volumes contribute when present. A local non-Cloud-Init CD-ROM must be detached first. An unknown disk size or unavailable capacity causes a refusal.

The common storage admission check requires an enabled, active target storage with `images` content. The sum of required bytes must not exceed `avail`. Plans include required and available byte counts. Migration repeats the check after workspace revalidation and before shutdown. A capacity refusal leaves a running source VM running and creates no migration task. Recovery after an unrelated migration failure still restarts the source VM.

Migration plans read power state from the source node's `/status/current`, because cluster inventory can still report `running` after a completed stop. Execution rejects a power state change after confirmation before changing the VM. This prevents migration from starting a VM that was already stopped. Regressions cover stale running inventory and an external stop between planning and execution; the core migration suite (23 tests) and server migration suite (6 tests) pass.

Free-space observations remain visible in plan summaries but do not enter the confirmation digest for creation or migration. Required bytes, disk identities, target placement and power state remain signed. Every replan and allocation still checks current capacity. API regressions accept an unrelated 4 KiB free-space change and refuse a decrease below the required allocation before changing a VM; creation, migration and shared job tests pass.

The owner extended the phase to creation during implementation. `create` uses the same admission check before full cloning or disk allocation, both when planning and when applying the plan. It counts every cloned Gold disk plus the requested new home. Gold disk sizes come from its configuration, which is readable with the Gold role; no migration or disk-configuration permission is added to Gold for production. A generated Cloud-Init drive reserves 4 MiB when Gold omits its size. Missing data/EFI/TPM sizes cause a refusal. A create on another node also checks the temporary full clone on Gold's source node, independently of the final clone-plus-home capacity on the destination.

These checks are admission checks, not storage reservations. Another workload can allocate space after the final check. Existing task error handling and cleanup remain necessary. Comparing full virtual capacities is conservative for thin or sparse storage; the code does not assume that overcommit is safe or that compression will save space.

## Verification

Simulator and HTTP API tests cover:

- migration refusal during planning without shutdown or state changes;
- aggregate disk capacity, including Cloud-Init, and acceptance at the exact byte boundary;
- space disappearing between plan and apply, refused before shutdown;
- an unreadable storage status and an unknown disk size;
- shared disks being excluded from migration capacity;
- normal running/stopped migrations and source recovery after a migration error;
- oversized creation refused before VM/disk allocation;
- creation rechecking capacity before cloning;
- both temporary source and final target capacity for cross-node creation.

The full `hs-core` and `hs-server` test suites passed. The final changed test targets passed again after lint corrections and the API recovery regression was added. Clippy passed for `hs-core`, `hs-server` and `pve-sim`, including all targets, with warnings denied.

## Isolated live refusal check

`scripts/e2e-capacity.py --output /tmp/homestack-capacity-run` selects only the paused verified lab bound to profile `e2e`. It reuses the resize harness's run marker checks, pinned loopback SSH connection, certificate verification, local server and temporary scoped credentials.

The test creates a dedicated empty tmpfs-backed `images` storage. Its capacity limit can change without filling the lab's disk or changing any existing storage. It checks an oversized create plan, a plan with sufficient capacity, and an apply after the limit is reduced. The apply must refuse before allocating a VM or disk. The harness removes its storage, mount, ACLs, token, role and pool and compares the pre-existing Gold/API configuration hashes.

All three checks passed in isolated lab `b2e4128c-29f4-4c56-9e9b-20e0352513c1`. With a 16 GiB limit the 8 GiB Gold plus a 256 MiB home received a valid plan. A 64 GiB home was refused. Reducing the empty storage limit to 1 MiB then refused the previously accepted plan synchronously at `Apply`, before creating a job, VM or disk. The core execution check also remains necessary for capacity changes after a job has been accepted.

The [sanitized live artifact](phase-17-results.json) records capacity evidence, source and server binary hashes, and cleanup. All fixture VMs were absent and existing Gold 100/API 101 configuration hashes remained identical. The temporary storage, mount, token, role and ACLs were removed. No production cluster was used.

## Cross-node live regression

The shared E2E runner now supports `--nodes 1|3`, defaulting to one node, within profile `e2e`. A fresh three-node run passed bootstrap, private networking, cluster join/quorum on every node, Gold preparation, image-cache preparation, HTTPS and browser login. No production cluster was used.

Nine live checks passed through the actual Rust server: HomeStack reported all three nodes online; migration planning refused insufficient capacity; applying a previously admissible plan refused a later shortage without stopping the VM or allocating a job/disk; an admissible free-space change preserved the plan digest. A running workspace moved `pve1 → pve2 → pve3 → pve1` with its home marker intact. A stopped workspace moved `pve1 → pve2 → pve1`, remained stopped, and retained the marker after boot.

The [sanitized cluster result artifact](../e2e-cluster-results.json) records the checks, binary/source hashes and cleanup evidence. The scenario removes only its workspace, temporary storage/mount, pool, role, token and ACLs; Gold configuration stays unchanged. The three-node lab remains paused for the owner's UI inspection. Interrupted one-node and three-node runs also passed process/disk cleanup checks.

The cross-node live success gate is complete. Run the scenario with `python3 scripts/e2e-migrate.py --output /tmp/homestack-migrate-run` against an `e2e --nodes 3` lab paused after verification.
