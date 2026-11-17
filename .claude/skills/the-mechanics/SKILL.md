---
name: the-mechanics
description: Apply The Mechanics design system (Sharp Retro-Tech — ink-framed panels, IBM Plex Sans/Mono, Light/Dark/Coffee themes, `--tm-*` tokens, `.tm-*` classes) to an application's UI and verify the result. Use this whenever you build, restyle or review screens in a project that uses or adopts The Mechanics — it imports `the-mechanics.css`, uses `tm-` classes or `data-tm-theme`, or the user mentions The Mechanics, "TM styles", "retro-tech", ink frames, the gallery or the three themes — including tables, forms, dashboards, charts, dialogs or settings pages in that project, even if the user does not name the design system. Do not use it for projects that use a different design system.
---

# The Mechanics — applying the design system

The Mechanics is a CSS-only design system: compiled CSS, three themes, no runtime JavaScript. A consuming app gets the look from **class names and native/ARIA state attributes**, never from re-creating the styles. The rules below are owner decisions; most are not visible in the CSS itself, which is why this skill exists: an agent that only reads the class list will invent rounded cards, blue focus rings and side bars that were explicitly rejected.

Source of truth (pin the version you use; the installed version is in `.the-mechanics-version` next to this file when it came from a release):
- `references/DESIGN.md` — full rules, tokens, component table, chart grammar, anti-patterns. Bundled with release installs; in a checkout of the repository it is `DESIGN.md` at the root.
- `references/distribution.md` — how to install and import per framework (`docs/distribution.md` in the repository)
- Repository: `github.com/hipotures/the-mechanics` (local checkout often at `~/DEV/the-mechanics`)
- Component Gallery — run the playground (`corepack pnpm build && corepack pnpm preview`) and open `#/components`; every component in every state, in all three themes

Read the relevant `DESIGN.md` section before inventing anything. This skill is the short version; when they disagree, `DESIGN.md` at the pinned version wins.

## 1. Before you start

0. **Respect the project's own rules** (AGENTS.md / CLAUDE.md: temp directories, memory limits, branches). They win over anything here.
1. **Confirm the project is allowed to use it.** Integration into another app is an owner decision. If the project does not already import The Mechanics and nobody asked for it, ask first.
2. **Find how it is wired.** Look for `the-mechanics.css` / `fonts.css` imports, `class="tm-root"` on the body or app root, and `data-tm-theme` on `<html>`. If missing, follow `docs/distribution.md` (plain HTML, React, Svelte, Rust/Datastar, Tailwind+daisyUI bridge, ECharts adapter).
3. **Do not add Tailwind/daisyUI (or any framework) just to get the styles.** The CSS is standalone.
4. **Look up the component in the Gallery or `references/recipes.md` before writing markup.** Most needs are already covered; a new component is a last resort and must follow section 2.

## 2. First: decide the emphasis (redesign, not translation)

The most common failure is translating the old page element by element into strong The Mechanics pieces. Every badge, date and button then gets a frame or a color, and the screen shouts everywhere at once — worse than the calm original. Before writing markup:

