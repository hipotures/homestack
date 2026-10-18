# Phase 13b: the HomeStack TUI

Status: steps 1–5 done; the live gate is open. This file is written before the code (PLAN.md
1.1) and updated as the phase goes; the report at the end says what was done
and what differs.

## 1. What it is

One full-screen program for the install, the everyday work and the
uninstall (owner's decisions of 2026-10-14 in `decisions.md`). Plain
`homestack` in a terminal opens it; every CLI command stays as it is. The
design is the mockups in `docs/tui/mockups/` (`00-system.txt` explains them);
this phase builds them.

The installer of phase 13c (its steps 1 and 2 are done and were verified live
by the owner) moves into the TUI: the Proxmox part, the servers and
`client.toml` stay the same code; only the way the user is asked changes.

## 2. Python behaviour that applies

Python has no TUI for the operations: its only Textual screen is the setup
selector (`src/homestack/setup_tui.py`, tests `test_setup_tui.py` and
`test_setup_structured_ui.py`, 49 tests, `excluded` in phase 0). They describe
setup (phase 14) and are reviewed with the setup box (step 5). Everything else
the TUI does is an existing CLI command, and the TUI calls the same code:
the same requests to the server, the same plans, the same local effects (I1,
I2), the same refusals. The TUI adds no rule of its own about Proxmox.

## 3. Steps

1. **The frame and the main screen, read-only.**
   - Themes, the border grammar of the mockups, the size guard (80 x 24).
   - Panels `¹workspaces ²cluster ³storage ⁴log` that the digits show and
     hide; presets; the automatic fit to the window.
   - The status from the server, refreshed in the background; the details of
     the selected workspace.
   - The menu with Check the servers (`transport`), Check this desktop
     (`discover`), Options (theme, presets), Help and Quit.
   - `homestack` without a command opens the TUI.
2. **Actions in the workspaces panel.** New, refresh, resize, add key, migrate
   and destroy, each as form › plan › run, with the job's progress. They use
   the code the CLI uses, including the local effects (SSH config,
   `known_hosts`, `identity_files`).
3. **Install and uninstall in the TUI.**
   - The install screen of the mockups replaces the form of 13c.
   - ssh to root runs without the terminal. Its questions come back to the
     TUI: the touch, a PIN, an unknown host key. Herdr panes are a choice.
4. **Start/stop and ssh.** Start and stop need new server operations; how ssh
   runs is decided after the TUI is built (owner, 2026-10-14).
5. **Setup and repo** (phase 14 code): the setup box with its groups and
   repositories.

**Gate** (PLAN.md): `--live`: from the TUI, a plan and its run on a test VM
through the server, with the job's progress.

## 4. Step 1 in detail

### 4.1 Where things live

- `crates/hs-cli/src/tui/`:
  - `theme.rs`: themes and colours;
  - `chrome.rs`: the border grammar (titles, hotkeys, buttons, counters,
    scrollbar);
  - `layout.rs`: which panels fit and where;
  - `state.rs`: what is selected and shown;
  - `data.rs`: the status snapshot and its refresh;
  - `draw.rs`: drawing;
  - `run.rs`: the terminal and the event loop.
- Preferences live in `~/.config/homestack/tui.toml`: theme, colour
  overrides, panels shown, presets.
  - It is a separate file from `client.toml`, so the theme can be chosen
    before anything is installed.
  - It is never sent to a server.
  - It is written as MyPowers writes its preferences: a new temporary file
    with mode 0600, then renamed over the old one.

### 4.2 Themes (as MyPowers)

- The list matches MyPowers: `HomeStack` (the default), `Catppuccin`, `Nord`,
  `Gruvbox`, `Tokyo Night`, `Solarized`, `Terminal`. The six named ones come
  from the same `ratcn` presets as MyPowers (`ratcn = "=0.0.5"`).
- `HomeStack` follows the owner's reference picture:
  - dark blue-grey background;
  - light grey text;
  - blue accent for focus and the active item;
  - teal for values and "running";
  - muted grey-blue for secondary text and borders.
- The roles beyond ratcn's are derived from the theme: hotkey (red, from
  `destructive`), value and ok (teal or green), info.
- `[colors]` in `tui.toml` overrides single roles with `#RRGGBB`, as in
  MyPowers. An unknown theme, role or colour is refused with a message.
