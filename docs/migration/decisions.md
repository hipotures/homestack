# Migration decisions

Decisions from section 10 of [api-mapping.md](api-mapping.md) and the results
of probes on the live cluster. Only the project owner records a decision here;
an agent stops when a phase needs one that is still open
([PLAN.md](PLAN.md), 1.1 and the gate before phase 4).

## Decisions

| # | Question | Options | Needed by | Status |
|---|---|---|---|---|
| 1 | Cloud-Init snippets | 7.1 A native Cloud-Init + static vendor snippet, B shared snippet storage, C SSH helper | phase 4 | open |
| 2 | Keep the Proxmox name of the root volume (no rename) | 7.2 A yes, B rename through the helper | phase 4 | open |
| 3 | `create` on another node | 7.3 A clone + migrate, B backup storage (PBS), C Gold replica per node, D helper stream | phase 10 | open |
| 4 | Where the `refresh` journal lives | 7.4 A shared storage, B VM `description`, C helper | phase 4 | open |
| 5 | Guest verification without `VM.GuestAgent.Unrestricted` (`Audit` + `FileRead`) | yes / no | phase 3 | **decided 2026-10-12: yes.** D2 uses the `get-*` agent commands and `file-read`; `exec` (`VM.GuestAgent.Unrestricted`) stays only for `resize` and the Gold guest contract (E2) |
| 6 | LUKS passphrase for `resize` | stays in the client / sent to the server for `input-data` | phase 7 | open |
| 7 | Setup and repo stay in the client in the first stage (7.5 A) | yes / no | phase 14 | open |
| 8 | Client authentication to the server | bearer token / plus hardware-key signature for destructive operations | not set by the plan | open |
| 9 | Gold in its own pool with narrower privileges | yes / no | not set by the plan | open |

## Probe results

| Date | Probe | Result |
|---|---|---|
| 2026-10-12 | PVE version | 9.2.21 (`repoid 4f6e0ac86f9e8c7f`) on all three nodes |
| 2026-10-12 | Pools | none exist yet (neither `homestack` nor `homestack-test`) |
| 2026-10-12 | Test resources for phase 3 (created with the owner's approval) | pool `homestack-test` (empty; VMIDs 240–249 free); roles `HomeStackTest` (the privileges of `hs-core` capabilities), `HomeStackTestGold` (`VM.Audit`, `VM.Clone`), `HomeStackTestStorage` (`Datastore.AllocateSpace`, `Datastore.Audit`); token `homestack@pve!test` (privsep) with `PVEAuditor` on `/`, `HomeStackTest` on `/pool/homestack-test`, `HomeStackTestGold` on `/vms/101`, `HomeStackTestStorage` on `/storage/local` and `/storage/local-lvm`, `PVESDNUser` on `/sdn/zones/localnetwork/vmbr0`; the user `homestack@pve` has the same roles plus `PVEAuditor` on each of those paths. Secret in `~/.config/homestack/pve-token-test` (0600) |
| 2026-10-12 | Local DNS for workspaces | master zone `hs.efez.net` on the Synology router (DNS Server, `192.168.100.1`); updates limited to TSIG key `homestack` (HMAC-SHA512, secret in `~/.config/homestack/dns-tsig.key`, 0600). `nsupdate -k` added and removed `hs-test.hs.efez.net`; both router addresses served it; an update without the key was `REFUSED`. Public `efez.net` (OVH) is untouched. No phase uses it yet |
| 2026-10-12 | Node names in local DNS | master zones `pve1.efez.net`, `pve2.efez.net`, `pve3.efez.net` on the router's DNS Server, each with an apex A record (`192.168.100.10/11/12`); both router addresses answer and TLS to `https://pveX.efez.net:8006` verifies with the answers. A Synology zone created without "Master DNS server" has no NS record and fails with `SERVFAIL` |
| 2026-10-12 | Guest agent at audit level (phase 2) | On all running HomeStack VMs (qemu-ga 10.2), `get-fsinfo` reports the disk serial as `0QEMU_QEMU_HARDDISK_drive-<slot>`, not the `serial=` of the VM config, and has no filesystem label. `total-bytes-privileged`, `used-bytes` and `total-bytes − used-bytes` equal `df`'s size, used and avail for `/home/user` on VMs 200, 204, 206, 207 and 210 (201 changed between the reads) |
| 2026-10-12 | Listing VM volumes (phase 2) | `GET /nodes/{n}/storage/{s}/content` returns nothing to a PVEAuditor token: `PVE::Storage::check_volume_access` requires `Datastore.Audit` on the storage and `VM.Config.Disk` on the owner VM, or `Datastore.Allocate` on the storage. The orphan scan therefore needs `VM.Config.Disk` on `/vms` |
| 2026-10-12 | Server token may list VM volumes (owner's decision after phase 2) | role `HomeStackServerDisks` (`VM.Config.Disk`); `/vms`: `PVEAuditor` and `HomeStackServerDisks` for `homestack@pve` and `homestack@pve!server`; `/vms/101`: `HomeStackServerDisks` added for the user. The server token now has `VM.Config.Disk` on every VMID (existing or not) and still only audit privileges elsewhere; the test token is unchanged. 28 volumes visible on pve2 `local-lvm` |
| 2026-10-12 | ACL inheritance | a role for a user on a specific path replaces what the user inherits from `/` there, and a privilege-separated token gets the intersection with its user: giving `homestack@pve` only the test roles took `PVEAuditor` away from `homestack@pve!server` on those paths until `PVEAuditor` was added for the user on each of them |
| 2026-10-12 | `import-from` variants (phase 6) | not run yet |
| 2026-10-12 | `GET /access/permissions?path=P` (phase 1) | answers for any path, existing or not, with the privileges inherited from parents (e.g. `/` → `/vms`), as `{"data":{"P":{"Priv":1,...}}}`. A privilege-separated token may query its own permissions with no extra privilege. The Administrator role has 47 privileges on PVE 9.2.21 |
| 2026-10-12 | Server token may run commands in Gold (owner's decision before phase 3, for E2) | role `HomeStackServerGoldGuest` (`VM.GuestAgent.Unrestricted`, `VM.GuestAgent.FileRead`); `/vms/101`: added for `homestack@pve`; for `homestack@pve!server` `PVEAuditor`, `HomeStackServerDisks` and `HomeStackServerGoldGuest` (a token ACL on `/vms/101` replaces what the token inherits from `/vms`). The server token ran `hostname` through `agent/exec` and read `/etc/hostname` through `agent/file-read` on Gold; `file-read` on VM 200 is refused (`VM.GuestAgent.FileRead|VM.GuestAgent.Unrestricted`). The test token is unchanged (`VM.Audit`, `VM.Clone` on Gold) |