1. Write down what the user must see in two seconds (e.g. "how much is left, when it resets, what needs attention") — usually two or three things. Those get size, color or a strong control.
2. **Normal state is quiet, abnormal state is loud.** Routine facts ("Enabled · authenticated · pro", "Resets …", "Last check …", "Ends in …") are muted text (`--tm-text-2`, `.tm-meta`, or `.tm-badge--plain`), not tinted badges or framed boxes. Tinted badges, plates and red/amber are for deviations only.
3. **Emphasis follows frequency.** A rare page-level action (Add account, settings) is a quiet ghost icon button with a tooltip, not a filled primary. **One strong action per record.** The everyday action is framed/primary; the rest (Details, Reauthenticate, Disable, Delete) are ghost buttons that read like quiet text until hovered. Destructive actions get their red in the confirmation dialog.
4. **Frames organise, they do not decorate.** Regions, panels, cards and real controls have frames. Data and metadata never do.
5. **Inactive records go quiet as a whole.** Disabled, paused, expired or stale data is the past, not the present: neutral bars, muted values, dates and warnings (all color goes) — the dimming is the message, no extra note that changes the card height; hide or disable actions that need an active record, keep the others normal. No opacity on the whole card.
6. **Panels: join only a topic.** Shared-divider grids (`.tm-panel-grid`) are for parts of one topic; independent records with their own actions are separate panels with a gap.
7. **Markers never outgrow their text** — and at text size they are drawn SVG (solid disc + surface-colored glyph), never a font "!"/"i" inside a stroked circle, which blurs at 1×; size them like round letters with optical overshoot (~0.82em, dipping ~0.06em below the baseline, whole pixels). **Also: transient problems are toasts.** A per-item problem goes on that item's line in smaller text ("5-hour · limit reached"), no big icon. Failed checks, timeouts and stale data become a toast naming the record — shown while the problem lasts, removed when it clears, ✕ hides it until a new one — and the record shows **no problem message or marker**, only its data (a quiet fact such as "· stale" is fine). While the toast is hovered or focused, the record's frame takes the toast's tone **at double weight** (border color + outer outline of the same width — nothing moves); a recolor alone is invisible in Dark/Coffee. The toast title scrolls to the record.
8. **Details on demand; color only when it signals.** Keep panel bodies to what is read every visit; put secondary facts behind a text-sized info mark with a tooltip. A value takes color only when it says something (a newer version), otherwise it is grey. Identity colors (agents, products) come from the subject — the app's own or brand colors — not from the chart palette, and keep their hue in every state (exhausted = darker, not a warning hue).
9. **Stable facts in the head, live data in the body** (plan next to the name, lighter: "Lab account · max"). **Sibling cards share levels; say each thing once; confirm actions.** Rows present in every card start at the same height across the row — put a control only some cards have on an existing line, small, with the line's height reserved everywhere. Do not repeat in text what the visual state already says, and do not state normal states. Each user action gets a success toast that leaves by itself (~4 s); errors stay until closed. A trigger turns busy only after its confirmation dialog is accepted, never while the user is deciding.
10. **Color has at most three jobs:** identity (a provider/series name), data (bars, charts) and attention (warning, error). If something is colored for another reason, mute it.
11. Then compare your screenshot with the original side by side: if yours looks busier, remove emphasis until it is at least as calm. `DESIGN.md` §1 "Emphasis budget".

## 3. The rules that matter most

Read `references/rules.md` for the reasoning and edge cases. In short:

- **Structure:** free-standing objects (panels, tables, menus, dialogs, controls) get a **2px ink frame**; dividers that split a container are 2px ink; lines that touch other lines are thin. No rounded cards (radius ≤ 3px), no pills, no soft shadows on panels, **no colored side bars** anywhere.
- **Three-level hierarchy:** region > panel > record. A nested header never competes with its parent.
- **One item grammar** for everything pickable (nav, in-page nav, list rows, menu/select options, palette results, picker cells): hover = grey block; selected = **selection fill only** (no frame, no tick); keyboard focus = the block's frame in ink. Geometry never changes between states.
- **Two "on" signals, never swapped:** *chosen among siblings* → **selection fill** (green/olive/copper: nav item, segmented option, category chip, selected row, the bulk bar). *A control's own on-state* → **ink fill** (checked checkbox, switch, count badge, `.tm-btn--toggle`, `.tm-meta-toggle`). Ink, because controls often sit inside a selected area and green on green would vanish.
- **State changes never move the layout.** Busy buttons keep their size (`aria-busy` hides the label in place and centres a spinner after 250 ms — never swap the text to "Saving…" or add a spinner beside it); counters reserve width; messages appear on existing lines.
- **Toggles never relabel.** A *view toggle* (filter or display option such as "Selected only", "Hide free") keeps its label and icon and changes only its fill. Nothing in a toolbar may shift when state changes (fixed labels, reserved counter width, optional buttons appended last). This is about view toggles only: an *action* that changes a record ("Enable" / "Disable", "Pause" / "Resume") may name the action it performs. A persistent on/off *setting* of a record is better shown as a `.tm-switch` with a label than as an ink-filled button, which would compete with the primary action.
- **Focus = lift:** controls keep their frame and get a hard ink shadow `--tm-focus-lift` (3px 3px, no blur); composites (stepper, input group, date field) lift as one piece and mark the focused part with the press tint; items get an ink block frame. Never a blue ring, never two focus looks.
- **No browser popups for meaning:** tooltips are `.tm-tip`, or `data-tm-tip="…"` on cells/icons/truncated text (port `enhanceTips` from `src/playground/lib.js`) — never `title`.
- **No browser dialogs.** Replace `window.confirm`, `alert` and `prompt` with `<dialog class="tm-dialog">` — destructive confirmations get a danger confirm button (`tm-btn--danger`), Cancel focused by default, Escape/backdrop cancel (port `confirmDialog` from `src/playground/lib.js`). Errors that `alert()` showed become an alert or toast.
- **Action weight:** one strong action per record; other actions ghost; destructive last and quiet in the row (ghost), red only in its confirmation dialog. A page-level destructive action in a danger zone may be `tm-btn--danger-outline`. Do not frame every action equally.
- **Grids have no empty cells:** a panel grid whose last row is short leaves a hole in the frame; balance the columns or let the last panel span the gap (see `balanceGrid` in `src/playground/lib.js`).
- **Status:** a standalone status (page head, panel head, KPI, image overlay) is a **status plate** `.tm-status` with a `.tm-mark`; small tinted `.tm-badge` only inside rows, cells and text. Alerts: 2px ink frame + tint + icon; a warning may be hidden and restored from a page-head status button.
- **Text fields have a clear ✕** while they hold text (space reserved, click clears and keeps focus, Escape clears search). Not on number/date/code/read-only fields, or where a cancel ✕ already exists.
- **Layout:** a filter sits next to what it filters; secondary information goes in collapsible sections; destructive actions last, in a danger zone; report real state — no invented progress bars. A small display option goes **on the existing element** (e.g. the unit in the panel head as `.tm-meta-toggle`), not in a new toolbar row.
- **Charts:** Apache ECharts styled through `adapters/echarts.js`; mono 11px medium labels; named categories always fully labelled (switch to horizontal bars instead of rotating or hiding); every value also reachable by tooltip and keyboard.
- **Themes change color only**, never geometry. Check every screen in Light, Dark and Coffee.
- **Data in examples is synthetic.** Never real names, customers or copied text.

