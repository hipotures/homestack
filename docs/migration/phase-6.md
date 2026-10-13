# Phase 6: `refresh` and recovery (B3, B4, E3)

**Status:** stopped after the opening probe (PLAN.md 1.4: Python's behaviour
looks like a bug). Nothing of B3/B4 is implemented yet.

## Gate

- Decision 4 (journal in the VM's `description`, written with `digest`,
  owner's rules 1–5 and option `force`) is in `decisions.md`.
- The `import-from` probe the phase opens with was run:
  `scripts/live-probe-import-from.sh`, test token, VM 240 cloned from the test
  Gold 248 into `homestack-test`, destroyed afterwards. Result in
  `decisions.md`, "Probe results", 2026-10-13. Both variants work; the script
  refuses VMIDs outside 240–249, any token but `homestack@pve!test` and a Gold
  outside `homestack-gold`, and is checked by shellcheck in `check.sh`.

## Variant for the import

Variant A (import into a free slot while the VM runs, then `PUT /unlink`
without `force`, which leaves the copy as `unusedN`) keeps Python's order:
the copy is made while the workspace runs, the phases stay
`prepared → imported → switching → switched → verifying → verified`, and the
downtime is the same as today. Variant B would change the order of the
journal's phases and lengthen the downtime, so it is not used (PLAN.md 1.2,
no change of behaviour without a decision).

## Why the phase stopped

Python unlinks disks with `qm disk unlink VMID --idlist DISK [--force 1]
--digest DIGEST` (`python/src/homestack/lifecycle.py`, `_unlink_disk`,
`_digest_args`). `qm disk unlink` is the API method `PUT
/nodes/{node}/qemu/{vmid}/unlink` (qemu-server `src/PVE/CLI/qm.pm`: `unlink
=> ["PVE::API2::Qemu", 'unlink', ...]`), whose schema has only `node`,
`vmid`, `idlist` and `force`, with `additionalProperties => 0`
(`src/PVE/API2/Qemu.pm`). The probe confirmed it on PVE 9.2.21: with `digest`
the call fails `400 Parameter verification failed: digest: property is not
defined in schema`.

Consequences in Python, whenever the VM config has a `digest` (it always
does):

1. `refresh_workspace` fails at "Switch root disks": unlinking the old root
   is refused, so no refresh can finish on a real cluster.
2. The automatic `_recover_refresh` that follows tries to delete the staged
   root through `_delete_unused_volume` → `_unlink_disk(force=True)` with
   `--digest`, which is refused too. Result: "Automatic recovery also
   failed", the journal stays, and the staged copy of Gold's root stays as
   `unusedN`.
3. A later `refresh` goes to `mode="recover"` and fails the same way, so the
   VM can be brought back only by hand.

Python's tests do not see it: `tests/test_lifecycle.py` replaces
`_unlink_disk` with a fake.

The Rust server cannot repeat the guard either: the API has no conditional
unlink. What it can do, and what I would propose:

- unlink without `digest`, right after re-reading the config and checking
  that the slot still holds the expected volume (Python checks the same just
  before), and check the result after;
- the journal write before each unlink is made with the current `digest`, so
  a change made by someone else between the read and the journal write is
  still caught; only the short window between the journal write and the
  unlink is unguarded.

This is a deliberate difference from Python (which never gets past the
unlink) and needs the owner's word, as does whether Python's
`_unlink_disk` should be fixed in Python (the agent does not change Python).

## Open questions for the owner

1. Accept the proposal above for unlinking without `digest` in Rust?
2. Python's `refresh` cannot finish on PVE 9.2 (see above): fix it in Python,
   or leave Python as it is and treat only the Rust `refresh` as the working
   one? This also decides what the parity tests compare against for the
   switch and recovery steps.
