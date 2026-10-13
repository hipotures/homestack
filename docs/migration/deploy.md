# Running homestack-server

One instance so far (several from phase 11). Environment variables say how
the instance runs and reaches Proxmox; what HomeStack does is in
`server.toml`, uploaded through the API (see [Configuration](#configuration)).

| Variable | Meaning |
|---|---|
| `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` (`0.0.0.0:8080` in the image) |
| `HOMESTACK_API_TOKEN_FILE` | Client bearer token, at least 32 printable characters (required) |
| `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
| `HOMESTACK_PVE_TOKEN_FILE` | `USER@REALM!TOKENID=SECRET` (required) |
| `HOMESTACK_PVE_CA_FILE` | Extra PEM CA for Proxmox, in addition to the system roots |
| `HOMESTACK_CONFIG_FILE` | Stored `server.toml`, default `/var/lib/homestack/server.toml` |
| `HOMESTACK_LOG` | Log filter, default `info` |
| `HOMESTACK_LOG_FORMAT` | `text` or `json` (`json` in the image) |

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

## Proxmox token

Phases 0–2 only read. The token used so far (created on PVE 9.2.21):

```
pveum user add homestack@pve --comment 'HomeStack server'
pveum acl modify / --users homestack@pve --roles PVEAuditor
pveum user token add homestack@pve server --privsep 1 --comment 'HomeStack server, read-only'
pveum acl modify / --tokens 'homestack@pve!server' --roles PVEAuditor
```

`PVEAuditor` holds `Datastore.Audit`, `Mapping.Audit`, `Pool.Audit`,
`SDN.Audit`, `Sys.Audit`, `VM.Audit` and `VM.GuestAgent.Audit`. The token's
secret is shown once; write it straight to a file.

The detached-volume scan also needs `VM.Config.Disk` on `/vms` (Proxmox lists
a VM volume only to who may change that VM's disks). Give it together with
`PVEAuditor` on `/vms`: a role on a more specific path replaces what the
user inherits from `/`, and a privilege-separated token gets the
intersection with its user.

```
pveum role add HomeStackServerDisks --privs VM.Config.Disk
pveum acl modify /vms --users homestack@pve --tokens 'homestack@pve!server' --roles PVEAuditor,HomeStackServerDisks
```

The Gold guest contract (from phase 3) runs commands in Gold through the
guest agent, which needs `VM.GuestAgent.Unrestricted` on Gold only (VMID
101 here). Workspaces are verified with `VM.GuestAgent.Audit` and
`VM.GuestAgent.FileRead` (decision 5). The token's roles on `/vms/101`
replace what it inherits from `/vms`, so all three are listed:

```
pveum role add HomeStackServerGoldGuest --privs VM.GuestAgent.Unrestricted,VM.GuestAgent.FileRead
pveum acl modify /vms/101 --users homestack@pve --roles HomeStackServerGoldGuest
pveum acl modify /vms/101 --tokens 'homestack@pve!server' --roles PVEAuditor,HomeStackServerDisks,HomeStackServerGoldGuest
```

## Cloud-Init vendor snippets

Workspaces made by the server use Proxmox's own Cloud-Init (decision 1 =
A): Proxmox writes user-data, network-config and meta-data from the VM's
options (`ciuser=root`, `sshkeys`, `ipconfig0`, `nameserver`,
`ciupgrade=0`), and `cicustom` names only a vendor snippet. The API cannot
write files to a node, so the two snippets are installed once on every node
that runs workspaces, by the owner, as root:

```
scp deploy/cloud-init/homestack-vendor-*.yaml root@NODE:/var/lib/vz/snippets/
ssh root@NODE chmod 644 '/var/lib/vz/snippets/homestack-vendor-*.yaml'
```

The directory is that of `[cloud_init] snippet_storage` (`local` above),
whose content types must include `snippets`. The files are generated from
`crates/hs-core/src/cloudinit_vendor.yaml` and a test keeps them equal;
after a change, copy them again. `homestack-vendor-create.yaml` formats a
blank new home, `homestack-vendor-keep.yaml` refuses to (refresh, addkey).

Setting `cicustom` needs `VM.Config.Cloudinit` on the VM and
`Datastore.AllocateSpace` on the snippet storage. Checking that a snippet is
installed needs `Datastore.Allocate` on that storage, which the server does
not ask for: a missing snippet fails the VM's start.

The snippet expects of Gold: exactly one regular account (UID 1000–65533),
whose home is `/home/<name>`; NetworkManager (it selects NetworkManager as
netplan's renderer, which Python's network snippet did per workspace), and
`netplan`, `udevadm`, `python3` with PyYAML (cloud-init's own) besides the
tools of the Gold guest contract.

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

## LXC

The target is an Alpine 3.24 LXC. The image builds a static musl binary
(`x86_64-unknown-linux-musl`, `crt-static`), so it needs no runtime libraries
and also runs on other distributions. Copy it out of the image:

```
docker create --name hs-bin homestack-server
docker cp hs-bin:/usr/local/bin/homestack-server .
docker rm hs-bin
```

Install it as `/usr/local/bin/homestack-server` in the container, add
`ca-certificates` (`apk add ca-certificates`), and run it as an unprivileged
user with the variables above under OpenRC (`supervise-daemon`, as in
mypowers `deploy/openrc`), with the secrets in `/etc/homestack/` (mode
`0600`) and `/var/lib/homestack` owned by that user (mode `0700`). The server speaks plain HTTP; put TLS in front of it (Caddy, as in
mypowers) before exposing it beyond loopback. The service files are part of
phase 11.

## Endpoints

| Method and path | Auth | Purpose |
|---|---|---|
| `GET /health/live` | none | `{"status":"ok"}` when the HTTP server answers; does not contact Proxmox |
| `GET /v1/pve/version` | bearer | `GET /version` from the first reachable endpoint |
| `GET /v1/pve/nodes/{node}/status` | bearer | `pveversion`, `kversion`, `uptime` of a node |
| `GET /v1/config` | bearer | the stored `server.toml` (`application/toml`, `ETag`) |
| `PUT /v1/config` | bearer | replace it (`If-Match` / `If-None-Match: *`) |
| `GET /v1/config/version` | bearer | `{"config_version": "<sha256>" \| null}` |
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
needs, takes `VM.Config.Disk` on `/vms` (or `Datastore.Allocate` on each
storage): a read-only token gets an orphan scan marked incomplete, with a
warning saying so.

The contract is [docs/openapi.json](../openapi.json). Errors:

```json
{"schema_version":1,"error":{"code":"pve_unavailable","message":"No Proxmox API endpoint is reachable.","retryable":true,"request_id":"..."}}
```

| HTTP | Codes |
|---|---|
| 401 | `unauthorized` |
| 404, 405 | `not_found`, `method_not_allowed` |
| 409 | `config_missing`, `config_version_mismatch`, `plan_changed`, `plan_applied`, `busy` (retryable) |
| 412, 428 | `precondition_failed`, `precondition_required` |
| 413 | `payload_too_large` |
| 422 | `validation_failed`, `config_invalid` (with `error.field`, e.g. `repo.owner`) |
| 502 | `pve_auth_failed`, `pve_permission_denied`, `pve_error` |
| 503 | `pve_unavailable`, `pve_node_unavailable` (retryable) |
| 500 | `internal` |

Every response carries `x-request-id` (also in the error body) and
`Cache-Control: no-store`.

No operation can be planned yet: the model is in place (phase 3) and the
operations arrive from phase 5. Plans and jobs are kept in memory and lost
on restart; a job runs on the server, independent of the request or stream
that follows it, and one job at a time works on a VM.