- `NO_COLOR` gives the `Terminal` theme without colours.

### 4.3 The border grammar

- Titles in a top border are framed `┐title┌`; buttons in a bottom border are
  framed `┘label└`.
- The panel digit is a superscript before the name, in the hotkey colour.
- A hotkey is one letter of its label, in the hotkey colour.
- Counters sit at the bottom right.
- The scrollbar replaces the right border (`↑ █ ↓`) when the list is longer
  than the panel.
- Controls that do nothing now (no row selected) are drawn muted.

### 4.4 What step 1 is tested with (written before the code)

Rendering into ratatui's `TestBackend`, as MyPowers does, with a fake status:

- below 80 x 24 only the size notice is drawn, with both sizes;
- at 80 x 24 the cluster and workspaces panels are drawn and storage and log
  are not; at 120 x 36 all four; at 160 x 45 the extra columns;
- the digits hide and show panels; with all others hidden the workspaces
  panel holds the menu and preset controls;
- the border titles and buttons are framed as in 4.3, and the hotkey letter
  has the hotkey colour while the rest of the label does not;
- the actions are muted without a selection and normal with one; the
  counter says `0/12` and `1/12`;
- the scrollbar appears only when the list is longer than the panel;
- each theme gives the expected colours, `[colors]` overrides a role, and
  bad values are refused;
- `tui.toml` is written 0600 and keeps the other keys;
- an unreachable server shows the servers that did not answer and why;
- `homestack` with no command opens the TUI only on a terminal and prints the
  help otherwise; `homestack --help` prints the help.

## 5. Step 1: what was done

- **Modules** (`crates/hs-cli/src/tui/`):
  - `theme.rs`: the seven themes, `[colors]` and `tui.toml`;
  - `chrome.rs`: the border grammar;
  - `layout.rs`: panels and window sizes;
  - `state.rs`: the status, selection, filter, node carousel, presets, menu,
    theme list, help and reports;
  - `draw.rs`: all the drawing;
  - `run.rs`: the terminal, the key reader thread, the status and the
    servers' health every 10 s, and the requests started by keys.
- **Shared with the CLI**: the reports of `transport` and `discover` are now
  `app::transport_report` and `app::discovery_report`; the CLI prints them as
  before, and the TUI shows them in a box. No request is new.
- **Entry**: plain `homestack` opens the TUI when stdin and stdout are
  terminals (`tui::wants_tui`); everything else is the CLI as before.
  `homestack --help` lists it.
- **Read-only**: step 1 changes nothing anywhere.
  - Every action in the workspaces border is drawn muted until step 2.
  - Without `client.toml` the workspaces panel says so and points to
    `homestack install` (the install screen is step 3).
- **Tests** (written first, shown failing, then passing):
  - `tests/tui_theme.rs`: 10 tests;
  - `tests/tui_chrome.rs`: 13 tests;
  - `tests/tui_screen.rs`: 19 tests.
- **Seen live** in `tmux` at 80 x 24 and 120 x 36 against the owner's
  cluster (read-only): nodes, servers, Gold, storage, log, and the server
  check box. Colours cannot be checked from `capture-pane`; the owner looks
  at them.

### Differences from the mockups

- The menu has only what works in step 1:
  - Check the servers;
  - Check this desktop;
  - Theme;
  - Help;
  - Quit.

  Reconfigure, Uninstall and Backup on this desktop come with steps 3 and 5.
- The cluster panel cannot tie a server to its node: the client knows only
  the servers' URLs. The servers and Gold are in a box of their own.
- There is no certificate line, because the client does not read the
  certificate.
- Presets are fixed for now:
  - auto: all panels;
  - 1: workspaces and cluster;
  - 2: workspaces only.

  The choice is kept in `tui.toml`.

## 6. Step 2: actions in the workspaces panel

- **Keys** on the selected workspace:
  - `r` refresh;
  - `z` resize;
  - `k` add key;
  - `g` migrate;
  - `d` destroy;
  - `n` new (needs no selection).

  The buttons in the bottom border light up as soon as a row is selected and
  no other action is open.
