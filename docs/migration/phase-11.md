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

Image `ghcr.io/hipotures/homestack-server:<version>` (Alpine 3.24): the
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

Only the Proxmox API (deploy token) and the server's API:

1. `cert`: the wildcard certificate from Let's Encrypt, DNS-01 through the
   OVH API (`_acme-challenge.hs` in zone `efez.net`), kept on the desktop;
   renewed when less than 30 days are left.
2. `apply`, one instance at a time (the others keep serving): pull the
   image on the node (`oci-registry-pull`); if the container exists,
   stop and destroy it; create it (`pool homestack-service`, the CTID, the
   address, the storage), set `entrypoint` and `env` (with the certificate),
   start it; wait for `GET /health/live` over TLS (by the instance's
   address with the certificate's name); remove the certificate from
   `env`; send the Proxmox token and `server.toml`; check
   `GET /v1/config/version` of every instance is the same.
3. `status`: every instance's container, version and config version.
4. `remove`: stop and destroy the containers (the live gate).

### 2.4 Client: the first reachable instance

`hs-client`: a list of instance URLs, the client token and the CA (none
for a public certificate). A read goes to the first instance that answers;
a plan, its apply and the job are kept on the instance that made the plan.
If that instance stops answering, the client does not repeat the apply
elsewhere (mapping 8.3): it reports the job as unknown, to be planned
again from the state then.

## 3. Tests

To be filled in.

## 4. Live cluster (L4)

To be filled in.

## 5. Deliberate differences from Python

1. Several instances and their lock have no Python counterpart.

## 6. Open questions

None.
