# PO-01 through PO-04 performance audit

Started on the existing `main` at `0415811ee7e7152119adf2812a9bb47d4862c43c`,
without resetting it. Only PO-01 through PO-04 are in scope.

Measurements use temporary diagnostics comparing the original functions from
that commit with the changed functions. Fixtures and endpoints are local and
synthetic; these are focused behavioral and work-count checks, not live PVE
or E2E evidence. Temporary modules, counters, fixtures and servers are removed
before the final build and commit. No permanent tests are added.

## PO-01: confirmed and optimized

The Settings draw function opened each provider's editor, parsed credentials
and integration settings, checked file existence, and loaded `deploy.toml` on
every frame. These synchronous reads were confirmed in `draw.rs`.

Settings now holds only provider status and saved non-secret test reports,
plus the displayed Prometheus summary. Loading still uses the existing editor
parser and validation, then drops the credential-bearing editor. Summaries
load at startup and refresh on Settings entry/reentry, editor save actions,
return from an editor, integration test results, and the existing explicit
reload request. Opening an editor still reads saved data directly.

Changed files: `crates/hs-cli/src/tui/{draw,state,integrations,prometheus,run}.rs`.

Temporary read-call counters measured **600 persistent text-read attempts
before versus zero after for 100 unchanged Settings renders**. The rendered
buffers were identical. This count excludes existence metadata checks, which
also moved out of drawing. Fixtures covered initial display; missing, valid,
incomplete and malformed providers; a persisted passing report with zone;
and absent, missing-URL, valid and malformed Prometheus configuration. Provider
and Prometheus saves, explicit reload, redraw and Settings reentry were checked.
No watcher, polling loop or async drawing was introduced. TUI-only release
impact: none.

## PO-02: partially confirmed and optimized narrowly

`home_filesystem()` read the ext4 superblock once per filesystem entry, including
repeated entries for the same device. It now remembers each normalized device's
label for that inventory read. It still processes every matching mount and
rejects ambiguity, including duplicate mounts of the same matching device.

Changed file: `crates/hs-core/src/status.rs`; inspected `guest.rs`, status callers,
persistent-home validation, configuration disk roles and the status observation
cache. The existing observation cache already reduces repeated network reads
across refreshes; this change removes duplicate label calls within an inventory
without relying on that cache.

