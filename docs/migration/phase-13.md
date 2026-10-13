# Phase 13: the client in Rust (CLI)

**Date:** 2026-10-13
**Branch:** `rust-migration`
**Scope:** the `homestack` command in Rust for the Proxmox operations,
which now go through the server: plans and confirmations, `--json` with
Python's exit codes, the local effects I1 (SSH config, `known_hosts`,
`identity_files`) and I2 (FIDO devices, touch cues).
**Decisions used** (`decisions.md`, owner's decisions of 2026-10-13 for
phase 13):

- decision 8: a bearer token only, sent only over HTTPS;
- the installer (F1) is moved to a new phase 13c;
- the binary is named `homestack`;
- `transport` and `discover` go through the server.

Also used: decision 6 (the LUKS passphrase goes to the server with the apply
request) and mapping 8.3 (the plan, its apply and its job stay on one
instance; after losing that instance the client reads the state again
instead of repeating the request).

## 1. Python behaviour (step 3)

### 1.1 The command line (`python/src/homestack/cli.py`)

- **Parser** (`build_parser`): global `-h/--help`, `--config PATH` (default
  `default_config_path()`), `--json`, `-y/--yes`. The same `--json`, `-y`
  and `-h` are accepted after a subcommand.
  - **Subcommands:** `discover`, `install`, `create VMID NAME [--home-size]
    [--storage] [--node]`, `refresh T`, `resize T (--root-size |
    --home-size)` (exactly one of the two), `addkey T [--key FILE]
    [--restart]`, `destroy T`, `migrate T NODE --target-storage S` (the
    flag is required), `setup ...`, `repo T [OWNER/REPO]`, `status [T]`
    and `transport`.
  - **Usage errors** (unknown command such as `sync`, missing argument,
    both sizes) exit with code 2 (argparse).
- **Help:** `-h`, no command, or `-h` after a command prints the help
  screen and exits 0, without reading the config.
- **Configuration:** `discover` and `install` run before the config is
  loaded. Every other command loads it first; a load error exits 1.
- **Confirmation** (`create`, `refresh`, `resize`, `addkey`, `destroy`,
  `migrate`), in this order:
  1. Build the plan. Every error up to here makes no change.
  2. Without `--yes`, `--json` prints `{"ok": false,
     "confirmation_required": true, "message": "No changes made. Re-run
     with --yes to ...", "plan": PLAN}` and exits **3**. The message
     differs per command; a refresh in recovery mode has its own.
  3. Without `--yes` and without `--json`:
     - the plan is shown;
     - with no TTY on stdin the run fails: `Interactive confirmation
       requires a TTY; use --yes for automation` (exit 1);
     - the question defaults to *no* and is worded per command (refresh in
       recovery mode, destroy "INCLUDING its persistent home"). Declining
       prints `Cancelled. No changes were made.` and exits 0.
  4. Execute. With `--json` the result is printed; exit 0.
- **Command-specific steps:**
  - `resize`: in the interactive path, a target size equal to the current
    size prints `Disk is already S: S -> S changes nothing.` and exits 0
    before the question.
  - `addkey`: the plan's steps are listed as numbered lines. After the key
    is prepared on a running VM, the user is asked whether to restart it,
    defaulting to *no*. The question is skipped with `--restart`, and never
    asked without a TTY or with `--json`.
- **Errors:** an `AppError` exits 1. With `--json` it prints `{"ok": false,
  "command": C, "error": MESSAGE}`; otherwise the error is shown in a
  panel. Ctrl+C exits 130 (`"error": "Interrupted"` with `--json`).
- **Targets** (`lifecycle.resolve_workspace_target`):
  - digits are taken as a VMID with no lookup;
  - a name must match `[A-Za-z][A-Za-z0-9.-]{0,62}` (message `Invalid
    workspace target ...`) and is resolved to exactly one tagged VM.
- **`status VMID` of an absent VM:** `{"ok": false, "command": "status",
  "vmid": V, "status": "absent"}`, exit 0. The human output reads "VM V
  does not exist."
