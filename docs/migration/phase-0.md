# Phase 0: skeleton and test infrastructure

**Date:** 2026-10-12
**Branch:** `rust-migration`, tag `rust-phase-0`
**Base:** `hipotures/homestack` `main` @ `c09636b`

> **Layout changed after the tag.** The phase was built with the Rust
> workspace in `rust/`. Right after it, the Rust workspace moved to the
> repository root (`Cargo.toml`, `crates/`, `Containerfile`), the Python
> application to `python/`, the parity tools to `parity/` and the scenarios to
> `scenarios/`, because Rust is the lasting project and Python is removed in
> phase 15. Paths below use the new layout. The image later moved from Debian
> to Alpine with a static musl binary (`3c0e7be`).

## 1. Python behaviour this phase depends on

Phase 0 ports no HomeStack function. It builds the tools that later phases use
to compare the two implementations, so it depends on how Python talks to
Proxmox today.

### Transport contract (`src/homestack/transports/base.py`, `herdr.py`)

- `Transport.run(command, check=True, timeout=120, output_lines=4000)` returns
  `RemoteResult(returncode, output)`. With `check=True` a non-zero exit raises
  `AppError("Remote command failed (<rc>) on <control_node>: <command>\n<output>")`
  (`HerdrSession.run`).
- `run_json_value` parses the whole output as JSON, then falls back to the last
  line that parses; with `check=False` a failed command yields `None`.
  `run_json` additionally requires an object and returns `{}` for a failed
  unchecked command.
- `run_with_progress` streams output lines to a callback while the command
  runs.
- `execution_info()` describes the session; for Herdr it is only available
  after `verify()`.
- `cli.main` opens the transport once per command through
  `transports.open_transport(cfg)`, after `load_config`. `discover`, `install`
  and `setup` do not open it.

`FixtureTransport` reproduces these semantics, so the CLI cannot tell it from
Herdr except through `execution_info`.

### Routing (`proxmox.node_shell_command`)

A command for a node other than `[control] node` is wrapped as
`ssh -o BatchMode=yes root@<node> '<command>'`. The API proxies
`/nodes/{node}/...` itself (mapping T2), so the translator unwraps it and
produces the same operation as for the local command.

### `pvesh` output and failures (observed on pve2, PVE 9.2.21)

| Case | Exit code | Output |
|---|---|---|
| success | 0 | `data` as one line of JSON, keys sorted |
| API error, e.g. unknown node | 255 | the API message without the trailing newline |
| no handler for the path | 1 | `No 'get' handler defined for '<path>'` |

### Proxmox API error shapes (observed on pve1 with the read-only token)

| Case | Status | Body |
|---|---|---|
| wrong token secret | 401 | empty |
| missing privilege (`/nodes/pve1/syslog`) | 403 | `{"data":null,"message":"Permission check failed (/nodes/pve1, Sys.Syslog)\n"}` |
| unknown node | 500 | `{"data":null,"message":"hostname lookup 'nosuchnode' failed - ...\n"}` |
| unknown path | 501 | `{"data":null,"message":"Method 'GET /cluster/nothing-here' not implemented"}` (no newline) |

The message is also the HTTP reason phrase. `pve-sim` returns the same bodies.

### The dry-run read (`status.transport_status`, mapping A4)

`homestack transport --json` runs
`pvesh get /nodes/<node>/status --output-format json` with
`<node> = [node] name` and reports `pveversion`, `kversion` and `uptime` as
`pve.version`, `pve.kernel` and `pve.uptime`, next to `execution_info()`.
It is the only Python read that needs nothing but node status, which is why it
is the phase 0 parity scenario.

## 2. What was built

| Part | Content |
|---|---|
| Cargo workspace (repository root) | `hs-model`, `hs-pve`, `hs-core` (empty, see 6.1), `hs-server`, `pve-sim`, `hs-parity`; edition 2024, Rust 1.99, `unsafe` forbidden, `unwrap`/`expect` denied by clippy |
| `hs-server` | `GET /health/live`, `GET /v1/pve/version`, `GET /v1/pve/nodes/{node}/status`; bearer token from a file; mypowers error document; `x-request-id`; `Cache-Control: no-store`; `tracing` logs (text or JSON) without secrets or raw paths |
| `hs-pve` | token from a file, TLS against system roots plus optional CA, endpoint list with failover, `GET /version`, `GET /nodes`, `GET /nodes/{node}/status` |
| `pve-sim` | HTTPS `/api2/json` with `/version`, `/nodes`, `/nodes/{node}/status`, task status and log; scenario loading; token ACL checks; operation log; fault injection |
| Python test tools | `FixtureTransport`, `RecordingTransport`, the command translator, the CLI driver, the test lister (`parity/python/`) |
| Formats | scenarios, `manifest.toml`, `normalize.toml`, `ops.md` (`parity/README.md`) |
| `scripts/check.sh` | fmt, clippy, Rust tests, Python tool tests, the Python suite, manifest check; `--live`, `--phase N` |
| CI | `.github/workflows/rust-migration.yml` runs `scripts/check.sh` |
| Image | `Containerfile`, `docs/migration/deploy.md` |
| OpenAPI | `docs/openapi.json`, checked by a test |

