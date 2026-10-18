# Phase 14: setup, repo and backup in Rust

Decision 7 (`decisions.md`, owner 2026-10-15): the setup stays in the
client and reaches the workspace over SSH with the hardware key, as the
Python HomeStack does. Every group of the setup tree moves, each as its own
section, and works exactly as it does now:

| Section | Groups | Status |
|---|---|---|
| 14a | the core; Files, Credentials, Environment | done (below) |
| 14c | Applications: installers, managed values, application files | done (below) |
| 14d | Backup (BK), `setup --local` | done (below) |
| 14e | Herdr | done (below) |
| 14f | Repositories, `homestack repo` | done (below) |
| 13b step 5 | the setup tree in the TUI (`e`; Menu › Set up this desktop) | done (`phase-13b.md` section 9) |

## The program on the workspace

`guest/setup_guest.py` is Python's `setup_guest.py`, byte for byte (a test
now checks its frozen reference hash after phase 15). It runs on the workspace, not on
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
  installers; 14c adds managed values and files, 14d Backup, 14e Herdr,
  14f Repositories.
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
  - `setup TARGET` without selectors (Python's Textual setup app) says to
    open it in the TUI (`e` on the workspace), where the setup box is;
  - `--json` keeps the key order of `serde_json` (sorted), not Python's.
  - an environment item of `setup status` also has `differs`: the files
    that differ from what the setup writes (the TUI marks them with `*`,
    and its STATE column says `differs` for Python's `needs update`; the
    owner, 2026-10-15).
- **Tests** (76 of the 76 of 14a ported; the manifest names them):
  `tests/setup_definitions.rs`, `setup_catalog.rs`, `setup_plan.rs`,
  `setup_guest.rs`, `setup_engine.rs`, and the CLI in `tests/cli.rs`
  against the in-process server; the 3 startup tests and the TUI colour
  test in `tests/tui_setup.rs` with the setup box.

## 14c: Applications with managed values and files

- **Managed values** (`setup/documents.rs`, Python `setup_documents.py`):
  each leaf of an application's `config_files` is read, compared and
  patched on the desktop; the workspace only reads and writes the bytes
  (`structured-read`, `structured-write` with the digest it expects,
  `structured-restore` from the operation snapshot).
  - **TOML** keeps its formatting with `toml_edit` (as `tomlkit`): the
    comments above and after a changed value stay, new keys go to their
    table, an inline table on the path of a new nested key becomes a
    standard table.
  - **JSON** keeps its key order and is written as Python's
    `json.dumps(indent=2, ensure_ascii=False)`, floats as Python's `repr`;
    duplicate keys and NaN or Infinity are refused, as before.
  - **YAML** (`yaml-rust2`) is read with aliases expanded and merge keys
    applied; a document that changes is written again from its values.
    Difference: `ruamel` kept the comments, anchors and quoting of a YAML
    file it changed; here they are lost. A YAML file that needs no change
    is not written, as before. (The owner's configuration has no YAML
    file.)
  - A document already right comes back byte for byte; every written
    candidate is read again and each leaf checked.
- **Engine** (`engine.rs`): one read and one merge per document for all
  its leaves; the merge is repeated after the installer, so its own edits
  are kept; a document that changed between preflight and apply is refused
  unless its application is being installed; the application's validator
  runs once after its values (or its installer) and only its status field
  is ever shown; a failure restores the documents the unit wrote, newest
  first, and records nothing.
- **Application files** (`files = [...]` of an application): copies owned
  by the application, after its installer, selectable alone by their
  stable ID (`app=claude:file:~/.claude/statusline.sh`).
- **Tests** (51 of the 55 of 14c ported): `tests/setup_documents.rs`,
  `tests/setup_structured.rs`. Excluded: 4 tests of `setup_guest.py` that
  swap its functions in the middle of an operation (the program is sent
  unchanged); the TUI test of the application files in `tests/tui_setup.rs`.

## 14d: Backup (BK) and `setup --local`

- **Where:** `setup/backup.rs` (Python `backup.py`); BK (`bk.py`) and its
  units are in `guest/backup/`, Python's byte for byte (a test compares
  them), installed with the guest program's `managed-files-install`.
