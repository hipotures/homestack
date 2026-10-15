# Running homestack-server

One instance per Proxmox node, made by `homestack-deploy` through the
Proxmox API only (see [Deployment](#deployment-homestack-deploy)); a single
instance also runs in Docker or by hand. Environment variables say how the
instance runs and reaches Proxmox; what HomeStack does is in
`server.toml`, uploaded through the API (see [Configuration](#configuration)).

| Variable | Meaning |
|---|---|
| `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` (`0.0.0.0:8080` in the Docker image) |
| `HOMESTACK_API_TOKEN_FILE` | Client bearer token, at least 32 printable characters |
| `HOMESTACK_API_TOKEN_SHA256` | Or the SHA-256 (hex) of that token, so the environment holds no secret; one of the two is required |
| `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
| `HOMESTACK_PVE_TOKEN_FILE` | `USER@REALM!TOKENID=SECRET`; without it the token is sent through `PUT /v1/secrets/pve-token` |
| `HOMESTACK_PVE_CA_FILE` | Extra PEM CA for Proxmox, in addition to the system roots |
| `HOMESTACK_STATE_DIR` | State directory, default `/var/lib/homestack`: `server.toml`, `secrets/`, `tls/` |
| `HOMESTACK_CONFIG_FILE` | Stored `server.toml`, default `server.toml` in the state directory |
| `HOMESTACK_INSTANCE` | The instance's name (`api1`), written into the leases it takes, default `homestack` |
| `HOMESTACK_TLS_CERT_B64`, `HOMESTACK_TLS_KEY_B64` | Certificate chain and key (PEM, base64), stored in `tls/` at the first start only |
| `HOMESTACK_CADDY_ADMIN` | Caddy's admin API, told to load a certificate sent with `PUT /v1/secrets/tls` |
| `HOMESTACK_LOG` | Log filter, default `info` |
| `HOMESTACK_LOG_FORMAT` | `text` or `json` (`json` in the images) |

Use the names on the nodes' certificates in `HOMESTACK_PVE_ENDPOINTS`, not IP
addresses: TLS is verified, and an endpoint whose certificate does not match is
treated as unreachable. If those names resolve only through the host's
`/etc/hosts`, give the container the same entries (`--add-host`, or
`/etc/hosts` in an LXC).

Secret files should be mode `0600` and owned by the user the server runs as;
the server warns about files other users can read. Secrets never appear in
logs or responses.

## Configuration

`server.toml` is the server's part of the Python `config.toml`: `[node]`,
`[storage_layouts]`, `[root]`, `[home]`, `[network]`, `[cloud_init]`,
`[user]` and `[repo] owner`, with the same names and the same validation
(`crates/hs-model/src/config.rs`). Unknown keys are errors, and values need
the types Python's own writer produces. It holds no secrets.

Make it, and the desktop's `client.toml`, from an existing configuration:

```
cargo run -p hs-config -- split            # reads ~/.config/homestack/config.toml
cargo run -p hs-config -- check ~/.config/homestack/server.toml
```

`split` validates the whole file like `config.load_config`, writes both files
next to it (mode 0600, never over an existing file) and names the tables it
did not carry over (`control`, `transport`, and anything Python ignores).

Upload it; the first time with `If-None-Match: *`, later with the current
version in `If-Match`:

```
auth="Authorization: Bearer $(cat api-token)"
curl -sS -X PUT -H "$auth" -H 'If-None-Match: *' -H 'Content-Type: application/toml' \
    --data-binary @server.toml http://127.0.0.1:8080/v1/config
version=$(curl -sS -H "$auth" http://127.0.0.1:8080/v1/config/version | jq -r .config_version)
curl -sS -X PUT -H "$auth" -H "If-Match: \"$version\"" -H 'Content-Type: application/toml' \
    --data-binary @server.toml http://127.0.0.1:8080/v1/config
```

The version is the SHA-256 of the text. The server validates before writing,
replaces the file atomically (mode 0600, directory 0700) and keeps the
previous one as `server.toml.backup-<hash>`. It reads the file only at
start, and an invalid file stops the start, so edit it through the API.
Requests that change state (from phase 3) must send the version they expect
in `HomeStack-Config-Version`; another version gets `409
config_version_mismatch`.

`GET /v1/capabilities` asks Proxmox which privileges the token holds on the
ACL paths the configuration implies (`/nodes`, `/vms`, `/vms/<gold_vmid>`,
`/storage/<id>`) and lists per operation what is required and what is
missing. The table is in `crates/hs-core/src/capabilities.rs`.

## Proxmox objects: `deploy/pve/homestack-pve.sh`

Everything HomeStack needs in Proxmox that the server's API cannot make
itself comes from one script, run by the cluster's owner as root on any node
(phase 4b). It reaches the other nodes of a cluster itself (`pvesh get
/cluster/status`, then `ssh root@<node address>` with the cluster's keys).

```
scp deploy/pve/homestack-pve.sh root@NODE:/root/
ssh root@NODE /root/homestack-pve.sh status  --storages worker-roots,local-lvm --snippet-storage local --service-storages local-zfs,local-lvm --template-storage local
ssh root@NODE /root/homestack-pve.sh install --storages worker-roots,local-lvm --snippet-storage local --service-storages local-zfs,local-lvm --template-storage local --dry-run
ssh root@NODE /root/homestack-pve.sh install --storages worker-roots,local-lvm --snippet-storage local --service-storages local-zfs,local-lvm --template-storage local
```

`--storages` are the storages of `[storage_layouts]`, `--snippet-storage`
is `[cloud_init] snippet_storage`. `--service-storages` hold the root disks of the
server containers (content `rootdir`) and `--template-storage` their image
(content `vztmpl`) ([Deployment](#deployment-homestack-deploy)). Gold is the one VM tagged
`homestack-gold`; with several, pass `--gold VMID`. `install` and
`uninstall` show the plan and ask before changing anything (`--yes` does
not ask, `--dry-run` only shows). `--test` works on the test part instead of
the production part. A second `install` changes nothing.

| Part | What | `install` | `install --test` | `uninstall` | `uninstall --test` |
|---|---|---|---|---|---|
| shared | user `homestack@pve`, roles `HomeStack`, `HomeStackGold`, `HomeStackStorage`, pool `homestack-gold` (with Gold for `install`), the vendor snippets on every node | yes | yes | removed | kept |
| deploy | roles `HomeStackDeploy`, `HomeStackDeployNodes`, `HomeStackDeployTemplates`, pool `homestack-service`, token `homestack@pve!deploy`, its ACL | yes | yes | removed (a pool with containers is kept) | kept |
| production | pools `homestack` and `homestack-test`, token `homestack@pve!server`, its ACL | yes | untouched | removed | kept |
| test | pool `homestack-test` with the test part's own Gold, token `homestack@pve!test`, its ACL | untouched | yes | removed | removed |

A new token's secret is written once to `/root/homestack-pve-token-<id>`
(mode 0600, `--token-dir` changes the directory) and never printed. Each
token gets:

| Path | Role |
|---|---|
| `/nodes`, `/storage`, `/sdn` | `PVEAuditor` (nodes, storages, networks; no VM) |
| `/pool/homestack` (server), `/pool/homestack-test` (both) | `HomeStack`: `Pool.Audit`, `VM.Allocate`, `VM.Audit`, `VM.Config.Cloudinit`, `VM.Config.Disk`, `VM.Config.Options`, `VM.GuestAgent.Audit`, `VM.GuestAgent.FileRead`, `VM.GuestAgent.Unrestricted`, `VM.Migrate`, `VM.PowerMgmt` |
| `/pool/homestack-gold` (server), `/pool/homestack-test` (both) | `HomeStackGold`: `Pool.Audit`, `VM.Audit`, `VM.Clone`, `VM.GuestAgent.Audit`, `VM.GuestAgent.FileRead`, `VM.GuestAgent.Unrestricted` |
| `/storage/<id>`, each layout storage and the snippet storage | `HomeStackStorage`: `Datastore.AllocateSpace`, `Datastore.Audit` |
| `/sdn/zones/<zone>/<bridge>` of Gold's `net0` | `PVESDNUser` |

The server's token (the panel) also works in the test pool, so the panel
shows and makes test workspaces. The test token works only in the test
pool: the test part's Gold is cloned into it by hand (keeping the tag
`homestack-gold`), so it never reaches the production Gold (the owner,
2026-10-15).

The deploy token gets instead:

| Path | Role |
|---|---|
| `/nodes` | `PVEAuditor` and `HomeStackDeployNodes`: `Sys.AccessNetwork` (pulling an image) |
| `/storage`, `/sdn` | `PVEAuditor` |
| `/pool/homestack-service` | `HomeStackDeploy`: `Pool.Audit`, `VM.Allocate`, `VM.Audit`, `VM.Config.CPU`, `VM.Config.Disk`, `VM.Config.Memory`, `VM.Config.Network`, `VM.Config.Options`, `VM.PowerMgmt` |
| `/storage/<template storage>` | `HomeStackDeployTemplates`: `Datastore.AllocateSpace`, `Datastore.AllocateTemplate`, `Datastore.Audit` |
| `/storage/<id>`, each service storage | `HomeStackStorage` |
| `/sdn/zones/<zone>/<bridge>` of Gold's `net0` | `PVESDNUser` |

It sees and changes only the containers of `homestack-service`. It cannot
remove an image from the template storage (that needs `Datastore.Allocate`,
which would allow removing anything there): old images stay until root
removes them (`pvesm free`); `homestack-deploy status` lists them.

Proxmox adds the roles of a VM's pool to those of `/vms/<vmid>`, so the
token sees, reads and changes only the VMs of the HomeStack pools; any
other VM is invisible to it (owner's rule, 2026-10-13), and the server
refuses it too (`409 outside_pool`). The user `homestack@pve` gets, on each of these paths, the
union of what its tokens resolve to there (a privilege-separated token gets
the intersection with its user). No `VM.Config.Disk` on `/vms` (decision
10 = C): the detached-volume scan lists only the volumes of the pool's VMs
and says it is incomplete.

The script only checks what belongs to the cluster's owner and stops
before any change when something required is missing: PVE 9.2 or later on
every node, the storages and their content types (`images`, `snippets`),
Gold (tagged, in no other pool) and its bridge, tag rules that would keep
the token from setting `homestack-*` tags. It warns when a node's API
certificate does not verify for `<node>.<domain>` with the system's CAs
(then give the server `/etc/pve/pve-root-ca.pem` in
`HOMESTACK_PVE_CA_FILE`) or that name does not resolve. An offline or
unreachable node is reported and the rest is done; running it again
finishes the job.

`uninstall` never removes a VM, a volume or Gold (only Gold's membership in
`homestack-gold`). A pool that still holds VMs is kept and listed; a
snippet some VM's `cicustom` still names, or that cannot be checked, is
always kept: a VM does not start without it (the owner, 2026-10-15; there
is no `--force`).

The objects made by hand in phases 0–4 (roles `HomeStackServerDisks`,
`HomeStackServerGoldGuest`, `HomeStackTest*`, the ACL on `/vms` and
`/vms/101`) are taken over: they disappear once no ACL entry uses them.

The server's instance settings name the pools:

| Variable | Meaning |
|---|---|
| `HOMESTACK_PVE_POOL` | pool the instance changes workspaces in, default `homestack` (`homestack-test` for a test instance) |
| `HOMESTACK_PVE_GOLD_POOL` | Gold's pool, default `homestack-gold` |

`GET /v1/capabilities` checks the privileges on these pools and reports in
`installation` whether both pools exist and Gold is in its pool; the
snippets it cannot see (`unknown`), `homestack-pve.sh status` checks them.

## Cloud-Init vendor snippets

Workspaces made by the server use Proxmox's own Cloud-Init (decision 1 =
A): Proxmox writes user-data, network-config and meta-data from the VM's
options (`ciuser=root`, `sshkeys`, `ipconfig0`, `nameserver`,
`ciupgrade=0`), and `cicustom` names only a vendor snippet.
`homestack-pve.sh install` writes both snippets into the snippet storage's
directory on every node it is enabled on (once for a shared storage) and
rewrites them only when their checksum differs. They are generated from
`crates/hs-core/src/cloudinit_vendor.yaml` into `deploy/cloud-init/` and
embedded in the script (`deploy/pve/embed-snippets.sh`); tests keep all
three equal. `homestack-vendor-create.yaml` formats a blank new home,
`homestack-vendor-keep.yaml` refuses to (refresh, addkey). Both end with a
key step: on every new Cloud-Init instance the keys of `sshkeys` that
root's or the user's `authorized_keys` lack are appended (addkey, phase 9).
After the snippets change, run `install` again on one node: it rewrites
them on every node.

Setting `cicustom` needs `VM.Config.Cloudinit` on the VM and
`Datastore.AllocateSpace` on the snippet storage. Checking that a snippet is
installed needs `Datastore.Allocate` on that storage, which the server does
not ask for: a missing snippet fails the VM's start.

The snippet expects of Gold: exactly one regular account (UID 1000–65533),
whose home is `/home/<name>`; NetworkManager (it selects NetworkManager as
netplan's renderer, which Python's network snippet did per workspace), and
`netplan`, `udevadm`, `python3` with PyYAML (cloud-init's own) besides the
tools of the Gold guest contract. Gold readiness checks them ("Cloud-Init
vendor snippet tools").

## Docker

```
docker build -f Containerfile -t homestack-server .   # from the repository root
openssl rand -hex 32 > api-token && chmod 600 api-token
docker volume create homestack-state
docker run -d --name homestack --user 65534:65534 -p 127.0.0.1:8080:8080 \
    -v homestack-state:/var/lib/homestack \
    -v "$PWD/api-token:/run/secrets/api-token:ro" \
    -v "$HOME/.config/homestack/pve-token:/run/secrets/pve-token:ro" \
    -e HOMESTACK_API_TOKEN_FILE=/run/secrets/api-token \
    -e HOMESTACK_PVE_TOKEN_FILE=/run/secrets/pve-token \
    -e HOMESTACK_PVE_ENDPOINTS=https://pve1.example:8006,https://pve2.example:8006 \
    homestack-server
curl -H "Authorization: Bearer $(cat api-token)" http://127.0.0.1:8080/v1/pve/version
```

`--user` must be able to read the secret files and write
`/var/lib/homestack`. A new named volume takes the image's directory
(owner 65534, mode 0700); with a bind mount, use a directory owned by the
`--user`. Without a volume the configuration is lost with the container.

## Deployment: `homestack-deploy`

One instance per Proxmox node, each a container made from the public node
image `ghcr.io/hipotures/homestack-server:<version>-<commit>` (CI publishes
it from `Containerfile`, target `node`), only through the Proxmox API with
the deploy token (owner's decisions of 2026-10-13, phase 11). Nothing runs
on the nodes as root, no SSH.

In the image, `runsvdir` starts two services (`deploy/lxc`): the server on
`127.0.0.1:8080` as the user `homestack`, and Caddy on `:443` with the
certificate the server keeps in `/var/lib/homestack/tls`. The server speaks
plain HTTP only to Caddy.

What it needs, once:

1. `homestack-pve.sh install` as above (pool `homestack-service`, the deploy
   token's secret in `/root/homestack-pve-token-deploy`; copy it to the
   desktop, mode `0600`).
2. A DNS name per instance in a domain whose public zone is at OVH
   (`api1.hs.efez.net` → `192.168.100.97`, ...); the names may resolve only
   locally. The OVH API key of Proxmox's ACME plugin (`OVH_AK`, `OVH_AS`,
   `OVH_CK`) in a file, mode `0600`.
3. A client token: `openssl rand -hex 32 > api-token`, mode `0600`.
4. `deploy.toml`, written by `homestack install` (phase 13c, section 8), or by hand (example: `crates/hs-deploy/tests/data/live-deploy.toml`):
   the image, `domain`, the network (`bridge`, `cidr`, `gateway`,
   `nameserver`), `[pve]` endpoints and the deploy token, `[server]` the
   instances' Proxmox token, pools, `server.toml` and the client token,
   `[acme]` the OVH key, the zone and a directory for the certificate, and
   one `[[instances]]` per node (`name`, `node`, `ctid`, `address`,
   `storage`).

```
cargo run -p hs-deploy -- --config deploy.toml cert      # *.domain from Let's Encrypt (DNS-01 at OVH)
cargo run -p hs-deploy -- --config deploy.toml apply     # every instance, one at a time
cargo run -p hs-deploy -- --config deploy.toml status
cargo run -p hs-deploy -- --config deploy.toml cert --push   # renew (< 30 days left) and load it everywhere
cargo run -p hs-deploy -- --config deploy.toml remove --yes
```

`cert` keeps the ACME account, the certificate and its key in `[acme] dir`
(mode `0600`); the OVH key never leaves the desktop. It adds the TXT record
`_acme-challenge.<domain>` at OVH, waits until OVH's name servers serve it,
and removes it afterwards.

`apply`, for each instance in turn (the others keep serving):

1. pulls the image into the template storage of the node unless it is
   there (`POST /nodes/{node}/storage/{storage}/oci-registry-pull`);
2. stops and destroys the instance's container if it exists;
3. creates it (`POST /nodes/{node}/lxc`: unprivileged, in
   `homestack-service`, the static address, `onboot`), and sets its
   environment: `HOMESTACK_INSTANCE`, `HOMESTACK_API_TOKEN_SHA256`, the
   Proxmox endpoints and pools, and the certificate
   (`HOMESTACK_TLS_CERT_B64`, `HOMESTACK_TLS_KEY_B64`);
4. starts it and waits until it answers over TLS by its name at its
   address;
5. removes the certificate from the container's configuration (Proxmox
   applies a change of a running container's environment at its next
   start, so the container is restarted) and checks it is gone;
6. sends the Proxmox token (`PUT /v1/secrets/pve-token`) and `server.toml`
   (`PUT /v1/config`);

then checks that every instance reports the same `GET /v1/config/version`.
Only the deploy token and root see a container's configuration while the
certificate is in it.

A client (`crates/hs-client`) lists every instance. A read goes to the
first instance that answers, starting from the one that answered last; a
plan, its apply and its job stay on the instance that made the plan, and
if that instance stops answering the client says so instead of repeating
the request elsewhere (mapping 8.3).

### Several instances on one VM

Before a job changes a VM, its instance writes a lease into the VM's
`description` with the config's `digest`:
`<!-- homestack-lease: <base64 JSON> -->` (instance, job, operation, times).
Another instance's lease refuses planning and applying with `409 busy`
naming the holder; the lease is removed when the job ends. It is not
renewed while the job runs (a renewal would change the `digest` a step
writes with) and lasts 2 hours; an instance that restarted takes over its
own lease of a job it no longer runs. Every operation that changes a VM
therefore needs `VM.Config.Options` on the pool (role `HomeStack` has it).

## A container by hand

The binary is static (`x86_64-unknown-linux-musl`, `crt-static`) and runs on
any Linux. Copy it out of an image (`docker create`, `docker cp
<container>:/usr/local/bin/homestack-server .`), run it as an unprivileged
user with the variables above, the secrets in files (mode `0600`) and
`/var/lib/homestack` owned by that user (mode `0700`), and put TLS in front
of it (`deploy/lxc/Caddyfile`) before exposing it beyond loopback.

## Endpoints

| Method and path | Auth | Purpose |
|---|---|---|
| `GET /health/live` | none | `{"status":"ok"}` when the HTTP server answers; does not contact Proxmox |
| `GET /v1/pve/version` | bearer | `GET /version` from the first reachable endpoint |
| `GET /v1/pve/nodes/{node}/status` | bearer | `pveversion`, `kversion`, `uptime` of a node |
| `GET /v1/config` | bearer | the stored `server.toml` (`application/toml`, `ETag`) |
| `PUT /v1/config` | bearer | replace it (`If-Match` / `If-None-Match: *`) |
| `GET /v1/config/version` | bearer | `{"config_version": "<sha256>" \| null}` |
| `PUT /v1/secrets/pve-token` | bearer | the Proxmox token (`text/plain`), used at once and kept in `secrets/`; write-only |
| `PUT /v1/secrets/tls` | bearer | `{"cert", "key"}` (PEM): Caddy's certificate, loaded at once; write-only |
| `GET /v1/capabilities` | bearer | operations the Proxmox token allows, with missing privileges |
| `GET /v1/status` | bearer | global status, as `homestack status --json` |
| `GET /v1/workspaces/{vmid\|name}` | bearer | one VM, as `homestack status TARGET --json` |
| `GET /v1/storage` | bearer | capacity of every layout storage |
| `GET /v1/orphans` | bearer | detached HomeStack volumes, and whether the scan was complete |
| `GET /v1/discovery` | bearer | nodes, Gold candidates, storage definitions (no configuration needed) |
| `GET /v1/networks?gold=VMID` | bearer | network profiles for the Gold bridge |
| `GET /v1/gold/readiness?vmid=VMID` | bearer | Gold checks: Proxmox part and, when Gold runs, the guest contract |
| `POST /v1/plans` | bearer | plan an operation (`{"operation", "parameters"}`); nothing changes |
| `GET /v1/plans/{id}` | bearer | a plan until it expires (15 minutes) |
| `POST /v1/plans/{id}/apply` | bearer | `{"digest"}`: plan again, compare, start a job (`202`, `Location`) |
| `GET /v1/jobs`, `GET /v1/jobs/{id}` | bearer | jobs of this instance (steps, Proxmox tasks, log, result or error) |
| `GET /v1/jobs/{id}/events` | bearer | WebSocket: `snapshot`, `job` after each change, `heartbeat` at least every 5 s |

The status routes read guests with the audit-level agent commands
(`VM.GuestAgent.Audit`); the guest verification of later operations adds
`file-read` (`VM.GuestAgent.FileRead`, decision 5), and only the Gold guest
contract runs commands (`VM.GuestAgent.Unrestricted` on Gold). Listing VM volumes, which the detached-volume scan
needs, takes `VM.Config.Disk` on the VMs (or `Datastore.Allocate` on each
storage): with `VM.Config.Disk` only on the pool (decision 10 = C) the scan
is marked incomplete, with a warning saying so.

The contract is [docs/openapi.json](../openapi.json). Errors:

```json
{"schema_version":1,"error":{"code":"pve_unavailable","message":"No Proxmox API endpoint is reachable.","retryable":true,"request_id":"..."}}
```

| HTTP | Codes |
|---|---|
| 401 | `unauthorized` |
| 404, 405 | `not_found`, `method_not_allowed` |
| 409 | `config_missing`, `config_version_mismatch`, `plan_changed`, `plan_applied`, `busy` (retryable), `outside_pool`, `operation_refused` |
| 412, 428 | `precondition_failed`, `precondition_required` |
| 413 | `payload_too_large` |
| 422 | `validation_failed`, `config_invalid` (with `error.field`, e.g. `repo.owner`) |
| 502 | `pve_auth_failed`, `pve_permission_denied`, `pve_error` |
| 503 | `pve_unavailable`, `pve_node_unavailable` (retryable), `pve_token_missing` |
| 500 | `internal` |

Every response carries `x-request-id` (also in the error body) and
`Cache-Control: no-store`.

Operations (phase 5): `workspace.create` with `{"vmid", "name",
"home_size"?, "storage"?}` clones Gold into the instance's pool, gives it a
home and native Cloud-Init, starts and verifies it; `workspace.destroy` with
`{"vmid"}` shuts down and destroys a workspace of the pool. Their job
result carries the event (`workspace.created`, `workspace.destroyed`) and
what the client needs locally (VMID, name, IP, user, `host_key_changed`).
Applying a plan needs `HomeStack-Config-Version` once a configuration is
stored. A VM outside the pool is refused with `409 outside_pool`, a state
that does not allow the operation with `409 operation_refused`. Plans and jobs are kept in memory and lost
on restart; a job runs on the server, independent of the request or stream
that follows it, and one job at a time works on a VM, across instances too
(its lease, [Several instances on one VM](#several-instances-on-one-vm)).
