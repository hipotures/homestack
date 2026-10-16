# Phase 1: server configuration and capabilities

**Date:** 2026-10-12
**Branch:** `rust-migration`, tag `rust-phase-1`
**Scope:** F2, F3, T3; sections 8.1 and 8.2 of [api-mapping.md](api-mapping.md)

Before the phase the server image moved to Alpine 3.24 with a static musl
binary (`3c0e7be`, the owner's choice of target runtime).

## 1. Python behaviour (step 3)

### `config.load_config` (`python/src/homestack/config.py`)

Checks run in this order; the first failure raises `AppError`:

1. `install_draft = true` → "HomeStack configuration is incomplete …" with
   the hint to rerun `homestack install` (before any other check).
2. `version != 1` → "Unsupported config version". The comparison is Python
   equality, so `1.0` and `true` pass.
3. A `[sync]` table → "Obsolete [sync] configuration is not supported".
4. `[network] dns` required (`_need`), converted with `tuple(str(x) for x in …)`,
   at least one entry.
5. `[node] name` required, converted with `str()`.
6. `[storage_layouts]` optional (`or {}`), must be a table; every name
   `homestack-storage-<N>` (N without leading zero); every value a non-empty
   array; entries `str(item).strip()`, non-empty, no duplicates after
   stripping.
7. `[control]` optional table; `node` defaults to `[node] name`.
8. `[transport] type` must be `herdr` (missing: "Missing configuration
   value [transport] type"); `[transport.herdr]` needs non-empty `workspace`
   and `tab`, `debug` boolean (default true).
9. `[display] storage_unit` in KiB…PiB (default GiB); `storage_decimals`
   `int()`, 0–4.
10. `[repo] owner` optional string, empty means none, else
    `GITHUB_OWNER_RE` (1–40 characters); `checkout_root` (default `~/DEV`)
    normalized below `~/` without `.`/`..`/empty parts; `sort` in recent,
    created, name.
11. `[[setup.*]]` through `setup_config.parse_setup`.
12. `[sounds]` (`_parse_sounds`): known keys only; `enabled` boolean;
    `player` string or array of non-empty strings; `wait`/`ok`/`fail`
    strings without NUL; `repeat_seconds` number 1–30, not boolean.
13. `[workspace_ssh]` table; `user` non-empty; `[user] name` non-empty;
    the two must match; `identity_files` non-empty array of stripped,
    whitespace-free, unique paths; `identities_only` boolean; `log_level`
    one of nine OpenSSH levels (case-insensitive, stored upper case);
    `fido_devices` array of `yubikey`, `trezor` or USB ids
    (`fido.validate_entry`), lower-cased, deduplicated in order.
14. The node name's trailing number (`re.search(r"([1-9][0-9]*)$")`) selects
    `homestack-storage-<N>`, which must exist; its first storage is the
    default for root and home.
15. `[root] disk` and `[home] disk` non-empty after stripping ("[root] disk and
    [home] disk must be non-empty", one message for either) and different.
16. Remaining required values: `home.default_size`,
    `network.prefix/cidr/gateway`, `cloud_init.snippet_storage/snippet_dir`,
    `user.uid/gid`, converted with `int()` or `str()`.

Unknown tables and keys are ignored (the owner's real configuration has a
`[runtime]` table nothing reads).

### `config.publish_config`

Validates by loading the written temporary file, writes it with mode 0600 in
the target directory (created with 0700), fsyncs, copies an existing target to
`config.toml.backup-<timestamp>` (0600, exclusive; not for an install draft),
renames over the target, fsyncs the directory; on any failure the temporary
file is removed and the original stays.

### Behaviour that looks like a bug (reported, not changed in Python)

Verified by running `load_config` on edited copies of the example:

| Input | Python |
|---|---|
| `gold_vmid = "100"`, `100.9`, `-5`; `uid = true` | accepted as 100, 100, -5, 1 |
| `gold_vmid = "abc"` | `ValueError` traceback instead of `AppError` |
| `node = 5` (not a table) | `TypeError` traceback |
| `dns = "192.0.2.53"` (a string) | accepted as eleven one-character servers |
| `name = 1`, `name = "pve example 1"` | accepted; the name later becomes an API path segment |
| `version = 1.0`, `cidr = 99`, `storage_decimals = "2"` | accepted |

The Rust readers reject all of them (section 6); the corpus records each case.

### What the server needs from Proxmox (T3)

`GET /access/permissions?path=P` on the live cluster (PVE 9.2.21, read-only
token): answers for any path, existing or not, with inherited privileges
(`/` → `/vms`), shaped `{"data":{"P":{"Priv":1,...}}}`. Recorded in
[decisions.md](decisions.md).

## 2. What was built

| Part | Where |
|---|---|
| `server.toml` schema and validation, SHA-256 version | `crates/hs-model/src/config.rs` |
| `hs-config split` / `check`; reader of the Python file; `client.toml` | `crates/hs-config` |
| Privileges per operation, probe paths, evaluation | `crates/hs-core/src/capabilities.rs` |
| `PveClient::permissions` | `crates/hs-pve/src/client.rs` |
| `/access/permissions`, the 47 PVE privileges | `crates/pve-sim` |
| Atomic store of `server.toml` with backup | `crates/hs-server/src/config_store.rs` |
| `GET`/`PUT /v1/config`, `GET /v1/config/version`, `GET /v1/capabilities` | `crates/hs-server/src/routes.rs` |
| `HomeStack-Config-Version` check for state-changing routes | `crates/hs-server/src/precondition.rs` |
| `HOMESTACK_CONFIG_FILE`, image directory `/var/lib/homestack` | `settings.rs`, `Containerfile` |
| Configuration corpus and its Python side | `parity/config-corpus.toml`, `parity/python/homestack_parity/config_check.py` |
| Docs | [deploy.md](deploy.md), `parity/README.md`, `parity/ops.md`, `docs/openapi.json` |

**Split of the configuration (8.1).** `server.toml`: `[node]` (name,
gold_vmid), `[storage_layouts]`, `[root]`, `[home]`, `[network]`,
`[cloud_init]`, `[user]`, `[repo] owner`. `client.toml`: `[workspace_ssh]`,
`[repo]` (owner, checkout_root, sort), `[display]`, `[sounds]` and
`[[setup.*]]` as written. Dropped: `[control]`, `[transport]` and unknown
tables (reported by `split`). Proxmox endpoints, tokens and the listen address
stay in environment variables: they differ per instance (phase 11) or are
secrets, and the uploaded document must hold no secrets.

**Upload (8.2).** The body is the TOML text; its SHA-256 is the version and
the `ETag`. `If-None-Match: *` for the first upload, `If-Match: "<version>"`
or `*` afterwards; neither gives `428`, a mismatch `412`, an invalid document
`422 config_invalid` with `error.field`. The same text again returns
`changed: false` and writes nothing. The server reads the file at start; an
invalid file stops it.

**Capabilities.** For each of nine operations (read, guest_read,
gold_readiness, create, refresh, resize, migrate, destroy, addkey) the
response lists required and missing privileges. The probe asks Proxmox once
per distinct ACL path (7 for a typical configuration). Readings of the
mapping are documented at the top of `capabilities.rs`: `/vms` stands for
workspaces until the pool exists, D2 still needs `VM.GuestAgent.Unrestricted`
(decision 5), `SDN.Use` is checked at plan time because the bridge comes from
Gold.

## 3. Tests

Written first and shown failing: none of the new tests compiled against the
phase 0 code (missing `hs_model::config`, `hs_config::*`,
`hs_core::capabilities`, `PveClient::permissions`, `ServerConfig::config_file`);
the Python tool test failed on a wrong field mapping, fixed before the Rust
side existed.

| | Count |
|---|---|
| Python tests of phase 1 (`test_config.py`) | 16 |
| ported | 13 |
| moved to phase 13 (installer, F1) | 3: `InstallDraftTests::*` test `publish_install_draft` / `load_install_draft`; their runtime-loader part is covered by `an_install_draft_is_rejected_with_the_resume_hint` |
| excluded | 0 |
| new Rust tests | 54 (plus 1 live) |
| new Python tool tests | 2 |
| Rust tests in the workspace | 100, plus 5 live |

Ports and their Rust tests are in `parity/manifest.toml`. Port notes:

- `test_default_config_*`: the default input of `hs-config split`.
- `test_publish_*`, `test_validation_failure_*`, `test_atomic_replace_*`:
  `PUT /v1/config` and the store. The atomic-replace test injects a failing
  rename like Python's patched `os.replace`.
- `test_generated_toml_round_trips_*`: `split` output reloads through both
  readers with equal values; `server.toml` also round-trips through
  `Config::to_toml`.
- `test_repo_config_validation`, `test_root_and_home_*`,
  `test_obsolete_sync_*`: both the reader of the Python file and the strict
  `server.toml` reader.

## 4. Parity with Python

- **Configuration corpus:** 71 cases (59 where both agree, 12 recorded
  differences, section 6). Python's `load_config` and `hs_config::legacy`
  reach the same verdict (accepted, or rejected for the same field) on every
  case. A deliberately wrong expectation fails the check (tried).
- **Scenarios:** the phase 0 scenario still passes. Phase 1 adds no Python
  command, so there is no new command scenario.
- The real configuration of the owner (`~/.config/homestack/config.toml`)
  splits cleanly: `not carried over: control, runtime, transport`, all 8
  setup items kept (output written to a scratch directory only).

## 5. Live cluster

`scripts/check.sh --live`, all passed:

- `live_capabilities_match_the_token_privileges`: a configuration for `pve1`
  uploaded in-process; for each operation, `missing` equals the required
  privileges that `GET /access/permissions` (asked directly) does not list.
  With the read-only `PVEAuditor` token: read, guest_read, gold_readiness
  available; create 8, refresh 6, resize 3, migrate 4, destroy 2, addkey 3
  privileges missing.
- `scripts/live-container.sh` (Alpine image): `PUT /v1/config` into a named
  volume, container removed and started again on the same volume, then
  `GET /v1/config/version` and `GET /v1/config` match the upload; capabilities
  answer with read available. The phase 0 gate (version, dead first endpoint)
  passed in the same run.
- Nothing was written to Proxmox.

## 6. Decisions taken in this phase and differences from Python

1. **Type strictness.** Both Rust readers require the TOML types Python's own
   `config_to_toml` writes and reject the coercions in section 1 (strings for
   numbers, floats, booleans as numbers, scalars as arrays). Corpus cases with
   `python = ...` record each one.
2. **Ranges Python does not check:** `gold_vmid` 100–999999999 (Proxmox
   VMIDs), `cidr` 0–32, `uid`/`gid` 0–4294967295.
3. **Node name** must be a DNS label (`NodeName`), because it becomes an API
   path segment. Python only needs a trailing number.
4. **`server.toml` and `client.toml` reject unknown keys**; the reader of the
   Python file still ignores them, like Python, and reports them as dropped.
5. **Messages never repeat values** (mypowers rule): "Invalid GitHub
   repository owner; expected …" instead of echoing the owner. Unknown keys
   are named only when they look like keys. The field (`error.field`) is the
   machine-readable part.
6. **Backup name** `server.toml.backup-<first 16 hex of the old file's
   SHA-256>` instead of a timestamp: uploading the same text twice keeps one
   copy, and identical content is never backed up twice.
7. **The setup catalog is not validated by `split`**; it is carried to
   `client.toml` as written and validated where it is used (phase 14). The
   corpus does not vary it.
8. **`HomeStack-Config-Version`** is the header name for the expected
   configuration version; missing → `428`, other version → `409
   config_version_mismatch`, no configuration → `409 config_missing`.
9. **No probe at start.** Capabilities are computed per request; a start-up
   probe that disables modules (mapping 8.5) makes sense once modules exist.

## 7. Open questions for the next phases

1. Do you accept the stricter validation (section 6, items 1–3) for the
   Python CLI too, i.e. should the Python bugs in section 1 be fixed there, or
   stay until Python is removed?
2. Phase 3's test pool and pool-limited token now exist (see
   [decisions.md](decisions.md), probe results). Capabilities will then check
   `/pool/<name>` in addition to `/vms`; `server.toml` needs a pool key for
   that (not added yet). The owner chose not to fix the Python bugs of
   section 1; they stay until Python is removed.
3. `client.toml` has no server address or client token yet; phase 12 adds
   them when the Python client talks to the server.
4. The cross-check `[workspace_ssh] user` = `[user] name` now spans two files;
   the client should compare its user with `GET /v1/config` (phase 12/13).

## Optional Gold after server installation

The Rust configuration permits an absent `node.gold_vmid`. The server can be
installed and queried before a base VM is prepared; create and refresh refuse
to run until Gold is configured. An explicitly supplied VMID still needs to
be an integer in the Proxmox range. Canonical serialization omits an unset
Gold instead of writing a placeholder VMID. This deliberately differs from
the legacy Python requirement. The shared parity corpus records
this deliberate difference; Rust configuration and API tests cover the new
contract.