The expected `HS_HOME_<VMID>` label remains authoritative. No mountpoint or
unique ext4 candidate is excluded. A managed home mounted at `/`, `/boot` or
another unexpected target must remain visible; mount names alone cannot prove
identity. Configured PVE root/home roles do not guarantee a corresponding guest
filesystem identity in this response. QGA documents filesystem `disk` hardware
information as potentially empty for unsupported disk types:
[QEMU Guest Agent protocol](https://www.qemu.org/docs/master/interop/qemu-ga-ref.html#object-guestfilesysteminfo).
We therefore rejected root/boot/loop heuristics and hardware-role filtering as
unjustified for this status identity check. Ordinary non-ext4 pseudo, EFI and
snap filesystems already require no label read.

The same counted fake QGA inventories were passed to both implementations:

| Inventory | Label reads before → after | Result unchanged |
| --- | --- | --- |
| Root only | 1 → 1 | No home |
| Root + home | 2 → 2 | Correct home |
| Root + ext4 boot + home | 3 → 3 | Correct home |
| Root + two unrelated ext4 + home | 4 → 4 | Correct home |
| Home at unexpected target | 2 → 2 | Unexpected mount reported |
| Root mounted twice + home | 3 → 2 | Correct home |
| Home mounted twice | 3 → 2 | Ambiguity rejected |
| Two different matching devices | 3 → 3 | Ambiguity rejected |
| No matching label | 2 → 2 | No home |
| Non-ext4 only | 0 → 0 | No block-backed home |
| Virtiofs entry | 0 → 0 | No ext4 label read |

All returned mount and capacity values matched the original. The virtiofs
status branch and stopped/unreachable guest handling are unchanged. No extra
guest commands or subprocesses were added. API image release required because
server status uses this code.

## PO-03A: confirmed and optimized

`listing_gaps()` cached privileges but awaited independent missing storage
permissions sequentially. It now collects distinct storage ACL paths, reads
at most eight concurrently with ordered buffering, and then conditionally
reads `/vms` once. Eight matches the existing default status parallelism.
Keeping `/vms` conditional preserves the behavior when all storages already
have `Datastore.Allocate`, including avoiding an otherwise unnecessary failing
`/vms` request. Empty path sets still return without needing a PVE client.

Changed files: `crates/hs-server/src/reads.rs`, `crates/hs-server/Cargo.toml`.
The existing `futures-util` dev dependency becomes a runtime dependency;
no new dependency version is introduced.

Storage failures propagate in sorted path order. Warning generation retains
its original node/storage iteration order and privilege rules. Failed required
reads still fail the operation. No privileges or cache semantics changed.

A local HTTPS PVE endpoint delayed each permission response by 100 ms. Two nodes
shared sixteen storage IDs. Both implementations made exactly seventeen
distinct permission requests, with `/vms` once, and returned the same thirty-two
ordered warnings. Elapsed time decreased from **2.415 s to 428 ms** (including
local HTTPS transport overhead), consistent with two bounded storage batches plus the
conditional `/vms` read.
Required storage and `/vms` failures were checked separately. With storage
allocation privileges, `/vms` was skipped even when configured to fail.
API image release required.

## PO-03B: confirmed and optimized

`addresses()` read the cluster inventory once, then awaited every guest's pending
configuration serially. It now buffers at most eight independent reads and
consumes their results in the original inventory order. Current/pending IPv4
parsing for LXC `netN` and QEMU `ipconfigN` is unchanged. Allocation remains
fail-closed, unsupported types still fail, and the original inventory snapshot
and pre-creation/recovery rechecks remain intact.

Changed files: `crates/hs-cli/src/services/deploy.rs`,
`crates/hs-cli/Cargo.toml`, `Cargo.lock` (existing `futures-util` dependency).

A fake PVE inventory contained sixteen alternating LXC/QEMU guests, each with
100 ms pending reads. Both versions made seventeen total reads and returned
the same thirty-two current/pending addresses. Elapsed time decreased from
**1.620 s to 203 ms**. Observed pending concurrency peaked at eight. One unreadable guest failed the operation. With multiple
failures, later guests failed in 10 ms while the first failed in 100 ms;
the returned error still named the first failing guest in inventory order,
matching the serial implementation. Unsupported guest types were rejected.
CLI-only release impact: none.

## PO-04A: confirmed and optimized

`hs-usage` converted the startup page from `Arc<str>` to a newly allocated
`String` for each request. Startup now stores `axum::body::Bytes` and the HTML
response consumes a cheap shared clone from extracted state. All startup
substitutions remain in the same place and run once.

Changed file: `crates/hs-usage/src/main.rs`.

Repeated handler requests checked identical body bytes, `text/html; charset=utf-8`
and `Cache-Control: no-store`. The collected response body shared the startup
buffer's exact pointer, demonstrating that it did not copy the page contents.
The substituted fixture page was 56,210 bytes. For one hundred responses,
full-page copies decreased from 100 to zero, avoiding 5,621,000 copied bytes.
Independent AI-usage image release required; this change alone would not
require an API image.

## PO-04B: confirmed and optimized

The WebSocket path cloned every watched Job before serializing a full snapshot.
`StreamMessage<T = Job>` now supports a borrowed Job while retaining the owned
default used by existing consumers. Stream encoding is synchronous and finishes
inside the watch-borrow scope; asynchronous sending occurs after the borrow
has dropped. REST job ownership is unchanged.

Changed files: `crates/hs-server/src/jobs.rs`, `crates/hs-model/src/jobs.rs`.
Inspected `operations.rs`; no operation/event changes were needed.

A representative Job had 500 steps, 500 tasks and 200 log lines of roughly
4 KiB each. Its event JSON was 888,795 bytes. Temporary Clone instrumentation
measured one complete Job clone before and zero after; serialized bytes were
identical with fixed envelope metadata. The actual WebSocket path also made
zero Job clones and delivered Snapshot(1), Heartbeat(2), final Job(3), then a
normal close with reason `job finished`. The published StreamMessage OpenAPI
schema was compared with the existing schema. The 4096 incoming-message limit
and one-way receiver remain unchanged.

Full Job serialization is intentionally retained as part of the protocol.
Delta events were rejected as an unnecessary protocol redesign. Heartbeats
still serialize null data without touching the Job. API image release required;
the generic model change does not change PVE-agent package behavior.

## Final verification

After removing every temporary diagnostic and fixture, `scripts/check.sh --full`
passed: formatting, wrapped locked workspace application builds, six retained
Python checks, strict workspace/all-target Clippy (`-D warnings`), retained
Rust checks, ShellCheck and all twelve installer shell checks. Updated application
binaries remain in `target/debug/`. The complete diff was reviewed for unrelated
changes and passed `git diff --check`. Full live E2E was not run.
Required release lines are the API and AI-usage images. No Pangolin, Collector
or PVE-agent artifacts changed.