## 4. Workflow

Restyling changes the look, not the product: keep the app's wording, features and behaviour. If a rule seems to require changing what a control does or what it is called, propose it in your report instead of doing it.

1. Map each piece of the screen to an existing component (Gallery / `references/recipes.md`). Write markup with `tm-` classes and express state with attributes (`aria-pressed`, `aria-selected`, `aria-current`, `aria-invalid`, `aria-busy`, `disabled`, `[open]`).
2. Put app-specific layout in the app's own CSS (unlayered, so it wins without `!important`). Use `--tm-*` tokens for every color, space, size and border, never hex values or ad-hoc pixel spacing.
3. Behavior comes from native elements (`<dialog>`, `<details>`, `popover`, form controls). For richer behavior (select fallback, date/time picker, clear button, restorable alert, multi-select, command palette, chart tooltips), port the reference helpers from the playground (`src/playground/lib.js`, `p1.js`, `charts.js`) instead of writing new ones with a different feel.
4. Run both audits: the source audit `node <skill-dir>/scripts/audit.mjs <app-src-dir>` and the rendered audit `node <skill-dir>/scripts/ui-audit.mjs <page-url> [<url>…]` (needs Playwright; checks every theme for radii, soft shadows, non-Plex fonts, off-token colors, controls that still look like the browser/OS, `title` tooltips, system dialogs and validation bubbles). Forms validate inline (`novalidate` + `validateInline`/`setFieldError` from `src/playground/lib.js`), never with browser bubbles. It flags the rejected patterns (large radii, pills, blurred panel shadows, colored side bars, blue focus rings, hard-coded colors, relabelling toggles). Treat its output as leads to inspect, not verdicts.
5. **Look at it** — and judge emphasis first: where does the eye land, and is that the thing that matters? Then take screenshots (Playwright or the project's own tooling) in Light, Dark and Coffee, at desktop and ~390px phone width, and inspect them yourself. Check: frames and hierarchy, nothing shifts when you toggle or select, focus by Tab looks like the lift everywhere, no horizontal page scroll, long text wraps or truncates with a tooltip, contrast of anything you colored. A successful build or passing DOM checks is not visual verification.
6. Report what you inspected (themes, widths, interactions) and what you could not check. Do not claim accessibility compliance you did not measure.

## 5. When the system does not cover it

Prefer composition of existing pieces. If something new is unavoidable: build it from the same tokens and grammar (2px ink frame if free-standing, item grammar for pickable parts, the right "on" signal, lift focus), add it to the app, and tell the user it is a candidate for The Mechanics itself rather than silently diverging. Do not edit The Mechanics repository from another project.
