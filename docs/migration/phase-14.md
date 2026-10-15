# Phase 14: setup, repo and backup in Rust

Decision 7 (`decisions.md`, owner 2026-10-15): the setup stays in the
client and reaches the workspace over SSH with the hardware key, as the
Python HomeStack does. Every group of the setup tree moves, each as its own
section, and works exactly as it does now:

| Section | Groups | Status |
|---|---|---|
| 14a | the core; Files, Credentials, Environment | done (below) |
| 14c | Applications: installers, managed values, application files | installers done in 14a; values and files open |
| 14d | Backup (BK), `setup --local` | open |
| 14e | Herdr | open |
| 14f | Repositories, `homestack repo` | open |
| 13b step 5 | the setup tree in the TUI (and `setup TARGET` without selectors) | open |

## The program on the workspace

`guest/setup_guest.py` is Python's `setup_guest.py`, byte for byte (a test
compares the two while `python/` exists). It runs on the workspace, not on
the desktop: the client streams it with its payload to `python3 -`, as
Python does, so its behaviour on the workspace cannot differ. Its own tests
run it the same way on this desktop (`tests/setup_guest.rs`). `bk.py` and
its units (14d) are sent unchanged the same way.

## 14a: the core, Files, Credentials, Environment

- **Where:** `crates/hs-cli/src/setup/`:
  - `definitions.rs` (`setup_config.py`): the default groups and items,
    `[setup]` of `client.toml` merged over them by ID with Python's checks
    and messages, the generated entries of an application;
  - `catalog.rs` (`setup_catalog.py`): rows, GitHub discovery through
    `gh api`, the immutable numeric snapshots in
    `$XDG_STATE_HOME/homestack/catalogs/`, the selectors;
  - `files.rs`, `plan.rs` (`setup_files.py`, `setup.build_plan`): order,
    dependencies, the desktop sources checked, overlapping writes refused;
  - `guest.rs`, `workspace.rs` (`setup.guest`, `WorkspaceSSH.run`,
    `transfer`): the clean `env -i` environment, one master SSH connection
    (`master.rs`), children that cannot authenticate, `rsync` copies;
  - `engine.rs` (`inspect_workspace_state`, `preflight_entry`,
    `apply_entry`, `execute_plan`): every item checked before anything is
    written, one operation snapshot on the workspace, items applied in
    order and recorded in the workspace's registry;
  - `cli.rs`, `show.rs` (`setup_cli.py`): `setup list`, `setup status
    TARGET`, `setup TARGET SELECTORS…` with `--dry-run`, `--catalog`,
    `--non-interactive`, `--yes`, `--json`; Rich's default tables.
- **Handlers:** file (Files, Credentials), environment, application
  installers. A handler not ported yet (managed values, Backup, Herdr,
  Repositories) refuses in its preflight, so nothing is written.
- **The workspace user** (name, uid, gid) and the address come from the
  server (`GET /v1/config`, `/v1/workspaces/{target}`); Python read them
  from `config.toml`.
- **Fixed on the way:** the SSH control directory is created `0700`, as
  Python's `TemporaryDirectory` (it was `0755`); this also applies to the
  install's root SSH.
- **Differences from Python:**
  - a template VM is refused because it is not running, not with
    "Refusing Setup on template VM" (the server's workspace read has no
    template flag);
  - `setup TARGET` without selectors (Python's Textual setup app) waits
    for the TUI's setup box; it says so;
  - `--json` keeps the key order of `serde_json` (sorted), not Python's.
- **Tests** (72 of the 76 of 14a ported; the manifest names them):
  `tests/setup_definitions.rs`, `setup_catalog.rs`, `setup_plan.rs`,
  `setup_guest.rs`, `setup_engine.rs`, and the CLI in `tests/cli.rs`
  against the in-process server. Open: the 3 startup tests and the TUI
  colour test of `test_setup_startup.py` and `test_setup_state.py`, with the
  TUI's setup box.
