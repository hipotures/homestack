# Migration decisions

Decisions from section 10 of [api-mapping.md](api-mapping.md) and the results
of probes on the live cluster. Only the project owner records a decision here;
an agent stops when a phase needs one that is still open
([PLAN.md](PLAN.md), 1.1 and the gate before phase 4).

## Open decisions

| # | Question | Options | Needed by | Status |
|---|---|---|---|---|
| 1 | Cloud-Init snippets | 7.1 A native Cloud-Init + static vendor snippet, B shared snippet storage, C SSH helper | phase 4 | open |
| 2 | Keep the Proxmox name of the root volume (no rename) | 7.2 A yes, B rename through the helper | phase 4 | open |
| 3 | `create` on another node | 7.3 A clone + migrate, B backup storage (PBS), C Gold replica per node, D helper stream | phase 10 | open |
| 4 | Where the `refresh` journal lives | 7.4 A shared storage, B VM `description`, C helper | phase 4 | open |
| 5 | Guest verification without `VM.GuestAgent.Unrestricted` (`Audit` + `FileRead`) | yes / no | phase 4 | open |
| 6 | LUKS passphrase for `resize` | stays in the client / sent to the server for `input-data` | phase 7 | open |
| 7 | Setup and repo stay in the client in the first stage (7.5 A) | yes / no | phase 14 | open |
| 8 | Client authentication to the server | bearer token / plus hardware-key signature for destructive operations | not set by the plan | open |
| 9 | Gold in its own pool with narrower privileges | yes / no | not set by the plan | open |

## Probe results

| Date | Probe | Result |
|---|---|---|
| 2026-10-12 | PVE version | 9.2.21 (`repoid 4f6e0ac86f9e8c7f`) on all three nodes |
| 2026-10-12 | Pools | none exist yet (neither `homestack` nor `homestack-test`) |
| 2026-10-12 | `import-from` variants (phase 6) | not run yet |
