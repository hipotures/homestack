# Phase 11: several instances and deployment

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** mapping 8.3 (several instances), the deployment of one instance
per node, TLS in front of the server, the client's choice of instance
**Decisions used:** the owner's decisions of 2026-10-13 for phase 11
(`decisions.md`): deployment only through the Proxmox API, a public OCI
image from ghcr.io, CTIDs 997–999, Caddy in the image with the wildcard
certificate `*.hs.efez.net` obtained on the desktop (DNS-01, OVH), the
certificate passed in the container's `env` at creation, the other secrets
through the server's API, a separate deploy token, the live gate on test
instances; decision 4 = B (the `description` holds HomeStack's blocks)

## 1. Python behaviour (step 3)

Python runs on one desktop and has no counterpart of several instances:
its only cross-process guard is the local persistent lock of `bk`
(`flock`), and `homestack-lock` (`proxmox.py`, `REFRESH_LOCK_TAG`) is a tag
the owner sets by hand to forbid `refresh`. No Python test is in the scope
of this phase (manifest: none for phase 11).

## 2. Design

### 2.1 Lock between instances (mapping 8.3)

Before a job changes a VM, its instance writes a lease into the VM's
`description`, after the journal of decision 4 and with the config's
`digest`:

```
<!-- homestack-lease: <base64 JSON> -->
```

The JSON: `instance` (`HOMESTACK_INSTANCE`, e.g. `api2`), `job`, `operation`,
`acquired_at`, `expires_at`. A lease of another instance that has not
expired refuses the job (`409 busy`, with the holder in the message); a
write that loses the race (`checksum mismatch`) is read again and refused
the same way. The lease is removed with `digest` when the job ends (the
VM's node is found again: `migrate` moves it; `destroy` removes the VM and
the lease with it).

There is no renewal while the job runs: every write of a running step
carries the `digest` its step read, and a renewal in between would fail
that step. The lease lasts 2 hours instead; a lease of the same instance
for a job the process does not know (it restarted) is stale and taken
over. The journal code keeps the rest of the `description`, the lease
included.

`create` has no VM to lease before the clone; a second clone of the same
VMID fails in Proxmox.

### 2.2 The instance in its container

Image `ghcr.io/hipotures/homestack-server:<version>-<commit>` (Alpine 3.24, `Containerfile` target `node`, public, pushed by CI): the
static server binary, Caddy and runit. The entrypoint is `runsvdir`, with
two services: the server, on `127.0.0.1:8080` as an unprivileged user, and
Caddy on `:443` with the certificate files the server keeps
(`/var/lib/homestack/tls`), started once they exist.

Environment of an instance (no secret but the certificate before the first
start):

| Variable | Meaning |
|---|---|
| `HOMESTACK_INSTANCE` | the instance's name (`api1`) |
| `HOMESTACK_API_TOKEN_SHA256` | SHA-256 (hex) of the client token, instead of `HOMESTACK_API_TOKEN_FILE` |
| `HOMESTACK_PVE_ENDPOINTS`, `HOMESTACK_PVE_POOL`, `HOMESTACK_PVE_GOLD_POOL` | as before |
| `HOMESTACK_STATE_DIR` | `/var/lib/homestack`: `server.toml`, `secrets/`, `tls/` |
| `HOMESTACK_TLS_CERT_B64`, `HOMESTACK_TLS_KEY_B64` | the certificate chain and key at creation; the server writes them to `tls/` and the deploy removes them from the container's config |

The Proxmox token comes through `PUT /v1/secrets/pve-token` (write-only,
`204`; kept `0600` in `secrets/`); until then the routes that need Proxmox
answer `503 pve_token_missing`. `PUT /v1/secrets/tls` (JSON with `cert`
and `key`) replaces the certificate and makes Caddy load it again
(renewal).

### 2.3 Deployment (`homestack-deploy`, on the desktop)

Only the Proxmox API (deploy token) and the server's API
(`crates/hs-deploy`, `deploy.md`, "Deployment"):

1. `cert`: the wildcard certificate from Let's Encrypt, DNS-01 through the
   OVH API (`_acme-challenge.hs` in zone `efez.net`, checked on OVH's name
   servers before the CA is told, removed after), kept on the desktop;
   renewed when less than 30 days are left; `--push` sends it to every
   instance (`PUT /v1/secrets/tls`).
2. `apply`, one instance at a time (the others keep serving): pull the
   image on the node (`oci-registry-pull`) unless it is there; if the
   container exists, stop and destroy it; create it (`pool
   homestack-service`, the CTID, the address, the storage, `onboot`), set
   its `env` (with the certificate), start it; wait for `GET /health/live`
   over TLS (by the instance's address with the certificate's name); remove
   the certificate from `env` (on a running container the change waits for
   a restart, so it is restarted) and check it is gone; send the Proxmox
   token and `server.toml`; check `GET /v1/config/version` of every
   instance is the same.
3. `status`: the certificate, the old images on each node, every
   instance's container and configuration version.
4. `remove`: stop and destroy the containers (the live gate).

### 2.4 Client: the first reachable instance

`hs-client`: a list of instance URLs, the client token and an extra CA
(none for the public certificate). A read goes to the first instance that
answers, starting from the one that answered last (a stopped instance then
costs its connect timeout once, not on every read); an instance whose
Caddy answers `502`–`504` without a HomeStack error document counts as
down, an error document is the instance's answer. A plan, its apply and
the job are kept on the instance that made the plan; if that instance stops
answering, the client reports `Lost` and does not repeat anything elsewhere
(mapping 8.3): the caller reads the state again and plans anew.

## 3. Tests

No Python test is in the scope of this phase; every test is new.

| Kind | Count | Where |
|---|---|---|
| Two server processes on one `pve-sim` | 1 | `crates/hs-server/tests/instances.rs`: the gate's test. Both instances plan a `refresh` of VM 240; A applies and stops at its first journal write (holding its lease); B's apply, B's plans of `refresh` and `destroy` get `409 busy` naming A, and the VM's config is unchanged; A killed and started again under its name takes its stale lease over and finishes the `refresh`; B then destroys the VM |
| An instance with its secrets through the API | 2 | the same file: the client token as SHA-256, `503 pve_token_missing`, `PUT /v1/secrets/pve-token` (`422` for a bad token, write-only, `0600`, kept across a restart), the certificate of the environment stored once (`0600`, not replaced at a restart); `PUT /v1/secrets/tls` stored and loaded by a stand-in for Caddy's admin API (`GET /config/`, `POST /load` with `must-revalidate`), and put back when Caddy refuses |
| Lease (`hs-core`) | 2 | `crates/hs-core/src/lease.rs`: the block next to the journal; who may take a VM (none, another instance's live lease, expired, the own instance's running or forgotten job) |
| Client | 3 | `crates/hs-client/tests/failover.rs`: reads skip a closed port and a proxy's `502`, start from the last answering instance, an API error is final; no instance answering; a plan and its job stay on one instance, a lost one is reported, a new plan goes elsewhere |
| Deploy | 10 | `crates/hs-deploy`: `deploy.toml` (defaults, paths, refusals), OVH credentials and the signature (a known SHA-1), the image's file name, the environment kept and changed, and the container steps on a stand-in for the Proxmox API (`tests/node.rs`: pull once, stop then destroy with volumes, create and `env`, old images) |
| Installer | 1 new, 11 changed | `deploy/pve/tests/run.sh`: the deploy part in every install and uninstall, its storage checks, a service pool with containers kept |
| Changed | 3 | `jobs.rs` (its token needs `VM.Config.Options` for the lease), the capability table (`VM.Config.Options` for every operation that changes a VM), the OpenAPI document |
| Live | 1 | `crates/hs-deploy/tests/live_deploy.rs` in `check.sh --live` |

Manifest unchanged: 593 Python tests, 136 ported, 380 todo, 77 excluded.

## 4. Live cluster (L4)

Done on 2026-10-13, before the gate:

- `homestack-pve.sh install --test` on pve2 (the plan only added the deploy
  part: three roles, pool `homestack-service`, token `deploy`, its ACL).
- The probe of `decisions.md` (an Alpine image pulled and run as CT 998).
- The certificate: first from Let's Encrypt's staging CA, then `*.hs.efez.net`
  from the production CA (issuer `YE2`, 89 days); the TXT record was gone
  from OVH's name servers after both. A records `api1`–`api3.hs.efez.net`
  in the local zone (TSIG key `homestack`).
- One instance by hand (`api2`): reachable by its name with the public
  certificate, `GET /v1/capabilities` with the test token missing nothing.

`live_deploy.rs` (73 s, image `0.1.0-d104adebb140`): `apply` made CT 997 on
pve1 (`local-zfs`), 998 on pve2 and 999 on pve3 (`local-lvm`), each
pulled, created, started, answering over TLS, restarted without the
certificate in its configuration, given the test token and
`live-server.toml`; the three reported the same configuration version
through a client of their DNS names. With CT 997 stopped, `/v1/status`,
`/v1/storage` and `/v1/config/version` answered through the others (3–5 s
each: before the client remembered the last answering instance), and a
plan was answered by api2. The three containers were removed.

To be completed with the final `check.sh --live`.

## 5. Deliberate differences from Python

1. Several instances, their lock and the deployment have no Python
   counterpart.
2. Every operation that changes a VM writes and removes a lease in the
   VM's `description` (two config writes), and needs `VM.Config.Options`.

## 6. Open questions

None.
