# Proxmox installer permissions

Both `deploy/pve/homestack-pve.sh` (`ROLE_PRIVS`) and the desktop installer's
`crates/hs-install/src/plan.rs` (`ROLES`) reconcile exact privilege lists.
They must agree: a manual privilege addition alone is removed by the next
installation unless it is included in these definitions. Review the generated
commands before applying a plan. Extra privileges are deliberately removed;
required privileges are retained on subsequent runs. Existing tokens are reused,
with privilege separation enabled; permission repair does not rotate secrets.

## Deploy operations and ACL paths

The desktop uses `homestack@pve!deploy` for API servers, Pangolin Site and
application containers. The reachable calls are in `hs-deploy/src/node.rs`,
`recreate.rs`, `console.rs` and `hs-cli/src/services/{deploy,update,migrate,lifecycle}.rs`.
The table groups the minimum privileges used across these operations.

| Operation | Required privilege | ACL path checked by PVE |
| --- | --- | --- |
| List pools and inspect existing containers, pending changes, snapshots and status | `Pool.Audit`, `VM.Audit` | `/pool/POOL`, `/vms/CTID` |
| Create an unprivileged LXC or temporary holder in an explicit pool; destroy a container and its attached volumes | `VM.Allocate` | Creation accepts `/pool/POOL`; existing containers use `/vms/CTID` |
| Set CPU, memory/swap, network/hostname/DNS, description/environment/options and tags | `VM.Config.CPU`, `VM.Config.Memory`, `VM.Config.Network`, `VM.Config.Options` | `/vms/CTID`, or the explicit pool during creation; tags are set after creation and pool membership |
| Allocate or resize root/home; reassign home between the service and holder | `VM.Config.Disk`, `Datastore.AllocateSpace` | `/vms/CTID` (both containers for reassignment), `/storage/STORAGE` for allocation |
| Start, stop, shut down or reboot a container | `VM.PowerMgmt` | `/vms/CTID` |
| Initialize a container through `termproxy` and its websocket | `VM.Console` | `/vms/CTID`; this allows a shell inside containers in the authorized pools |
| Restart-migrate an LXC with root/home to a selected storage | `VM.Migrate`, `VM.Config.Disk`, `Datastore.AllocateSpace` | `/vms/CTID`; disk configuration is required for the storage mapping; allocation on `/storage/TARGET` |
| Read storage status/content and use a template | `Datastore.Audit` (content also accepts `Datastore.AllocateSpace`) | `/storage/STORAGE` |
| Pull an OCI image | `Datastore.AllocateTemplate`, `Sys.AccessNetwork` | `/storage/TEMPLATE_STORAGE` and `/nodes/NODE`, respectively |
| Delete obsolete `vztmpl` archives after replacement | `Datastore.Allocate` | `/storage/TEMPLATE_STORAGE`; `AllocateTemplate` and `AllocateSpace` do not authorize deletion |
| Attach a network interface to the selected bridge | `SDN.Use` | The selected SDN vnet path, or `/sdn/zones/localnetwork/BRIDGE` for a local bridge |

The installers assign these privileges as follows:

| Role | Deploy token scope |
| --- | --- |
| `HomeStackDeploy` | `/pool/homestack-service` and `/pool/homestack-test` only |
| `HomeStackDeployNodes` (`Sys.AccessNetwork`) | `/nodes`, propagated to cluster nodes |
| `HomeStackDeployTemplates` (`Datastore.Allocate`, `Datastore.AllocateSpace`, `Datastore.AllocateTemplate`, `Datastore.Audit`) | Only `/storage/TEMPLATE_STORAGE` selected in the installer |
| `HomeStackStorage` (`Datastore.AllocateSpace`, `Datastore.Audit`) | Each explicitly selected service root/home storage, except a storage already covered by the template role |
| `PVEAuditor` | `/nodes`, `/storage`, `/sdn`; infrastructure discovery without global VM visibility |
| `PVESDNUser` | Only the selected bridge/vnet path |

`Datastore.Allocate` is broader than template deletion: PVE can authorize deletion
of other volumes and storage configuration changes on that storage too. There is
no filename-prefix ACL for `vztmpl` deletion. HomeStack filters its cleanup by image
name, but the token's authority covers the selected storage ID, including that
ID's instances on different nodes. It is never granted on `/storage` as a whole
or automatically on service disk storages. A dedicated template storage limits
this authority. If a service disk storage is also the selected template storage,
the template role necessarily covers that entire storage.

`VM.Backup` is not added. Both current UI update callers pass an empty backup
selection. The dormant `vzdump` branch in `services/update.rs` would require
`VM.Backup` on the container and `Datastore.AllocateSpace` on an explicitly
authorized backup storage before it could be exposed. No backup storage ACL is
inferred from storage discovery. Likewise, complete inventory of detached or
foreign VM volumes is not a reason to grant the server global `/vms` rights or
`Datastore.Allocate` on workspace disk storages: the server reports listing gaps.

## Server, test and installer identities

The server and test tokens operate workspace QEMU VMs with `HomeStack` on their
workspace pools, `HomeStackGold` on the appropriate Gold pool, and
`HomeStackStorage` on configured workspace/snippet storages. Their existing
`VM.Migrate` handles workspace migration; template cleanup is a desktop deploy
operation and does not require granting them `HomeStackDeployTemplates`.
`HomeStackCluster` supplies `Sys.Audit` at `/` for cluster quorum, without VM
administration privileges.