- **Envelopes:**
  - `status` adds `ok`, `command`, `scope` (`global`), `config` (the
    config path) and `execution` (the Herdr session) around the read;
  - the global summary also carries `storage_unit` and `storage_decimals`
    from `[display]`;
  - `transport` prints `{"ok", "command": "transport", "transport":
    execution_info, "pve": {"node", "version", "kernel", "uptime"}}`.

### 1.2 Local effects after an operation (I1)

They are in `lifecycle.py`, `addkey.py` and `workspace_ssh.py`:

- **`create`** (`create_workspace`), after the guest is verified:
  - `forget_local_ssh_host(ip)`, then `write_local_ssh_config(cfg, vmid,
    name, ip)`;
  - if writing the config fails, the error says `Workspace VM V (N) was
    created and verified successfully, but local SSH config generation
    failed: ...` and the VM is not rolled back;
  - the result adds `ssh_known_hosts_removed`, `ssh_config_path` and
    `ssh: {"root": "ssh root@NAME", "user": "ssh NAME"}`;
  - a failed create writes no SSH config.
- **`refresh`:** after the result is verified, `forget_local_ssh_host(ip)`.
  The result adds `ssh_known_hosts_removed` and `ssh: {"root": "ssh
  root@IP", "user": "ssh USER@IP"}`.
- **`destroy`**, only once the VM is verified absent, in this order:
  1. `cleanup_retrieval_keys(vmid)` (BK, `backup.py`);
  2. `remove_local_ssh_config(vmid)`;
  3. `forget_local_ssh_host(ip)`.

  A failure says `Workspace VM V (N) was destroyed successfully, but local
  SSH cleanup failed: ...`. If the VM still exists, nothing local is
  touched. The result adds `backup_keys_removed`, `ssh_config_removed` and
  `ssh_known_hosts_removed`.
- **`addkey`** (`add_key`):
  - after a restart, `forget_local_ssh_host(ip)` fills
    `known_hosts_removed`;
  - with `--key`, `sync_local_ssh` runs: `register_identity` appends the
    private key (in `~/` form) to the one-line `identity_files` array of
    `[workspace_ssh]`, keeping the file's other text and its mode, and is
    a no-op when the key is already there. Then the VM's SSH config is
    written with the new identity;
  - the result has `local: {"identity_files_changed", "ssh_config"}`. A
    local failure becomes `local_error` and does not fail the command;
  - `private_key_for(pub)` requires the private key to exist next to the
    `.pub` file before planning.
- **`write_local_ssh_config`:**
  - writes `~/.ssh/config.d/homestack/vm<VMID>-<NAME>.conf` atomically:
    a temporary `.NAME.*` file with mode 0600, `fsync`, then `rename`;
  - after the rename, removes the other `vm<VMID>-*.conf` files of that
    VMID;
  - if the rename fails, the old entry stays and the temporary file is
    removed.
- **SSH config content** (`local_ssh_lines`):
  - `Host NAME vmVMID`, `HostName`, `User`, each `IdentityFile`,
    `IdentitiesOnly`, `LogLevel`;
  - with identity files of two or more security-key kinds (file names with
    `yubikey` or `trezor`), plain keys go first, then one `Match
    originalhost NAME,vmVMID exec "[!] fido2-token -L ... | grep -qE
    'vendor=0x..'"` block per kind, in `fido_devices` order.
- **`remove_local_ssh_config(vmid)`** removes the `vm<VMID>-*.conf` files,
  sorted, and nothing else.
- **`forget_local_ssh_host(host)`**, for each of `host` and `[host]:22`:
  - `ssh-keygen -F`: exit 1 means not present, skip it; any other non-zero
    exit is an error;
  - `ssh-keygen -R`, then report the targets removed;
  - fails when `ssh-keygen` is missing.

### 1.3 FIDO devices and touch cues (I2: `fido.py`, `touch.py`, `WorkspaceSSH.__enter__`)

