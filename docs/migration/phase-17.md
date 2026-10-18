# Phase 17: reject disk allocation before changing a VM

## Behavior

Migration planning now reads `GET /nodes/{source}/qemu/{vmid}/migrate?target=…` to identify the local volumes Proxmox will move. It counts each volume once, omits shared volumes, and reads virtual disk capacities from source storage content rather than estimating space from guest filesystem usage or sparse image allocation. Root, home, Cloud-Init, EFI and TPM volumes contribute when present. A local non-Cloud-Init CD-ROM must be detached first. An unknown disk size or unavailable capacity causes a refusal.

The common storage admission check requires an enabled, active target storage with `images` content. The sum of required bytes must not exceed `avail`. Plans include required and available byte counts. Migration repeats the check after workspace revalidation and before shutdown. A capacity refusal leaves a running source VM running and creates no migration task. Recovery after an unrelated migration failure still restarts the source VM.

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

The selected E2E lab has one Proxmox node. A new actual migration cannot run there. The owner's other cluster is production, and was not used for these tests. Phase 8 records the earlier successful live migration; simulator regressions cover successful migration with the new check. The phase 17 cross-node live success gate has not been repeated with this change.