The Rust production installer also grants the server token `HomeStack` and
`HomeStackGold` on `homestack-test`, and the shared `HomeStackDeploy` role on
`homestack-service`. Consequently additions to that shared role also apply to
the server token's service-pool authority, including `VM.Console`. The shell
installer does not assign that role to the server token. These existing token
scope differences are retained; the role privilege lists now agree. The test
token receives neither deploy-template privileges nor the deploy role.

The test Gold scopes also differ intentionally: the desktop installer selects a
separate Gold in `homestack-test` and grants its test token both workspace and
Gold roles there. The standalone script uses the Gold in `homestack-gold`,
including for `--test`; its test token has `HomeStack` on `homestack-test` and
`HomeStackGold` on `homestack-gold`. Neither path gives the test token the deploy
role. The desktop's optional Gold selection and tag-based adoption are separate
from the standalone script's required Gold workflow.

Both installers check `homestack-ws`, `homestack-lock` and `homestack-server`
against `registered-tags` and `user-tag-access`, plus `homestack-test` for a test
installation. A registered or disallowed server tag must stop preflight before
any changes; `user-allow=existing` retains the warning that each tag must already
exist. No tag policy or runtime privileges are changed by these checks.

Credential filenames are specific to each entry point. The desktop writes
`pve-token` (server), `pve-token-test` and `pve-token-deploy` in its selected local
configuration directory. The standalone script writes
`homestack-pve-token-{server,test,deploy}` in `--token-dir` on the node running
it. Their credential content format agrees, but neither installer promises
interchangeable artifact paths. Existing tokens are reused without writing a
new secret file; filename parity is not a reason to rotate them.

The installer itself runs administrative `pveum`/`pvesh` commands through the
owner's root access. Creating users, roles, pools, tokens, ACLs and snippets, or
adopting existing guests into pools, is not a privilege requirement of a runtime
deploy/server/test token. Disposable E2E lab credentials are separate and do not
define installer roles.

## Inheritance and repeat installation

A pool grant authorizes its actual members through PVE's pool ACL resolution;
`/pool/POOL` is not an ancestor of `/vms/CTID`. Containers must belong to an
authorized pool, including the temporary holder. New containers pass `pool`
explicitly, allowing creation without a `/vms` grant. Moving to another node
does not change pool membership or CTID. Do not put foreign guests or storages
in HomeStack pools: pool privileges apply to their members.

ACL entries propagate explicitly (`--propagate 1`) on installer-managed paths.
Both installers detect `propagate=0` and include a repair in the install plan for
the selected token, deploy token and corresponding user grants. Retained ACLs of
other tokens are not rewritten just to change propagation. Repeat installation
with the resulting role lists and ACL flags produces no further permission
changes. Uninstall does not repair inheritance on retained entries.

With `privsep=1`, effective privileges are the intersection of the token and
`homestack@pve` privileges at the checked path. The generated user ACL is the
union needed by the configured tokens, including inherited audit roles on more
specific paths; an explicit user entry can replace inherited user roles.
Granting a role only to the token, only to the user, or at an unrelated pool,
node or storage path is insufficient. `NoAccess` can also override pool-derived
rights. PVE returns a granted privilege with value `0` when it does not propagate:
presence, rather than a value of `1`, establishes access on the exact path.

Migration review queries `GET /access/permissions?path=...` separately for the
actual `/vms/CTID` and target `/storage/STORAGE`. It accepts granted values `0`
and `1`, without treating a pool listing or an ancestor grant as proof that the
operation is authorized. A permission failure directs the owner to review the
updated installer plan and both user/token ACLs, without replacing tokens.

## Permission repair and verification

Use **Settings > General > Reinstall HomeStack** to review the desktop plan with
saved selections. For the standalone script, use `install --dry-run` with the
same `--storages`, `--snippet-storage`, `--service-storages`, `--template-storage`
and, where needed, `--gold`/`--test` selections. Apply only the reviewed plan.
Existing tokens and their secrets are retained.

An owner can inspect effective rights without retrieving a token secret:

```sh
pveum user token permissions homestack@pve deploy --path /vms/CTID
pveum user token permissions homestack@pve deploy --path /storage/TEMPLATE_STORAGE
pveum user token permissions homestack@pve deploy --path /storage/TARGET_STORAGE
pveum user token permissions homestack@pve deploy --path /nodes/TARGET_NODE
```

HS-14 verification uses local fake-PVE installer checks, temporary plan and
permission diagnostics, and a wrapped build of `homestack`. These verify plan
generation and application logic; they do not demonstrate live PVE authorization.
No production cluster role, ACL or token operation is needed for this audit.

Permission requirements were checked against the upstream implementation:
[LXC API](https://github.com/proxmox/pve-container/blob/master/src/PVE/API2/LXC.pm),
[LXC configuration checks](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC.pm),
[storage deletion](https://github.com/proxmox/pve-storage/blob/master/src/PVE/API2/Storage/Content.pm),
[OCI pull](https://github.com/proxmox/pve-storage/blob/master/src/PVE/API2/Storage/Status.pm),
[effective ACL resolution](https://github.com/proxmox/pve-access-control/blob/master/src/PVE/RPCEnvironment.pm)
and [token and inheritance documentation](https://github.com/proxmox/pve-docs/blob/master/pveum.adoc).
