# Phase 13b: the HomeStack TUI

Status: in progress (step 1). This file is written before the code (PLAN.md
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