- **The box** (`tui/action.rs`, `tui/acting.rs`, `draw::action_box`):
  - **Position.** At 120 columns and wider it sits beside a narrow list
    (VMID, name, node, state). Below 120 columns it takes the top of the
    panel.
  - **Stages.** They sit in the top border, the current one raised:
    `┐form┌ ✓form …`. Refresh and destroy have no form.
  - **Buttons.** They sit in the bottom border; the focused one is raised.
- **Forms.**
  - **new**: VMID, name, node (default: Gold's), storage (follows the node),
    home size.
  - **resize**: disk (home or root) and the new total size.
  - **add key**: "the keys already in sshkeys" or a `~/.ssh/*.pub`.
  - **migrate**: the other nodes and their storages.

  A bad VMID, name or empty size is said in the form; nothing is sent.
- **Plan.** The worker (`action::work`) asks the server for the plan and shows
  it as the CLI prints it, without the CLI's frame (`action::unframe`), with
  the CLI's question.
  - **Destroy** starts with the focus on cancel.
  - **Resize to the same size** shows the CLI's reason and has no apply.
  - **A LUKS passphrase** is typed into a hidden field in the plan.
  - **Esc** cancels; nothing is applied.
- **Run.** Apply starts the job (`app::run_job`, now shared with the CLI).
  - Each read of the job shows its steps (✓ done, ◐ running, ✗ failed), and
    the row in the list shows `◐ refresh` (etc.).
  - Esc hides the box and the work goes on; ↵ shows it again. A second action
    waits until the first is closed (a log line says so).
- **After the job.** The desktop's part is the CLI's own code, moved out of
  the CLI into shared functions (`app::after_create`, `after_refresh`,
  `after_destroy`, `after_addkey`).
  - The SSH config and `known_hosts` are handled exactly as with the CLI.
  - The result is the CLI's result text.
  - Add key on a running VM asks the CLI's second question ("Restart … now?",
    default: later).
  - The status is read again when the box closes.
- **Not changed:** what the CLI prints and returns.
  - The CLI calls the same moved functions.
  - `tests/cli.rs`, `install_cli.rs` and `views.rs` pass unchanged.
- **Tests:**
  - `tests/tui_actions.rs`: 17 tests (keys, forms, plan, run, hide, result,
    question, passphrase, no-apply plan, narrow box).
  - `tests/tui_work.rs`: 4 tests. They run the worker against the in-process
    server on the Proxmox simulator: the plan in the CLI's words; cancel and a
    closed box change nothing; create with its steps and the SSH config;
    destroy removes it; a refused plan ends with the server's message.
- **Gate** (`tests/live_tui.rs`, in `check.sh --live` after `live_cli`):
  - It deploys the three test instances (CT 997–999).
  - On VM 240 in `homestack-test` it runs, through the TUI's worker: a
    cancelled create (nothing made), then create, refresh and destroy.
  - It checks the plan comes first, every step ends done, and the SSH config
    is written and removed.
  - Then it removes the instances.
- **Seen live (read-only)** in `tmux` on the owner's cluster: the new form and
  the create plan of VM 245 (a plan changes nothing), then Esc.

### The live gate was not passed (2026-10-14)

- **What happened.** The first run of `live_tui.rs` deployed test CT 997
  (api1) on pve1 at `192.168.100.97`. That is the address of the installed
  server CT 115 (api1), which `homestack install` (phase 13c step 2) gave the
  same names and addresses as the live gate's `live-deploy.toml`:
  `api1–3.hs.efez.net` at `.97–.99`.
- **Effect.** The deployment reached the installed server through the shared
  name and was refused (401). Nothing changed there: its configuration
  version stayed the same.
- **Cleanup.** CT 997 stood next to CT 115 on the same address for about a
  minute and was then removed (`homestack-deploy remove`). CT 998 and 999 had
  not been made.
- **Guard** (`DeployConfig::clashes`):
  - `live_deploy.rs`, `live_cli.rs` and `live_tui.rs` compare their instances
    with `~/.config/homestack/deploy.toml` when it exists;
  - on a shared CTID, address or name they refuse before anything is made.
- **To pass the gate** the test instances need their own names and addresses
  (owner's decision: DNS records in `hs.efez.net` are the owner's). Done
  2026-10-14: `test1–3` at `.240–.242` (`live-deploy.toml`); the run then
  stopped at `pve_auth_failed`: the test token `homestack@pve!test` is gone
  (an uninstall of phase 13c deleted the user `homestack@pve` with its
  tokens), and so is the test Gold 248.