- **`fido.py`:**
  - `fido_devices` entries are `yubikey`, `trezor`, `vvvv` or `vvvv:pppp`;
  - devices are listed with `fido2-token -L`, which is required;
  - `select` returns the first preferred kind with a connected device,
    with all of its devices. With none connected the error is `No
    configured security key is connected (YubiKey, Trezor); plug one in
    and retry.`;
  - `sandbox_prefix` runs `bwrap --bind / / --dev /dev --dev-bind
    <device>...` (bwrap is required);
  - `filter_identity_options` drops `-i` files of the other kind;
  - `missing_key_message` gives three lines when the host lists only keys
    of another kind;
  - `advice` applies when several kinds are connected.
- **`touch.py`:**
  - `Sounds`: freedesktop files and the first available player
    (`pw-play`, `paplay`, `canberra-gtk-play -f`), with the terminal bell
    when there is no player or file;
  - `TouchWatch`: the first cue after 1 s, then every 8 s, up to 300 s.
    `finish(ok)` plays `ok` only if a cue was played and `fail` always; it
    is idempotent;
  - `classify_ssh_failure` gives `absent`, `not-confirmed` (signing failed
    after at least 25 s), `denied` or `failed`, each with a short message.
- **The workspace SSH master** (`WorkspaceSSH.__enter__`):
  - with `fido_devices` set, it selects the devices, checks the host's
    `IdentityFile` list from `ssh -G`, runs inside the sandbox with `-F
    ~/.ssh/config` (or `/dev/null`) and `IdentityAgent=none` unless the
    options name an agent, and offers only the matching identities;
  - the ssh failure is shown as a short message, never as raw output;
  - the touch cue wraps the master;
  - the master is used by `setup`, `repo` and the passphrase path of
    `resize`. Of those, only `resize` is in phase 13, and the passphrase
    now goes to the server.

## 2. Design

### 2.1 Crate `hs-cli`, binary `homestack`

- **Arguments:** a hand-written parser that mirrors argparse: the same
  commands and flags, `-h`, and exit code 2 with a usage line on a usage
  error.
- **Commands only in Python until their phase:**
  - `setup`, `repo`: phase 14;
  - `install`: phase 13c.

  They exit 1 with `homestack COMMAND is still provided by the Python
  client (phase N)`.
- **Configuration:**
  - `client.toml` (`hs-config`), default
    `$XDG_CONFIG_HOME/homestack/client.toml`, overridden by `--config`;
  - new section `[server]`:
    - `urls`: one or more `https://` URLs;
    - `token_file`: default `~/.config/homestack/api-token`, mode 0600 or
      stricter;
    - `ca_file`: optional, extra trusted CA;
  - an `http://` URL is refused (decision 8);
  - `hs-config split` writes `[server]` only when given `--server-url`.
- **Server calls** go through `hs-client` (phase 11):
  - reads go to the first instance that answers;
  - an operation:
    1. `GET /v1/workspaces/{target}` resolves a name to a VMID.
    2. `POST /v1/plans`; the `plan` printed is the plan's `summary`.
    3. `GET /v1/config/version` on the same instance.
    4. `POST /v1/plans/{id}/apply` with that version, plus the LUKS
       passphrase when the plan's `secrets_required` names it.
    5. Poll `GET /v1/jobs/{id}` on that instance until the job ends. In the
       human output each finished step is printed as it ends.
  - if that instance stops answering, the command fails and says to run
    it again: the plan or the job may or may not have been applied, and a
    new run reads the state again (mapping 8.3).
- **Local effects** run after the job succeeds, from the job's result
  (`vmid`, `name`, `ip`, `user`, `host_key_changed`, `restarted`).

### 2.2 Differences from Python (deliberate)

1. **Plan and result content** is the server's (phases 5–10): no snippet
   or root-rename fields. Each command adds the fields Python's CLI added
   for the client (`ok`, `ssh*`, `known_hosts`).
2. **`status`:**
   - `config` is the path of `client.toml`;
   - `execution` names the server instance that answered: `{"type":
     "server", "url": ...}` instead of the Herdr session.
