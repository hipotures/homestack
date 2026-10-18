# Phase 16: live resize disk-layout matrix

`scripts/e2e-resize.py` runs the resize matrix in the paused, verified lab selected by profile `e2e`. It never uses the production client configuration. Every node command verifies `/etc/homestack-e2e.json` against the selected run ID, and SSH uses that run's pinned loopback connection.

The generator refuses an occupied VMID range 240–249. It creates a disposable base at VMID 241 and sequential variants at VMID 240, all in `homestack-test`. A temporary privilege-separated API token can modify only that pool; node permissions are read-only, and disk-allocation permission is restricted to the lab's `local` storage. A separate local Rust server runs the real plan/apply/job API over the lab's SSH SOCKS tunnel, with HTTPS certificate and hostname verification enabled for Proxmox. Gold and existing API containers are not reconfigured.

## Matrix

Each layout is tested independently for both the mounted root and persistent home:

- ext4 on the whole disk;
- ext4 on the last partition;
- LVM with one PV and one linear LV, containing ext4;
- LUKS with a key file, then LVM and ext4;
- LUKS with a passphrase, then LVM and ext4;
- single-device Btrfs with a subvolume;
- single-device Btrfs on LUKS.

Each positive case grows the disk by 256 MiB through the server and verifies filesystem capacity and a data marker, repeats the same requested size, then tests recovery after interruption. For that recovery check, a guest barrier stops the final filesystem command after disk growth; the test kills the actual server process and blocked command, restores the tool, restarts the server and reapplies the same target size. The filesystem must grow without another disk enlargement or a changed data marker.

Three additional layouts must be refused during planning for each role: multi-device Btrfs, a VG with multiple LVs, and a selected partition that is not last. The refusal check compares the VM configuration digest, block layout, filesystem capacity and data marker before and after the request.

Root variants boot the copied operating system on the selected real disk stack using a fixture kernel/initrd, rather than installing a separate bootloader for every stack. The encrypted boot fixture uses a known test passphrase; the resize request still exercises key-file or passphrase authentication according to its own `/etc/crypttab` entry. The selected root is mounted at `/`, not simulated by changing a home configuration field.

## Run and evidence

Keep the E2E controller paused after `verify`, then build the server and run the matrix through the project's quiet verification agent:

```sh
python3 scripts/cargo.py build --locked -p hs-server --bin homestack-server
python3 scripts/e2e-resize.py --output /tmp/homestack-resize-run-1
```

The output directory must not already exist. `--role home` or `--role root` and repeated `--layout NAME` options select diagnostic subsets. A subset is not the full phase gate. During fixture debugging, `--keep-base-on-failure` retains only the owned, completed base template and its boot files after a failure; the temporary server credentials and variant are still removed. A subsequent run must explicitly select `--reuse-base` to accept that template after checking its owner, pool and run ID. A successful run removes the retained template as well.

`results.json` records every case and its capacity/marker evidence. `cleanup.json` records remaining fixture VMIDs and hashes proving that pre-existing Gold/API configuration was preserved. Detailed server and SSH logs stay in the private output directory. The harness removes its VMs, token, ACLs, role and node boot files; it removes the test pool only if the fixture created it. Cleanup refuses a VM whose ownership marker changed.

## Live result: passed

All 20 cases passed in isolated lab `b2e4128c-29f4-4c56-9e9b-20e0352513c1`: 14 positive cases (seven layouts for each role) and six refusals. Every positive case passed growth, a same-size repeat and recovery after killing the server between disk growth and filesystem growth. Every refusal preserved the complete captured state.

The sanitized [result artifact](phase-16-results.json) records capacities at each stage, marker checks, refusal reasons, the tested server commit and binary hash, and cleanup evidence. The initial smoke run supplied the two home cases for whole-disk ext4 and passphrase LUKS/LVM; the root retry supplied the corresponding two root cases; the remaining matrix supplied 16 cases. Failed fixture attempts are excluded from the pass count. The first smoke retained its owned base explicitly; the root retry and final matrix both removed all fixture VMs. Configuration hashes for Gold 100 and API container 101 remained identical throughout.

Fixture corrections included waiting for Cloud-Init completion without treating its recoverable status as a guest command failure, preparing home in PID 1's mount namespace with a fresh disk, refreshing the guest agent's mount view, accepting HomeStack's lease suffix after the owner marker, and discovering a snapshot's disk from its mounted filesystem instead of assuming Linux device-letter order. Encrypted root boot uses its LUKS UUID for the same reason. No Rust resize implementation change was needed.

The phase 16 live gate is complete.