## 7. Step 3: install and uninstall in the TUI

- **Where it opens.** Without `client.toml` the TUI starts on the install
  screen (Esc quits). Menu › Install or reconfigure and Menu › Uninstall
  open it over the main screen (Esc goes back).
- **The screen** (`tui/install.rs`, `tui/draw_install.rs`; mockups 02–08,
  18):
  - The steps are in the top border, done ones `✓`, the current one raised
    `┌check┐`; how root is reached and the Proxmox version on the right.
  - The step's buttons are in the bottom border, `n/5` (`n/4` for the
    uninstall) on the right. At 120 x 36 the log panel is under it.
  - **access:** Method `‹ SSH ›` or `‹ Herdr pane ›` (← → on the method;
    Herdr lists its tabs), the host as `USER@HOST`; ↵ connects.
  - **check:** `✓` found, `·` the install adds it, `!` to note, from a
    first plan of the kept choices; "Nothing was changed."
  - **settings:** every question of 13c on the left with its value, the
    chooser of the focused one in a box on the right (⇥ in and out, ␣
    ticks), the server domain as a text row; ↵ reviews.
  - **review:** problems first (Install stays grey while there are any),
    then the same sections as the CLI's confirmation; ↑↓ scroll.
    The uninstall's review lists what is removed and what is kept, with
    cancel focused.
  - **install:** each step as it is done (`✓`, the running one `◐`), then
    the end: `HomeStack is installed.` and ↵ opens the main screen, which
    reads `client.toml` again; a failed run says why and ↵ goes back to
    the review.
- **ssh's questions** (`askpass.rs`): ssh runs under `setsid` with
  `SSH_ASKPASS` set to this `homestack` and `SSH_ASKPASS_REQUIRE=force`;
  `homestack` started that way asks through a Unix socket in a private
  (0700) directory. The TUI shows a box: the touch (with the key and a
  countdown; it closes when ssh stops its notifier), a PIN or passphrase
  (dots), an unknown host key (`Trust ↵`, Esc answers no; the TUI's master
  uses `StrictHostKeyChecking=ask`, the CLI keeps `accept-new`).
- **One install for both** (`install_flow.rs`): `Session` (start, review,
  keep, apply) and `deploy` are what the CLI's form and the TUI's worker
  (`tui/installing.rs`, a thread holding root) both run.
- **Tests:** `tests/askpass.rs` (5: the kinds, an answer, a cancel, the
  touch ending, a fake ssh behind the prefix with no terminal),
  `tests/root_ssh.rs` (the TUI's master behind the prefix asks about host
  keys), `tests/tui_install.rs` (12: every step, the boxes, the uninstall,
  the log panel), `install_cli.rs` (the worker installs the fake cluster
  through the same session: Proxmox part, servers, `client.toml`, the
  status through the new client).
- **Differences from the mockups:**
  - no "each node gets" box in the settings and no grouped progress with
    an overall bar (open since 13c);
  - the run cannot be stopped midway (Esc does nothing while it runs);
  - the touch countdown assumes ssh's 30 s.
- **Live:** the owner installs the servers of `homestack.app` from this
  screen (step 3 replaces the 13c form for that).

### Open after the owner's decisions of 2026-10-14 (`decisions.md`)

- Workspaces numbered by address: the code still makes the address
  `<prefix>.<vmid>`; by the decision VMID 101010 is `192.168.101.10` (the
  third octet times 1000 plus the fourth, both ways).
- The live gates: the test part of Proxmox again (test token, a test Gold)
  and the test addresses of the decision (test services from
  `192.168.102.200`, test workspaces from `192.168.101.200`, numbered the
  same way), names in `homestack.app`.
- The router's zones `hs.efez.net` and `pve1–3.efez.net` go once the
  servers answer at `apiN.homestack.app`.
- Renumbering a workspace to the numbering by address (VMID 206 →
  1010xx): Proxmox cannot renumber a VM, so it is a full copy of the
  stopped VM under the new VMID, the home's serial and its filesystem
  label `HS_HOME_<vmid>`, the address in Cloud-Init, a start with the
  create's checks, the desktop's SSH entry, and the old VM removed last.
  Until then a workspace of the Python HomeStack (2–254) joins by being
  put in pool `homestack` (done for 206 `cpu`, 2026-10-15).