3. **`transport`:**
   - `transport` describes the server instances, one entry per URL:
     `url`, `ok`, `config_version` and `error` (no route reports the
     server's own version);
   - `pve` is the status of the node named in the server's `server.toml`
     (`GET /v1/config`), read with `GET /v1/pve/nodes/{node}/status`;
   - exit 1 when no instance answers.
4. **`discover`:**
   - needs `client.toml` for the server's address;
   - reports the local commands (`ssh`, `ssh-keygen`, `fido2-token`,
     `bwrap`), the hardware SSH identities and the server's
     `/v1/discovery`;
   - Herdr is not scanned;
   - exit 1 when no instance answers.
5. **`resize` with a LUKS passphrase:** the passphrase is asked with no
   echo, on a TTY and without `--json` (as in Python), and sent to the
   server (decision 6). There is no SSH master to the VM, so no boot-id
   check over SSH.
6. **`addkey` restart:** the server takes `restart` in the plan. In the
   interactive path the CLI first applies a plan with `restart: false`.
   When the VM is running and the user says yes, it applies a second
   plan, without the key and with `restart: true`; the server then keeps
   `sshkeys` as they are. So the order matches Python's (prepare first,
   ask after).
7. **Ctrl+C while a job runs** stops only the client. The job continues
   on the server, and the message names the job and its instance.
8. **`known_hosts` is named:** `ssh-keygen -F/-R` gets `-f
   $HOME/.ssh/known_hosts`. Python relies on ssh-keygen's default, which
   follows the account's home directory, not `HOME`; for a normal login
   both are the same file. Naming it keeps every desktop file of the
   client under `HOME`, which the tests and the live gate use.
9. **BK keys at `destroy`:** the cleanup of BK retrieval keys
   (`backup.cleanup_retrieval_keys`, phase 14d) is not ported yet. Until
   then `destroy` lists the files of that VMID still present under
   `backup_keys_left` and warns, removing nothing.

## 3. Tests (step 4)

| Python | Rust |
|---|---|
| `test_cli.py` (7) | parser (`create`, `refresh`, `destroy`, `status`, `migrate`, `repo` parsing, `sync` rejected with 2, `--node`), `main` callable, error text kept as written, the JSON plan of `create` forwarding the destination without applying, the create plan view showing the node and source |
| `test_resize_cli.py` (2) | exactly one size; the JSON plan exits 3, `--yes` applies |
| `test_workspace_ssh.py` (8) | `local_ssh` module: the exact files with mode 0600, a custom identity list, stale files of the VMID replaced, a failed rename keeping the old entry, removal by VMID, `known_hosts` both forms and the no-op; no SSH config when the create job fails |
| `test_addkey.py::LocalTests` (6) | `private_key_for`, `register_identity` (appended once, file and mode kept; refused for another section or a multi-line array), the lines with mixed key kinds, `sync_local_ssh`, a local failure reported as `local_error` |
| `test_lifecycle.py` (7) | create result `ssh` alias; SSH config failure after a verified create; destroy: local steps only after the VM is absent and in order, the failure message, the result view, `gib_value`; backup keys left untouched when the job fails |
| `test_status.py` (10) | the view functions: free percent and its colours, the unit and precision, the storage table's columns and offline state, the summary lines, the volume lines |
| `test_fido.py` (18) | `fido` module and the master's argv (built by a function, run through an injected runner) |
| `test_touch.py` (11 of 12) | `touch` module with an injected player and clock; `test_herdr_registration_plays_failure_cue_on_nonzero_exit` moves to phase 14e (Herdr setup) |
| `test_discovery.py` (2) | hardware identities; discover issues only reads |

**New:**

- the CLI end to end against in-process servers on `pve-sim` behind
  HTTPS;
- parity of `status --json` between the Python CLI and the Rust CLI on the
  same simulated cluster: four comparisons added to
  `scenarios/status-cluster.toml` (`rust = "cli ..."`, rules
  `cli-status-global` and `cli-status-workspace`).

**Gate (`--live`):**

- `homestack` on the test instances (`live-deploy.toml`, deployed for the
  run and removed after), with VM 240 in `homestack-test`, in this order:
  `create --yes --json`, `status`, `refresh --yes`, `destroy --yes`;