- **Retrieval keys** on the desktop, `~/.ssh/homestack/backup/vm<VMID>-bk-
  {archive,status}[.pub]`: inspected without creating anything, reconciled
  under a lock on the private (0700) directory, never rotated; every file
  is read through `O_NOFOLLOW`, written through its pinned parent with a
  check that nothing changed in between, and a private key's public half
  is derived from a memory file, never from a path (`rustix`, as Python's
  `os` calls). The public halves go to the workspace's `authorized_keys`
  restricted to BK.
- **The workspace:** the runtime checks (python3, file, git, systemctl,
  the Python modules of BK, the user systemd manager, `/usr/bin/cat`),
  only the changed assets sent, the configuration created only when
  missing, `daemon-reload` when a unit changed, `backup.timer` enabled and
  verified; BK's own data under `~/backup` is never read or written.
- **`setup --local backup=bk`:** this desktop's own account (`getent
  passwd`), only the Backup group, no SSH and no retrieval keys; commands
  run with a clean environment, the guest program with the desktop's
  `python3`.
- **Tests** (20 of the 94 of 14d ported): `tests/setup_backup.rs` (the
  retrieval keys with the real `ssh-keygen`), and the dry runs in
  `tests/cli.rs`. Excluded: the 74 tests of BK itself (`test_bk.py`), a
  program sent unchanged; the TUI test in `tests/tui_setup.rs`.

## 14e: Herdr

- **Where:** `setup/herdr.rs` (Python `setup_herdr.py`); the user unit is
  `guest/herdr/herdr.service`, Python's byte for byte.
- **On the workspace:** the official installer only when Herdr is
  missing, linger for the user (refused with the administrator's command
  when not allowed), the unit written with the digest it replaces (a
  concurrent edit is refused), the service enabled and started only when
  no session runs; a running server is never updated, restarted or
  stopped, and the service then starts at the next boot.
- **On the desktop:** `herdr machine list` finds the workspace's profile
  (by name or address, session `default`; a label taken by another
  profile is refused); an existing profile is renamed and enabled without
  authentication, a missing one is added with `herdr machine add`, which
  may ask for the touch of a security key: the CLI shows the notice and
  the command and gives it the terminal with the touch cues; the
  desktop's SSH alias must resolve to the workspace's address, account
  and keys first. Unattended, a missing profile blocks before anything is
  written. External output is never shown; known diagnostics are named.
- **Tests** (23 of the 23 of 14e ported): `tests/setup_herdr.rs` (a
  simulated workspace and desktop, the real guest program), the cue in
  `tests/touch.rs`, and the vendor snippet's linger after the home is
  mounted; the TUI test in `tests/tui_setup.rs`.

## 14f: Repositories and `homestack repo`

- **Where:** `setup/repo.rs` (Python `repo.py`): the checkout under
  `[repo] checkout_root`, a deploy key for each repository in
  `~/.ssh/homestack/github/`, registered read-write on GitHub through the
  desktop's `gh`, and the checkout's own `core.sshCommand`.
- **In the setup:** one guest call reads every selected repository for
  the status; the preflight checks on GitHub that the repository is still
  the same one, administered and not archived, inspects the checkout and
  refuses what it would have to clean, reset or replace (a non-Git
  directory, another origin, uncommitted or untracked files, a detached
  HEAD, no upstream) and any key repair, which only `homestack repo` does.
  The apply clones, or fetches and fast-forwards only (merge options of
  the branch and hooks ignored, ignored files never overwritten), then
  verifies Git access. Only `.git/config` goes into the operation
  snapshot, when the origin or SSH command changes.
- **`homestack repo TARGET [REPOSITORY] [--json]`:** the repository
  (default `[repo] owner`/workspace name) of a running workspace: `--json`
  reports the state; otherwise the `REPOSITORY` panel and the menu Exit,
  Setup, Rotate key (a terminal is needed). Rotation generates a new key,
  revokes the old one on GitHub, installs and registers the new one, then
  verifies access.
- **Tests** (28 of the 28 of 14f ported): `tests/setup_repo.rs` (the
  helpers and the actions, and setup on real temporary Git checkouts with
  a simulated GitHub), and `repo` in `tests/cli.rs`.