## 8. Step 4: start/stop and ssh

- **start/stop** (`t`): new server operations `vm.start` and
  `vm.stop` (`{"vmid": N}`, events `workspace.started` and
  `workspace.stopped`; `hs_core::lifecycle::plan_power` and
  `power_vm`), shown as the other actions: plan › run. They need the
  servers deployed from this code. The configured Gold can also be powered
  manually in its Gold pool, with `VM.PowerMgmt` and `VM.Config.Options`
  (operation leases) on the Gold role; its
  events are `gold.started` and `gold.stopped`. Inspection never starts it.
- **ssh** (`s`, owner 2026-10-14: decided once the TUI was built): on a
  running workspace the TUI gives its terminal to `ssh` and takes it back
  when ssh ends (`tui/shell.rs`, `run.rs` `hand_over`).
  - The target is the desktop's entry `vm<VMID>` when
    `~/.ssh/config.d/homestack/vm<VMID>-*.conf` exists (its keys, its
    `known_hosts`, the touch of a security key in the terminal as with a
    plain `ssh`); else the user login the server reports (`USER@IP`).
  - The reader of the TUI's keys stops while ssh runs, so no key typed for
    ssh is taken; the TUI ignores SIGINT, so Ctrl+C before ssh's session
    does not end it.
  - A failed ssh (not 0) leaves its message on the screen until Enter; the
    log panel gets how it ended.
  - Not done: ssh in a Herdr pane or another terminal window.
- **Tests:** `tests/tui_shell.rs` (5: the key on a running and a stopped
  workspace, the button's colour, the target from the entry or the login,
  the log line); `tests/tui_actions.rs` for start/stop.

## 9. Step 5: setup in the TUI

The setup box is Python's Textual setup app (`setup_tui.py`) in the TUI;
it runs the setup of phase 14 (`setup::engine`), the same as `setup
TARGET SELECTORS…`.

- **Opening it:** `e` (└sEtup┘) on a running workspace; Menu › Set up
  this desktop (Python `setup --local`: only Backup). `setup TARGET`
  without selectors says to open it here.
- **Start** (`tui/setup_work.rs`, a thread of its own): the workspace is
  resolved through the server, the catalog loaded with the owner's
  repositories and its snapshot saved, then one connection is opened and
  kept until setup opens on another workspace (the box opens again without
  a touch). The touch
  of the security key is asked in the TUI's touch box, through the askpass
  bridge of the install; then the state is read (`inspect_workspace_state`).
  The box shows each step with a spinner; a failure is shown in the box.