Tests were written before the code for each crate and shown failing (they did
not compile against the empty crates) before the implementation.

## 3. Tests

| | Count |
|---|---|
| Python tests in the manifest | 593 |
| ported | 0 |
| todo, with a phase | 529 |
| excluded, with a reason | 64: 49 TUI (`test_setup_tui`, `test_setup_structured_ui`), 12 Herdr transport and the shell-command transport protocol, 3 ssh routing between nodes |
| new Rust tests, L1–L3 | 41 (`hs-model` 4, `hs-pve` 8, `pve-sim` 12, `hs-server` 9, `hs-parity` 8) |
| new Rust tests, L4 | 4 (ignored unless `--live`) |
| new Python tool tests | 13 |

Phases with tests assigned: 1 (16), 2 (37), 3 (9), 4 (3), 5 (17), 6 (12),
7 (28), 8 (8), 9 (24), 10 (13), 13 (90), 14a (73), 14c (55), 14d (94),
14e (22), 14f (28).

`scripts/check.sh`: passed. `scripts/check.sh --live`: passed. `--phase 0`:
passed.

## 4. Parity with Python

1 scenario (`scenarios/transport-node-status.toml`): passed. Python's
`homestack transport --json` on `FixtureTransport` and the Rust server's
`GET /v1/pve/nodes/pve-example-1/status` issue the same single operation,
`get{path=/nodes/pve-example-1/status}`, and report the same data after the
`transport-node-status` rule. Breaking the rule on purpose makes the test fail.

## 5. Live cluster

Run on 2026-10-12 against pve1, pve2 and pve3 (all online), read-only, with
the `homestack@pve!server` token (`PVEAuditor` on `/`).

| Check | Result |
|---|---|
| `/v1/pve/version` equals `GET /version` on every endpoint | passed |
| `/v1/pve/nodes/{node}/status` for every online node | passed |
| failover past a dead first endpoint | passed |
| an endpoint addressed by IP (certificate does not match) is refused; failover to the next | passed |
| the image answers `/v1/pve/version` with a dead first endpoint (`scripts/live-container.sh`) | passed |

The node names `pveX.efez.net` resolve on the desktop only through
`/etc/hosts`, not DNS. The container check passes the host's answers to the
container with `--add-host`; certificates are still verified by name.

No state on the cluster was changed by the tests. Setting up the token itself
(user `homestack@pve`, token `server`, two ACL entries) was done by hand on
pve2 before the phase, see `deploy.md`.

## 6. Decisions taken in this phase and differences from Python

No behaviour of Python was changed or reproduced differently. The single
allowed output difference is the `transport-node-status` rule in
`normalize.toml`.

Technical choices that later phases build on:

1. **Workspace layout (section 3 of the plan) confirmed with two changes:**
   `hs-core` exists but is empty until phase 2 brings the first operation and
   the `Pve` trait it is written against; `hs-cli` is created in phase 13.
2. **`FixtureTransport` asks `pve-sim`** instead of reading the scenario
   itself. The scenario state lives in one place, both implementations are
   logged by the same code, and the translator is checked against what the
   simulator actually received.
3. **The dry-run read is A4's Proxmox part.** The plan names only
   `/v1/pve/version` for phase 0, but no Python command reads `/version`
   alone. The server therefore also passes `/nodes/{node}/status` through;
   phase 2 decides the final shape of A4.
4. **Failover only when no connection was made** (refused, DNS, TLS
   verification). Once a request is sent, its result is final, so failover can
   never repeat a state-changing call. The endpoint that answered last is tried
   first next time.
5. **The token never reaches an unverified endpoint:** a TLS failure counts as
   "not connected" and the token is only sent after verification (test
   `an_endpoint_with_an_untrusted_certificate_never_receives_the_token`).
6. **Status codes:** Proxmox rejecting the server's own token (401/403) is a
   502, not a 401/403, because the client's request was fine.
7. **Settings from environment variables** until phase 1 (`server.toml`),
   named like mypowers' (`*_FILE` for secrets).
8. **`pve-sim` privilege model:** a token's ACL is its effective privileges;
   `*` grants everything and stands for `root@pam`, as which Python runs today.

## 7. Open questions for the next phases

1. The plan counts 594 Python tests; `unittest` discovery finds 593 at
   `c09636b` (1 skipped). The manifest follows the real list, and the check
   fails if it changes.
2. The phase of every test comes from rules in `seed_manifest.py`. Mixed files
   (`test_lifecycle`, `test_proxmox`, `test_status`, `test_alien_workspace`)
   were split by class and test name; each phase should confirm its part in
   step 3.
3. The installer (`test_install.py`, F1) has no phase in the plan; its tests
   are under phase 13 for now. Phases 11, 12, 14b and 15 have no Python tests
   (11 and 12 get new ones; 14b Environment tests may be hidden in
   `test_setup.py`).
4. Phase 3 changes state on the cluster and needs the pool `homestack-test`,
   VMIDs 240–249 and a token limited to that pool. None of them exist yet.
5. Behaviour modelled but not observed: HTTP 595 for an offline node (all
   nodes were online). Worth recording when a node is down.
6. The server will need the node names in DNS or in the container's
   `/etc/hosts` wherever it is deployed (phase 11).
