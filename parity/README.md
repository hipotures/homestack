# Parity tests

Python is the reference implementation until the migration ends. The tools in
this directory check that the Rust implementation behaves the same. The plan is
in [docs/migration/PLAN.md](../docs/migration/PLAN.md), section 2; everything
runs through [scripts/check.sh](../scripts/check.sh). This directory and
`python/` are removed in the last phase; `scenarios/` stays.

| Path | What it is |
|---|---|
| `manifest.toml` | Every Python test, the phase that ports it and its Rust replacement (L1) |
| `ops.md` | The operation vocabulary shared by both sides |
| `normalize.toml` | Allowed output differences, each with a reason |
| `../scenarios/*.toml` | Simulated cluster states and requests (L2, L3) |
| `config-corpus.toml` | Valid and invalid configurations both implementations must judge alike (L3) |
| `python/homestack_parity/` | Test-only Python tools; HomeStack itself never imports them |

## Test manifest

```toml
tests = [
  { python = "tests/test_status.py::StatusTests::test_x", phase = "2", status = "todo" },
  { python = "tests/test_status.py::StatusTests::test_y", phase = "2", status = "ported", rust = ["status::tests::y"] },
  { python = "tests/test_setup_tui.py::T::test_z", phase = "0", status = "excluded", reason = "TUI: ..." },
]
```

- `python`: `tests/<file>.py::<Class>::<method>`, as printed by
  `python -m homestack_parity.list_tests tests` (run in `python/`).
- `phase`: `0`–`15`, or `14a`–`14f`.
- `status`: `todo`, `ported` (needs `rust`) or `excluded` (needs `reason`).
- `rust`: test names as `cargo test -- --list` prints them.

`hs-parity manifest-check` fails when a Python test is missing from the
manifest, an entry names a Python test that no longer exists, a `ported` entry
names a Rust test that does not exist, or an `excluded` entry has no reason.
`cargo test` running green covers "the Rust test passes".

The first assignment of phases came from
`python/homestack_parity/seed_manifest.py`; phases edit the manifest directly.

## Scenarios

A scenario is one TOML file:

```toml
[scenario]
name = "transport-node-status"
phase = "0"
description = "..."

[homestack]
config = "python/config.example.toml"    # Python config, relative to the repository root
# python_token = "root@pam!parity"       # defaults; both must exist in [pve]
# server_token = "homestack@pve!server"

[parity]
python = ["transport", "--json"]          # homestack arguments
rust = "GET /v1/pve/nodes/pve-example-1/status"
normalize = "transport-node-status"       # rule in normalize.toml

[parity.expect]                           # optional
ops = ["get{path=/nodes/pve-example-1/status}"]
body = { node = "pve-example-1" }

[pve]                                     # the simulated cluster, read by pve-sim
version = { release = "9.2", version = "9.2.21", repoid = "..." }
[[pve.nodes]]   # name, status (online/offline), pveversion, kversion, uptime, cpu, mem, ...
[[pve.tokens]]  # id, secret, acl = { "/path" = ["Privilege", ...] }; "*" grants all
[[pve.tasks]]   # node, type, id, user, log, polls_until_done, exitstatus
[[pve.faults]]  # op, occurrence (1-based), status, message
```

Unknown keys are errors. The ACL of a token is its effective privileges: for a
privilege-separated token, already the intersection with its user.

`crates/hs-parity/tests/scenarios.rs` starts `pve-sim` with the scenario, runs
the real Python CLI on `FixtureTransport` and the Rust server in-process, and
checks that

1. the Python command translation matches what the simulator saw,
2. both issued the same operations,
3. the outputs are equal after the normalization rule,
4. both match `[parity.expect]`.

`HOMESTACK_PHASE=N` limits the run to one phase (`check.sh --phase N` sets it).

## Configuration corpus

`config-corpus.toml` lists edits of `python/config.example.toml` (`set`,
`remove`, `append`, or a whole `text`) and the expected verdict: `accepted`
or the field the error is about (`repo.owner`,
`storage_layouts.homestack-storage-1`, ...).
`crates/hs-parity/tests/config_corpus.rs` writes every case to a file, runs
them all through Python's `config.load_config`
(`python -m homestack_parity.config_check`, which maps each `AppError`
message to its field) and through Rust's `hs_config::legacy::read`, and
fails on any disagreement. A case where Python legitimately differs says so
with `python = "<verdict>"` and a `reason`.

## Python tools

- `FixtureTransport` implements HomeStack's `Transport` protocol by sending
  each `pvesh` command to `pve-sim` over HTTPS. The scenario state therefore
  lives in one place, and the simulator logs both implementations' calls the
  same way. Python runs as `root@pam` on a node today, so its scenario token
  usually has `*`.
- `RecordingTransport` wraps a transport and keeps the commands and their
  operations.
- `cli_driver` runs `homestack.cli.main()` unchanged, with `open_transport`
  replaced to yield the two transports above.
- `config_check` prints Python's verdict on configuration files for the
  corpus.

Run `pve-sim` by hand with
`cargo run -p pve-sim -- --scenario scenarios/<file>.toml --ca-out /tmp/ca.pem`.

## Simulated behaviour not observed on a live cluster

- An offline node answers `/nodes/{node}/...` with HTTP 595 and
  `No route to host`. All nodes were online when the error shapes were
  recorded, so this follows PVE's proxy code rather than an observation.
- Task endpoints require `Sys.Audit` on the node; PVE also lets users read
  their own tasks.
- `/access/permissions` without `path` lists the paths of the token's ACL.
  PVE also lists paths derived from them (`/access/groups`, `/mapping/pci`,
  ...); HomeStack only queries single paths.