- **Layout:** the box takes the whole workspaces panel (one frame, as
  btop's panels; the workspace buttons go while it is open) and hides the
  storage and log panels for its width; they come back when it closes. A
  bar lists every group with its count; each item has a STATE column (✓
  ready, ! will be overwritten, ○ absent, ? unknown) and, once selected,
  what applying it does (→ install, → update, → verify).
- **Sections:** each group of the catalog (Files, Environment,
  Applications, Credentials, Backup, Repositories, and any configured) is
  a section: Tab and Shift+Tab walk them, the group shows in the top
  border (`‹ Applications ›`). The first row is the group: Space on it
  toggles the whole group. Inside a section everything works as in
  Python:
  - the tree with tri-state checkboxes; an application's configuration
    files, their sections, the managed values and the file copies under
    it; counters `selected/total`; ← → close and open, ← on a leaf goes to
    its parent; 0 selects the branch;
  - `/` filters; bulk selection affects the visible matches only; the line
    of the target says how many selected items the filter hides;
  - green: detected ready; red: selected update, reapply or overwrite;
  - the details of the highlighted node beside the tree (below it when
    narrow), PageUp/PageDown or the wheel scroll them;
  - the mouse: the checkbox toggles, the name only highlights, the arrow
    opens and closes; a right click does nothing;
  - F5 reads the state again over the same connection, Ctrl+R the catalog
    and the state; a selected item that disappeared stays selected, marked
    removed, until it is deselected.
- **Review and run:** Enter shows the plan (Python's review text), Enter
  or └apply┘ runs it, Esc goes back. The run shows the activity and the
  item states; Esc asks to stop between items (the engine's
  `Observer::cancelled`, Python's raise in `progress`). Interactive
  installers and Herdr's `herdr machine add` get the terminal (the TUI
  lends it as for ssh), with the touch notice and cues of the CLI. After
  the run the state is read again; done items leave the selection.
- **Differences from Python:**
  - one section per group instead of one tree under a root, so a group
    cannot be collapsed;
  - Tab walks the groups (Textual moved the focus between the tree and the
    details);
  - Herdr's desktop registration gets the terminal instead of running
    detached under the modal.
- **Tests:** `tests/tui_setup.rs`: the 49 tests of `test_setup_tui.py` and
  `test_setup_structured_ui.py`, the startup tests of `test_setup_startup.py`
  and the TUI tests of the groups (colours, application files, Backup,
  Herdr), the key `e` and the sections.

### The selection and the details (owner, 2026-10-15)

- The first workspace is selected from the start, and a selection the
  list no longer shows (a filter, the node carousel) moves to the first
  row it shows: there is always a selection when there are rows.
- The details box of the selected workspace opens and closes with `i`
  (└info┘) only, at every width; it no longer opens by itself beside the
  list on a wide screen, so the list keeps its columns. Enter does not
  open it.
- New is about no workspace: its form never takes the selection.

### Storage and log under the list (owner, 2026-10-15)

- Storage and log go under the workspaces list, side by side: the log on
  the left, storage in a column of 36 (48 from 160 columns) on the right;
  one of them alone takes the whole width. The list keeps the whole width
  of the window, as the install keeps it above its log.
- They are 8 rows high (10 from 45 rows) and only while the list keeps 16
  rows; with the list hidden they share the room below the cluster.
- The log scrolls back: the wheel over it, ↑ and ↓ of its scrollbar a
  line, a click or a drag on the track to that place. Scrolled back, the
  lines read stay in place while new ones come. The install's log is the
  same panel and scrolls the same.

### The scrollbar and the mouse (owner, 2026-10-15)

- Every scrollbar of the TUI is one element (`chrome::scrollbar`) that
  says what it scrolls: the workspaces list, the log, the action's plan,
  the setup tree, its details and its review, the install's review.
- The mouse holds its thumb and drags it, also with the pointer off the
  bar, until the button is let go; ↑ and ↓ move a line; a press on the
  track puts the thumb's middle there and holds it; the wheel over the bar
  scrolls three lines. A list scrolled by its selection (the workspaces,
  the setup tree) moves the selection to the bottom row shown.
- The thumb stays shorter than the track while rows do not fit, so it can
  always move.

### The log's highlight and its unread star (owner, 2026-10-15)

- A click on a line of the log highlights it with the list's bar; the
  wheel over the log moves the highlight a line and the view follows it
  (with no highlight the wheel scrolls three lines). The scrollbar takes
  the highlight with the view, onto the nearest line shown. The counter
  says its place, `3/40`, 1 the newest; a new line keeps the highlight on
  its line.
- Scrolled back, new lines do not move the view: they come above it and
  `┐*┌` shows in the log's top right corner until the log is scrolled to
  the newest line, as an editor marks a change. At the newest line new
  lines are seen as they come and there is no star.

### The log starts with the earlier runs (owner, 2026-10-15)

- At the start the log shows the last 200 lines of `tui.log` (from its
  last 256 KiB), muted, under the line `── earlier runs ──`; this run's
  lines come above it.
- `tui.log` lines carry the date now, `YYYY-MM-DD HH:MM:SS  text`; read
  back, a line of another day shows `MM-DD HH:MM:SS`, one of today the time
  only. Lines of before the date was written are read as they are.
- The file is not trimmed.

### Explicit install values and copying terminal text

`homestack tui --ssh USER@HOST --server-domain DOMAIN [--server-image IMAGE]` opens the install
form with the given SSH target and server domain. Both remain editable; the
initial domain is also used for the first plan, and rescans retain subsequent
edits. Explicit install arguments open the form even when `client.toml` exists.
The command requires an interactive terminal.

**F2** enters copy mode on any screen: it freezes drawing and releases mouse
capture so the terminal can select and copy text normally. **F2** or **Esc**
returns to the current state. Background work continues while text is frozen.
The install footer and the help list show the shortcut.