- the local SSH files are written to a temporary `HOME`;
- the exit codes are checked, and the plan without `--yes` must exit 3.

## 4. What was built

- **`crates/hs-cli`, binary `homestack`:**
  - `args`: the parser;
  - `config`: `client.toml`, the token and the CA;
  - `app`: the commands, the confirmations and following the job;
  - `view`: the panels and tables of `ui.py`;
  - `local_ssh`: the SSH config, `known_hosts` and the hardware identities
    (I1);
  - `identity`: `private_key_for`, `register_identity`, `sync_local_ssh`;
  - `fido`, `touch` and `master`: I2;
  - `system`: local commands and the terminal, both replaceable in tests.
- **`hs-config`:**
  - `[server]` in `client.toml`: `urls` (https only), `token_file`,
    `ca_file`;
  - `split --server-url URL` (repeatable) writes that section.
- **`hs-client`:**
  - `Session::apply_with` (the secrets of the apply, decision 6) and
    `Session::config_version`;
  - `Client::get_from` (the answer and the instance that gave it).
- **`pve-sim`:** `tls` is public, so tests can serve the server over HTTPS.
- **Parity:**
  - `rust = "cli ARGS"` in scenarios runs the client in-process against the
    server behind HTTPS;
  - rules `cli-status-global` and `cli-status-workspace`;
  - phases `13b` and `13c` added to the manifest's list.
- **`scripts/check.sh --live`:** the client gate (`live_cli.rs`) runs after
  the deployment gate.

## 5. Results

**Tests:**

- 71 Python tests ported. Manifest totals: 593 Python tests, 207 ported,
  309 to do, 77 excluded.
- `test_herdr_registration_plays_failure_cue_on_nonzero_exit` moved to
  phase 14e.
- 29 installer tests (`test_install.py`, `InstallDraftTests`) moved to
  phase 13c.

**New tests:**

- `cli.rs` (19, all on `pve-sim` behind HTTPS):
  - every operation asks before any change;
  - the LUKS passphrase is asked only interactively;
  - the status of an absent VM keeps its shape;
  - `http://` is refused;
  - Python-only commands say where they are;
  - migrate end to end.
- `hs-config`: `[server]` takes only HTTPS.

**`scripts/check.sh`:** all passed.

**Parity with Python:** 16 comparisons in `status-cluster.toml`, of them 4
new for the client (`status`, `status test1`, `status 205`, the absent
`status 239`), all equal after their rules. A changed target made them
fail, so they do compare.

**Live (2026-10-13), `scripts/check.sh --live`:** all passed.

- Image `ghcr.io/hipotures/homestack-server:0.1.0-081c65607888`, test
  instances `api1`–`api3` (CT 997–999).
- `homestack` ran, in order:
  1. `status 240` (absent)
  2. `create 240 hst-cli13 --json` (exit 3)
  3. `create ... --json --yes` (exit 0; SSH config with `HostName
     192.168.100.240`, `ssh root@hst-cli13`)
  4. `status hst-cli13`
  5. `status`
  6. `transport`
  7. `refresh --json` (exit 3)
  8. `refresh --json --yes` (exit 0)
  9. `destroy 240 --json --yes` (exit 0; SSH config removed)
  10. `status 240` (absent)
- Containers removed after; no CT 997–999 and no VM 240 left.

## 6. Open questions

1. **A missing `~/.ssh/known_hosts` stops `create` after the VM is
   verified, in Python and here alike.** `ssh-keygen -F` exits 255 ("Cannot
   stat"), which `forget_local_ssh_host` treats as an error. It looks like
   a bug in Python; it is reported, not fixed (PLAN.md 1.4). Should a
   missing file count as "nothing to remove"?
2. **BK retrieval keys at `destroy`:** listed under `backup_keys_left`, not
   removed, until BK is ported (phase 14d).
3. **`install`** is phase 13c (owner's decision). Until then `client.toml`
   comes from `hs-config split --server-url ...` or is written by hand.
4. **Phase 13b** (the TUI) is next; it uses `hs-client` and the job events
   over WebSocket instead of polling.
