# Running homestack-server

Phase 0 runs one instance. Configuration comes from environment variables
until phase 1 introduces `server.toml`.

| Variable | Meaning |
|---|---|
| `HOMESTACK_LISTEN` | Listen address, default `127.0.0.1:8080` (`0.0.0.0:8080` in the image) |
| `HOMESTACK_API_TOKEN_FILE` | Client bearer token, at least 32 printable characters (required) |
| `HOMESTACK_PVE_ENDPOINTS` | Comma-separated Proxmox API origins, tried in order (required) |
| `HOMESTACK_PVE_TOKEN_FILE` | `USER@REALM!TOKENID=SECRET` (required) |
| `HOMESTACK_PVE_CA_FILE` | Extra PEM CA for Proxmox, in addition to the system roots |
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

## Docker

```
cd rust
docker build -f Containerfile -t homestack-server .
openssl rand -hex 32 > api-token && chmod 600 api-token
docker run -d --name homestack --user "$(id -u):$(id -g)" -p 127.0.0.1:8080:8080 \
    -v "$PWD/api-token:/run/secrets/api-token:ro" \
    -v "$HOME/.config/homestack/pve-token:/run/secrets/pve-token:ro" \
    -e HOMESTACK_API_TOKEN_FILE=/run/secrets/api-token \
    -e HOMESTACK_PVE_TOKEN_FILE=/run/secrets/pve-token \
    -e HOMESTACK_PVE_ENDPOINTS=https://pve1.example:8006,https://pve2.example:8006 \
    homestack-server
curl -H "Authorization: Bearer $(cat api-token)" http://127.0.0.1:8080/v1/pve/version
```

`--user` must match the owner of the mounted secret files.

## LXC

Build the binary (`cargo build --release -p hs-server`) or copy it out of the
image, install it as `/usr/local/bin/homestack-server` in a Debian 13
container with `ca-certificates`, and run it as an unprivileged user with the
variables above, e.g. from a systemd unit with `EnvironmentFile=` and the
secrets in `/etc/homestack/` (mode `0600`). The server speaks plain HTTP;
put TLS in front of it (Caddy, as in mypowers) before exposing it beyond
loopback. That setup is phase 11.

## Endpoints

| Method and path | Auth | Purpose |
|---|---|---|
| `GET /health/live` | none | `{"status":"ok"}` when the HTTP server answers; does not contact Proxmox |
| `GET /v1/pve/version` | bearer | `GET /version` from the first reachable endpoint |
| `GET /v1/pve/nodes/{node}/status` | bearer | `pveversion`, `kversion`, `uptime` of a node |

The contract is [docs/openapi.json](../openapi.json). Errors:

```json
{"schema_version":1,"error":{"code":"pve_unavailable","message":"No Proxmox API endpoint is reachable.","retryable":true,"request_id":"..."}}
```

| HTTP | Codes |
|---|---|
| 401 | `unauthorized` |
| 404, 405 | `not_found`, `method_not_allowed` |
| 422 | `validation_failed` |
| 502 | `pve_auth_failed`, `pve_permission_denied`, `pve_error` |
| 503 | `pve_unavailable`, `pve_node_unavailable` (retryable) |
| 500 | `internal` |

Every response carries `x-request-id` (also in the error body) and
`Cache-Control: no-store`.
