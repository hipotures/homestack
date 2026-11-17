# The Mechanics — Design Language

**Status:** first reviewable implementation (v0.1.0) for owner review. This document records the *proposed* design language; it is not yet an approved standard for other applications.

Direction: **Sharp Retro-Tech / Structured Technical Minimalism** — precise geometry, a strong hierarchy of primary regions, lighter nested boundaries, compact legible typography, restrained decoration. It should look like precise, useful desktop software: not a rounded-card SaaS dashboard, not a terminal emulator, not a game.

Source of truth for values: `src/the-mechanics/tokens.css` (geometry, type, spacing, motion) and `src/the-mechanics/themes.css` (color). This document explains *why* and *how to use* them.

---

## 1. Principles

1. **Hierarchy by boundary strength.** Importance is shown by how strong a boundary is, not by how many boxes there are. One strong outer frame; medium panels inside; hairlines and tints inside those.
2. **Density without noise.** Compact rows (32 px), 13 px UI text, tabular numerals — but separators get *weaker* as content gets more granular.
3. **Color means something.** Accent = primary action / selection. Status hues = state. Chart hues = series identity. Decoration uses neutrals.
4. **Never color alone.** Every status badge has a marker shape; series have dash patterns or marker styles; selection is a fill whose hue differs from hover (grey), and focus is a shape (ink lift shadow or ink item frame), not a hue; invalid fields get an error-colored border and a message with a “!” glyph.
5. **Themes change paint, never geometry.** Verified by the smoke test: page dimensions are identical in all three themes on all six pages.
6. **No side accent bars.** Elements never get a thick colored bar on their left edge (no `border-left` accents, no inset-shadow bars). State is shown by fill, frame and marker shapes: invalid fields = border in the error color + message; selected items = the inset selection block (see *Item grammar*); alerts and toasts = 2px ink frame + tone tint + icon; code blocks = 1px frame. Allowed exceptions: the blockquote rule and the underline of frameless tabs.
7. **Strong controls, calm structure.** Everything you operate is drawn at full strength: inputs, selects, steppers, buttons, segmented controls, toggle chips, checkboxes, radios and switches use a 2px border in `--tm-ink` (dark ink in Light, bright text color in Dark/Coffee); checked checkboxes/radios/switches are filled with ink and carry a surface-colored mark. Structure (regions, panels, rows) keeps its own, calmer hierarchy. Control heights do not change with border width (border-box). Selected segments and chips are marked by the selection fill inside the ink frame (chips also switch to a selection-colored frame) — no underline.
8. **One hover language.** Every clickable control signals hover by a fill tint (`--tm-hover`), never by changing its border; selection never changes font weight (widths stay constant). Only form fields darken their border on hover. Containers that are not clickable (panels, product cards, regions) do not react to hover.
9. **Emphasis budget (owner rule).** Strong controls are only strong if few things are strong. Decide first what a screen must say in two seconds, then spend emphasis there:
   - *Normal state is quiet, abnormal state is loud.* "Enabled · authenticated · pro" is muted text (`--tm-text-2`, or `.tm-badge--plain`); tinted badges, plates and color are for deviations (disabled, expired, stale, limit reached, failed).
   - *Emphasis follows frequency.* A rare action (add an account, settings) is a quiet icon button with a tooltip, even when it is page-level; the filled primary button is for the action people come to the page to do, if there is one at all.
   - *One strong action per record* (framed or primary); other actions are ghost/text buttons; destructive ones turn red in their confirmation, not in every row.
   - *Frames organise, they do not decorate:* panels and controls have frames; data, dates, periods and metadata are plain (muted) text, never boxed.
   - *Color has at most three jobs on a screen:* identity (e.g. provider name), data (bars, charts) and attention (warning/error).
   - *Inactive records go quiet as a whole.* When a record is disabled, paused, expired or its data is stale, its figures are a record of the past, not the current state: bars turn neutral (no identity/status color), values and dates use muted text, and warnings go quiet too ("limit reached", "stale" — the figures they describe are the past), and the dimmed treatment itself is the message — no extra note (it would repeat the last-check time and change the card's height); marker badges are for columns of mixed states in tables. Actions that need the record active (check now, start, statistics, schedules) are hidden or disabled; actions that still make sense (enable, details, reauthenticate, delete) stay normal. Never fade the whole card with opacity — that makes the remaining actions unreadable and look unclickable. (Same idea as the stale chart panel: hatched, reduced, labelled.)
   - *Join panels only into a topic, never into one block.* A framed panel grid with shared dividers is for a group of parts that form one topic (an island such as "Traffic & throughput"); several topics are several islands with space between them. Independent records with their own actions (accounts, runtimes, products) are separate free-standing panels with a gap; a short last row is just canvas.
   - *Text-sized marks are drawn, solid.* At 1em a stroked circle around a font glyph blurs into a blob (the dot of "!" touches the ring). Small problem/info marks are SVG: a solid disc in the state color (grey for info) with a surface-colored glyph drawn as shapes, with a clear gap between stem and dot. Size them like round letters, with optical overshoot: about 0.82em, slightly above cap height and dipping ~0.06em below the baseline (a circle exactly from baseline to cap height looks small, floating and flat-bottomed), with the size rounded to whole pixels so edges stay crisp at 1×.
   - *Markers never outgrow their text.* An icon or status mark next to text is at most the height of that text line. A problem that concerns one item sits on that item's line, smaller than its label ("5-hour · limit reached"), not as a separate line with a big icon.
   - *Transient problems are notifications, not card text.* A failed check, a timeout or stale data of one record is a toast that names the record ("Lab account — usage check timed out…"): it stays while the problem lasts, disappears by itself when the problem clears, and ✕ hides it until a new problem appears. Toasts do not pile up a history. The record carries **no problem message or marker** — only its data, which may include a quiet fact ("Last check 14:17 · stale"). The toast is matched to its record the other way round: while the toast is hovered or focused, the record's frame takes the toast's tone **and doubles in weight** (frame color + an outer outline of the same width; outline takes no space, so nothing moves). Hue alone is not enough: in Dark and Coffee the ink frame and the state colors are both light, so a recolored frame of the same weight is barely visible (owner review). The toast title is a quiet button that scrolls to the record. The next failed attempt brings a dismissed toast back (dismissing acknowledges this occurrence, it does not mute the problem). The same goes for errors that are not about one record: a failed action is a danger toast that stays until closed (the same message is not stacked twice), and a lost connection is a condition toast that leaves by itself on reconnect — no alert is inserted above the lists, which would push the whole page down.
   - *Details on demand.* A panel body shows only what is read every visit (here: the last check); secondary facts (retained version, verification time, last result) sit behind a small text-sized info mark beside the title, in a tooltip.
   - *Color a value only when it signals something.* A version number is grey when it equals the installed one and takes the identity color only when a newer one exists — the color itself is the message.
   - *Identity colors belong to the subject, not to the palette.* Product/agent colors come from the product (its brand or the app's established colors), are tuned per theme for contrast, and stay the same hue in every state: an exhausted bar is the same color a step darker, never another hue — a warning hue on one agent's bar reads as a different agent.
   - *Stable facts in the head, live data in the body.* What rarely changes (provider, account name, plan) belongs in the card head next to the name, in lighter text ("Lab account · max"); the body is for values that change between visits.
   - *Sibling cards share their levels.* Rows that exist in every card (account line, first limit, footer) start at the same height across a row of cards; a control that only some cards have (a group switch) goes on an existing line in its small size, and that line reserves the same height in every card.
   - *Say each thing once.* A state already shown visually (a dimmed card = disabled) is not repeated in text; normal states ("enabled", "authenticated") are not stated at all — only deviations ("expired").
   - *Busy starts after the decision.* An action that asks for confirmation shows nothing while the dialog is open; the trigger turns busy only after the user confirms, the work runs in the background, and the result arrives as a toast (and, for a delete, the record leaves).
   - *Every action the user takes is confirmed, errors are kept.* After an action (enable/disable, check, upgrade, rollback, save) a success toast names the object and what happened, and leaves by itself after ~4 s (paused on hover). Failures stay until closed. Actions that open a dialog or view need no toast.
   - *State changes never move the layout.* Busy buttons, toggles, counters, appearing messages and optional buttons keep their size and position (see Button, toggles, bulk bar).
   - Restyling an existing app is a redesign, not a translation: keep its features and wording, but rebuild the emphasis from what the user needs, not element by element from the old DOM.
10. **Native first.** `<dialog>`, `<details>`, `popover`, real inputs and buttons. This keeps the CSS portable to any framework or server-rendered stack.

## 2. Three-level visual hierarchy

| Level | Use | Border | Header band | Examples |
|---|---|---|---|---|
| **1 · Region** | App shell parts and major page sections | `2px` `--tm-line-1` (ink) | `--tm-head-1`, display font, rule below | `.tm-topbar`, `.tm-sidebar`, `.tm-region`, `.tm-kpi-strip`, dialogs |
| **2 · Panel** | Functional panels, tables, cards | `2px` ink when free-standing; inside a region split by 2px ink dividers instead of own frames (see *Structure rule*) | `--tm-head-2`, UI font semibold 13px, 1px rule below | `.tm-panel`, `.tm-table-wrap`, `.tm-card`, `.tm-product` |
| **3 · Record** | Nested groups, rows, block labels | `1px` `--tm-line-3` (hairline) or background tint only | `--tm-sunken`, mono 11px | `.tm-record`, `.tm-group`, `.tm-row`, table rows |

Rules:

- A nested header must never use a heavier border, larger type or darker band than its parent.
- Panels inside a region with many siblings use **`.tm-panel-grid`** / **`.tm-cells`**: panels lose their own frame and are separated by shared 2px ink dividers, so 16 charts read as one instrument cluster instead of 16 cards.
- Statuses inside rows and text are small badges; standalone statuses are plates (see *Standalone vs. inline status*).
- Charts sit directly in the panel body; they never get their own frame.
- Shadows exist only on overlays (dialogs, menus, pickers, toasts) as a hard 4 px offset; regions and panels are flat.

### Structure rule (owner-approved)

1. **Free-standing objects** (nothing touching their frame: page sections, editors, filter panels, product grids, tables and field groups embedded with space around them) get a **2px ink frame**.
2. **Dividers that split a container into parts** (Workspace columns, analytics panel cells, product grid cells, the line under a toolbar) are **2px ink** and meet the frame in T-junctions.
3. **Lines that touch or sit next to other lines** (panel header rule, column header rule, table header rule, data rows, record rows) are **thin** (1px), so lines never stack into heavy bands.
4. **Controls** are always 2px ink. Never nest a framed box flush inside another frame; split with dividers instead (`.tm-cells`, `.tm-panel-grid`).

### Item grammar (one selection language)

Every pickable item — side-nav link, in-page navigation link (`.tm-toc`), selectable list row, menu item, select or multi-select option, command palette result, picker cell (day, hour, minute) — is the same inset block:

| State | Look |
|---|---|
| hover | grey `--tm-hover` block, transparent frame |
| selected / current | `--tm-select-bg` fill; frame `--tm-select-frame` — **transparent by default** (owner decision: fill-only selection); set it to `var(--tm-select-line)` for a framed variant |
| keyboard focus | the same block's frame turns `--tm-focus` (ink; never a second ring around it) |
| selected + focus | selection fill + focus frame |

The block geometry is the same for every item (`--tm-select-inset` 4px / `--tm-select-inset-y` 2px from the container, 2px reserved frame, `--tm-select-pad` 8px / `--tm-select-pad-y` 4px to the content), so nothing moves when an item is selected, hovered or focused. Selectable rows have no separator lines. In Dark and Coffee the selection fill is tuned to stay clearly distinct from hover without a frame. Select options carry no tick: the selected block marks the choice, so option labels sit exactly where the closed field's text sits. Segmented controls follow the same rule — the selected cell is filled and its frame is the control's own outline and ink dividers. Lists and menus are free-standing, so their container has a 2px ink frame. Multi-select options add a small check box in front of the label so "several can be chosen" is visible before anything is chosen; the fill still marks the chosen ones, and the keyboard-active option gets the focus frame. In the command palette focus stays in the search field, so the result that Enter will run (`aria-activedescendant`) carries the selection fill. On touch screens (`hover: none`) nav and in-page links drop the hover tint, so a tapped link never looks selected after the tap.

### Two “on” signals (architectural rule, owner-approved)

There are exactly two ways to show that something is on, and they never swap roles:

| Signal | Means | Used by |
|---|---|---|
| **Selection fill** `--tm-select-bg` (green / olive / copper) | *this item is the chosen one among its siblings* — where you are, which option, which record | side-nav item, in-page nav, segmented control and button group (one-of-N, e.g. 24 h / 7 d / 30 d, Compact / Comfortable), category chips (All / Stationery / …), selected table rows and list rows, menu/select/picker options, pipeline stage in view, pass cell shown in the preview, the bulk-action bar (it exists because records are selected) |
| **Ink fill** `--tm-ink` with inset-colored content | *this control itself is switched on* | checked checkbox, radio dot, switch on, count badge (“3 selected”), on/off toggle button `.tm-btn--toggle` (“Selected only”), panel-head meta toggle `.tm-meta-toggle` (heatmap “%”) |

Why ink for controls: a control usually sits *inside* a selected area — the checkbox of a selected (green) row, the toggle in the (green) bulk bar. Green on green would not show, so control state is always ink, and it reads the same on every background in every theme. Consequences: a toggle's label and icon never change (“Show X / Hide X” swaps move the buttons around it); only its fill changes. New components pick one of the two signals by this test — *choice among siblings* → selection fill; *own on/off state* → ink. Chart legend items are the one exception: they show series visibility by fading the hidden series, not by a fill.

### Standalone vs. inline status

A status that stands on its own (page head, panel head, KPI tile, image overlay) is a **status plate** `.tm-status` (`--sm` in panel heads and overlays): control height, ink frame, state carried by `.tm-mark`. The small tinted `.tm-badge` is only for statuses inside table cells, list rows and lines of text. Alerts are free-standing: 2px ink frame, tone tint and icon, no side bars. A warning can be hidden; a status button then stays in the page head and brings it back (`dismissibleAlert`). Blocking errors cannot be hidden.

## 3. Themes

Selected by `data-tm-theme` on `<html>` (or any subtree). Light is the default.

| Theme | Intent | Canvas → surface | Text | Accent | Focus |
|---|---|---|---|---|---|
| **Light / Paper-Stone** | Stone-gray canvas, cream paper, green-black ink. Surfaces are visibly distinct from canvas (not white-on-white). | `#dcd9cf` → `#efebe1` (shell `#e6e0cc`) | `#22271f` | olive `#44622a` | ink `#2b312a` |
| **Dark / Graphite** | Lifted graphite (canvas `#26292c`, never near-black); muted olive accent, no neon. | `#26292c` → `#33373b` | `#e9ebe6` | `#aac67b` | ink `#dfe2dc` |
| **Coffee / Espresso-Mocha** | Warm mid-dark espresso, mocha surfaces, cream text, muted copper. Distinct from Graphite by hue, not just lightness. | `#33261e` → `#433228` | `#f3e8d8` | copper `#d79560` | ink `#ead9c2` |

Design choices:

- **Focus = lift (owner decision).** A focused control (button, field, select, segmented option, chip, pagination, input group, multi-select) keeps its own frame and gets a hard ink shadow `--tm-focus-lift` (3px right/down, no blur) — it looks raised off the flat page; nothing changes size or moves. Pickable items (nav, rows, options, picker cells) show focus as an ink frame on their block (item grammar). Focus is told apart from selection by *shape* (shadow/frame vs fill), so the two stay distinguishable on the same element in every theme. Composite controls (stepper, input group with affixes, date/time field, multi-select, drop zone) lift as one piece; a focused part inside them (a stepper button, the calendar button) is marked by the press tint, never by a second frame, so there is exactly one focus look. Checkboxes, radios and switches lift too. Tabs, accordion summaries, charts and links keep a 2px ink outline. Text selected inside a field uses the selection fill; the highlighted segment of a native date/time field is a browser system color and cannot be restyled. Native date/time fields keep the browser's display format (12/24 h, field order follow the browser or OS language, not the page `lang`); the `.tm-picker` popup is always Monday-first and 24 h. Firefox draws its own calendar/clock button inside the field with no way to hide it, so in a `.tm-picker-field` our button slides over it (Firefox only). The blue browser-style ring was rejected as foreign to the ink-framed design; a thicker ink frame was the runner-up. The hard shadow is reserved for focus and for floating objects (menus, dialogs, toasts); regions, panels and cards stay flat. **Mouse-opened popups show no focus frame.** Popups move focus in script when they open (the selected option, the first menu item, today's cell). Before the first click on a page browsers paint that as keyboard focus, after it they do not, so the same list looked different on first and later opens (owner review). The optional `data-tm-input` hook on `<html>` (`trackInputModality()`) removes the frame for pointer sessions; the first key press restores it.
- Ink lines (`--tm-line-1`, `--tm-ink`) are dark in Light and light gray / latte in the dark themes. `--tm-line-2` (1px header and table rules) meets ≥3:1 against surface and canvas in every theme. Level 3 hairlines are decorative separators inside a panel.
- The built-in daisyUI `retro`, `coffee`, `caramellatte`, `wireframe` and `cyberpunk` themes were inspected as references (radius/border/depth and base colors). None is used; all three themes are original values.

Contrast for 49 essential pairs per theme is computed by `pnpm check:contrast` → [docs/verification/contrast.md](docs/verification/contrast.md). All pass (4.5:1 text, 3:1 graphics/control boundaries).

## 4. Tokens (public API `--tm-*`)

### Color roles (per theme)
`--tm-chrome`, `--tm-ink` (controls), `--tm-canvas`, `--tm-surface`, `--tm-surface-alt`, `--tm-head-1`, `--tm-head-2`, `--tm-sunken`, `--tm-inset`, `--tm-inverse` · `--tm-text`, `--tm-text-2`, `--tm-text-3`, `--tm-text-inverse`, `--tm-text-disabled` · `--tm-line-1/2/3`, `--tm-line-input`, `--tm-line-disabled` · `--tm-accent`, `-hover`, `-press`, `-text`, `-soft` · `--tm-link`, `--tm-link-hover` · `--tm-hover`, `--tm-press`, `--tm-select-bg`, `--tm-select-line`, `--tm-focus` (plus `--tm-select-frame`, defined in `tokens.css`) · status `--tm-{success|warning|danger|info|pending|neutral|offline}` each with `-soft` and `-line` · charts `--tm-chart-1…8`, `--tm-chart-grid`, `--tm-chart-axis`, `--tm-chart-threshold`, `--tm-chart-target`, `--tm-chart-fill-opacity`, `--tm-heat-lo`, `--tm-heat-hi` · `--tm-shadow-overlay`, `--tm-shadow-raised`, `--tm-backdrop`.

### Typography
| Token | Value | Use |
|---|---|---|
| `--tm-font-ui` | IBM Plex Sans | body, controls, panel titles |
| `--tm-font-mono` / `--tm-font-numeric` | IBM Plex Mono | labels, IDs, numbers, ticks, code |
| `--tm-font-display` | IBM Plex Mono (bold, −0.02em) | page/region titles, hero |
| `--tm-font-prose` | IBM Plex Sans | long-form reading |
| Scale | 11 · 12 · **13** · 14 · 16 · 18 · 22 · 28 · 40 px | 11px is the floor (eyebrows, ticks) |
| Weights | 400 / 500 / 600 / 700 | |
| Line heights | 1.15 tight · 1.3 snug · 1.4 UI · 1.65 prose | |
| Tracking | −0.02em titles · 0.08em uppercase mono labels | |

Why Plex: open license (OFL-1.1), a *soft* monospace with clear `0/O`, `1/l/I` distinction, a matching sans for body text, and complete Latin Extended-A coverage — Polish diacritics (`Zażółć gęślą jaźń`) render in both families in all screenshots. Monospace is used for labels and data, not paragraphs.

### Spacing, geometry, density
- Spacing: 4 px rhythm `--tm-space-1…16` (4, 8, 12, 16, 20, 24, 32, 40, 48, 64).
- Radius: regions/panels **0**, controls/badges **2 px**, overlays **3 px**. No pills.
- Border widths: `--tm-bw-1` 2 px, `--tm-bw-2` 1 px, `--tm-bw-3` 1 px (strength comes from color), `--tm-bw-control` 2 px (all interactive controls, in `--tm-ink`), focus = `--tm-focus-lift` shadow on controls, 2 px ink frame on items, 2 px ink outline + 2 px offset elsewhere.
- Controls: 28 / 32 / 40 px; checkbox/radio 18 px; icons 14 / 16 / 20 px.
- Selection block: `--tm-select-inset` 4px, `--tm-select-inset-y` 2px, `--tm-select-pad` 8px, `--tm-select-pad-y` 4px; `--tm-select-text` / `--tm-select-text-y` give the content offset for unselected rows.
- Density: rows 32 px compact (default) / 40 px comfortable via `.tm-density-comfortable` on any subtree. Toolbar 44 px, topbar 48 px.
- Media: product 4:3, thumbnail 1:1, hero 16:9.

### Layout
Reading width 68ch; content max 1600 px; narrow 1120 px; sidebar 232 px (56 px collapsed); drawer `min(26rem, 100vw)`; dialog `min(30rem, 100vw − 2rem)`. Breakpoints: 640 (stack tables, single-column forms), 900 (sidebar → horizontal strip, layouts stack), 1200, 1440.

### Motion
90 / 150 / 240 ms, `cubic-bezier(.2,0,0,1)`. Only color, opacity and small translations animate. `prefers-reduced-motion: reduce` zeroes durations and stops spinners, shimmer and stripes (verified). Theme switching suppresses transitions for one frame (`.tm-theme-switching`) so the change is instant.

## 5. Component rules

| Component | Rules |
|---|---|
| Button | Variants primary / secondary / outline / ghost / danger / danger-outline. **Outline buttons share the control face** (`--tm-inset`, like fields, selects and segmented controls), so a row of framed controls has one surface on any background; hover/press tints are layered over it; ghost stays transparent (owner review: a transparent outline button beside selects looked like a hole on the canvas); sizes sm / md / lg; icon-only must have `aria-label`. Loading = `aria-busy="true"`: the button keeps its size — after 250 ms (fast actions never flash) the label and icons hide in place and a spinner takes the centre; the label text never changes ("Save" does not become "Saving…"). Disabled = muted surface, gray border and text, `not-allowed` cursor. Pressed = `aria-pressed="true"`. On/off toggles use `.tm-btn--toggle`: the label and icon never change (no “Show X / Hide X” swaps, so nothing around it moves); on = ink fill with inset-colored text, like a checked checkbox. A `.tm-btn-group` with `aria-pressed` is a one-of-N choice and uses the selection fill (see “Two on signals”). |
| Segmented control | Radio inputs inside `.tm-segmented__option` (or `aria-pressed` buttons) in a 2px ink frame with **1px ink dividers**: the options are values of one control, so the lines between them are lighter than its frame (owner decision; 2px dividers drew a grid that outweighed the choice, 1px graphite was the runner-up). The joined `.tm-btn-group` follows the same rule. Test: several things (panel grid, KPI tiles, pipeline stages → 2px dividers) or several values of one thing (→ 1px). Selected = selection fill + full-strength text color (no underline). Selection never changes font weight, so widths never shift. |
| Fields | Label above, hint below, error replaces nothing (both can show). Invalid = `aria-invalid="true"` → red border + `.tm-field__error` with a “!” glyph. Native number spinners are always hidden; use `.tm-stepper` ([−] [unit value] [+], square buttons with full-height dividers, matching control height, buttons disabled at min/max). Read-only = dashed outline, transparent fill. Prefix/suffix via `.tm-input-group` + `.tm-affix`. Every editable text field (text, search, email, url, tel) has a clear button `.tm-input-clear` (✕ with a “Clear” tooltip) on the right while it holds text: space is reserved so the text never moves, a click clears and keeps focus in the field, Escape clears search fields; the browser's own search ✕ is hidden so all browsers match (playground helper `enhanceClearable`, opt out with `data-no-clear`). Number, date/time, code cells and read-only/disabled fields have none. |
| Select | Closed control: field geometry with a drawn caret. Option list: where `appearance: base-select` is supported (Chromium 135+, incl. Android) it is styled like a menu — ink frame, hard shadow, options in the item grammar, **no tick**, option text aligned with the closed field's text, list as wide as the longest label; browsers without it (e.g. Firefox) get a small scripted fallback (`enhanceSelects` in the playground) that keeps the native `<select>` as the source of truth but opens the same styled `.tm-listbox` (keyboard: arrows, Home/End, type-ahead, Enter, Escape). |
| Selection | Square checkbox (2 px radius), round radio, rectangular switch (not pill). Indeterminate shows a bar. Descriptions via `.tm-choice__desc`. |
| Badge | Only inside table cells, list rows and lines of text. Mono 11 px, 1 px tinted border, soft fill, **marker shape per tone** (✓ success, ▲ warning, ✕ danger, ● info, ○ pending, dashed □ offline, — disabled). Count badge is inverse. |
| Status plate | `.tm-status` (32px) / `.tm-status--sm` (28px) for standalone statuses: page head, panel head, KPI tile, image overlay. Ink frame, surface fill, state shown by `.tm-mark` (complete ✓ · active/info dot · review half · warning triangle · error · pending ring · skipped dashed · none faint). As a `<button>` it restores a hidden message. |
| Alert vs toast | Alerts are in-flow, 2px ink frame, tone tint + icon. A warning may be hidden; `dismissibleAlert` leaves a status-plate button in the page head that restores it. Blocking errors cannot be hidden. Toasts use the same frame and are transient (5 s default, pause on hover, progress line); errors are persistent until dismissed. |
| Tooltip | Supplementary only; appears on hover **and** focus; Escape dismisses without moving focus. One tooltip look everywhere (`.tm-tip`): never the browser's `title` popup for information that matters (it differs in color, font and size from ours and is unreachable by keyboard). Tooltips of small inline marks open from the mark toward the content, so they stay inside the card. For cells, icons and truncated text use `data-tm-tip="…"` (+ playground helper `enhanceTips`): the same bubble, placed in fixed position so `overflow: hidden` cannot clip it; `data-tm-tip-overflow` shows it only when the text is truncated, `data-tm-tip-collapsed` only in the collapsed icon rail. |
| Side nav groups | `.tm-nav__group` with a `.tm-nav__toggle` header (label + chevron, `aria-expanded`). A collapsed group still shows its current page; the icon rail and the phone strip always list every destination. |
| In-page navigation | `.tm-toc` (content site, gallery): items in the item grammar — current section = selection fill, no rail and no side bar; on narrow screens the list wraps into rows. Scroll-spy sets `aria-current`. |
| Meta toggle | `.tm-meta-toggle`: a panel-head meta value (usually the unit) that also switches how the panel presents its values — the text in an empty 2px ink frame, on = ink fill; 22px, so the head keeps its height. Use it instead of adding a toolbar row for one small display option; give it an accessible name that says what it switches. |
| Tabs | Underline 3 px in selection color, weight unchanged; arrow-key roving. Prefer no tabs when all content fits as one view plus collapsible sections. |
| Table density | Compact = one line per record (secondary details in the title tooltip, a note marker instead of the note line), 32px minimum rows; Comfortable = full secondary lines, 40px minimum and extra vertical padding. The toggle must make a visible difference, not just a few pixels. |
| Table | Sticky mono header within bounded scroll containers, tabular numerals, numbers right-aligned, negative values in danger color with a real minus sign, selected row = selection fill (+ checked checkbox). On < 640 px `.tm-table--stack` turns rows into labelled records. |
| Menu / popover | Native `popover`, 2px ink frame (free-standing) and hard shadow; items in the item grammar (hover grey, focus frame); arrow keys, Home/End, Escape, focus return. A button that opens a menu ends with `.tm-caret` — the same solid 10×5 triangle and color as the select's caret — so every "opens a list below" control looks alike (owner review: a stroked chevron on "More" beside selects with solid carets read as two systems). Stroked chevrons are reserved for navigation (back / next) and in-place expansion (nav groups, collapsible sections). **Anchored popups stay attached** (select list, menu, date/time picker, multi-select): while the page scrolls they follow their anchor, scrolling inside the popup changes nothing, and they close once the anchor leaves the viewport (`followAnchor()` in `lib.js`). Popups sit fixed in the top layer, so without this they stay put while the field scrolls away (owner review of the date picker). |
| Date / time picker | Native `<input type="date|time">` stays the field (typing, validation); its browser popup is replaced by `.tm-picker` (playground helper `enhanceDateTime`): Monday-first month grid without weekend colouring, 24h hour/minute columns; cells use the item grammar. Keyboard: Alt+↓/F4 or the field button opens; arrows, PageUp/PageDown, Enter, Escape. |
| Workflow | `.tm-pipeline` of `.tm-stage` cells (one framed strip split by dividers; a stage that opens a view is a link), `.tm-mark` status markers in pass matrices, `.tm-passcell` = [status][run] square buttons (status button pressed = shown in the preview), `.tm-section-head` = title + meta + the filter for that section, `.tm-collapsible` (`<details>`: title, count, description, chevron) for secondary information, `.tm-danger-zone` (explanation + destructive action, disabled with a reason when blocked) last on the page. |
| Verification code | `.tm-otp` of `.tm-input.tm-otp__cell` boxes (one character each, mono, grouped with `.tm-otp__sep`), wrapped in `role="group"` with a label. Typing advances, Backspace goes back, paste fills all cells; invalid = `aria-invalid` on every cell + one error message. |
| Multi-select | `.tm-multiselect` = a field holding `.tm-chip`s and a filter input (`role="combobox"`); the list is `.tm-menu.tm-listbox` with `aria-multiselectable`, `.tm-listbox__box` check boxes and a `.tm-listbox__foot` count + Clear. Enter toggles, Backspace in an empty input removes the last chip, Escape closes. |
| Command palette | `.tm-palette` (modal `<dialog>` near the top, 2px ink frame): search field divided from the results by a 2px rule, grouped results (`.tm-palette__group` labels), the matched part of a label underlined, a key-hint footer. Ctrl/⌘+K opens, arrows move, Enter runs, Escape closes. |
| Non-modal sheet | `.tm-sheet` with `popover="manual"`: docked bottom-right (full-width bottom on phones), no backdrop and no focus trap — the page stays usable. Dialog head/body/foot parts; Escape with focus inside or the close button closes it and returns focus to the trigger. |
| Dialog / drawer | Native `<dialog>`; 2 px ink frame; head band `--tm-head-1`; destructive dialogs use a danger confirm button. Escape and backdrop close; focus returns to the trigger. |
| Empty / error / stale | `.tm-state` (+ `--error`, `--warning`). Always title + explanation + (where possible) one recovery action. |
| KPI | Mono value, unit in smaller muted type, trend with arrow + sign + text (“improving/worsening” for screen readers); good/bad depends on metric direction, not on sign. |
| Charts | See §6. |
| Product card | Image first (4:3); body is a stack of plain lines — meta, title (wraps fully, never truncated), description, rating, stock status as a bare badge (marker + text, no frame). Footer is one fixed-height strip of two cells: price left, action button flush with the card frame on the right, so the two share one height and center line and footers align across a row. Never mix a boxed badge, a stacked price and a floating button in the footer. Missing image shows a hatched fallback with text. Lines inside the card (image | body, body | footer, price | action) are **1px ink**: they are parts of one object, while the card frame and the lines between cards stay 2px (same test as the segmented control; owner review: equal 2px lines made the grid of cards read as one grid of cells). |
| Prose | 16 px / 1.65, 68ch, display-font headings, square accent bullets, ink blockquote bar, code blocks in a 1px frame on the inset surface. |

### Layout patterns

- **Overview with pipeline:** page head (back link, eyebrow, title, meta, status plate) → stage pipeline → main table → collapsible secondary sections → page-level action last.
- **List + preview:** two columns split by a 2px divider; the list's search sits in the list head; the preview has no tabs and pages with “Load next N”.
- **Pass matrix:** rows = units/sections, columns = passes; each cell a `.tm-mark` (overview) or a `.tm-passcell` (working view). On phones secondary columns move under the name so all pass columns fit without horizontal scroll.
- **A filter sits next to what it filters** (section head, list head), never in a distant toolbar.
- Report state, not invented progress: no progress bars for work that does not report partial progress.
- **Selection across pages:** the bulk bar counts the whole selection; the header checkbox acts on the visible page; when a full page is selected and more rows match, the bar offers “Select all N matching”; the “Selected only” toggle lists the whole selection (and shows as a removable filter chip). The bar does not repeat what the table already shows, and nothing in it shifts: fixed labels, a two-digit counter width, and the optional “Select all N matching” placed last.

### Documentation-only state classes
`.is-hover`, `.is-active`, `.is-focus`, `.is-selected`, `.is-open` mirror interactive states for galleries and visual tests. Applications should rely on real pseudo-classes and ARIA states.

### Native leftovers

`.tm-root` takes over what the browser would otherwise draw: `color-scheme` per theme, `accent-color` = ink, scrollbar colors, and elements with no class at all (buttons, fields, selects, dialogs, checkboxes, radios, ranges, progress bars) get the same look as their `tm-` counterparts. Some things CSS can never restyle, so the system replaces them instead of using them:

| Browser feature | Use instead |
|---|---|
| `title` tooltip | `.tm-tip` / `data-tm-tip` |
| `alert` / `confirm` / `prompt` | `<dialog class="tm-dialog">` (danger confirm) |
| validation bubbles (`required`, `pattern`, `reportValidity`) | `novalidate` + inline `.tm-field__error` (`validateInline`, `setFieldError`) |
| `<select>` list without `appearance: base-select` (Firefox) | `enhanceSelects` styled list |
| date/time picker | `.tm-picker` (`enhanceDateTime`) |
| file chooser button | `.tm-dropzone` |

Verify with the rendered audit `skills/the-mechanics/scripts/ui-audit.mjs <url>`: it reports, per theme, radii, soft shadows, non-Plex fonts, colors outside the tokens, controls that still draw the browser or OS look, `title` tooltips, system popups and validation bubbles.

## 6. Chart grammar

- **Renderer: Apache ECharts** (SVG renderer). Charts are never hand-drawn; ECharts owns layout, axis label thinning and label-collision handling (`labelLayout: moveOverlap / hideOverlap`). All visual values come from `--tm-*` tokens through `adapters/echarts.js` (`readChartTokens` → `createEChartsTheme`), so charts follow the active theme and are rebuilt on theme switch. KPI sparklines stay as tiny inline SVG (no labels).
- Chart text: IBM Plex Mono **medium (500)**, 11px; category names and data values in full text color, axis numbers and axis units in `--tm-text-2`; legends and panel unit labels also medium. Bold is reserved for tooltip values and the day-boundary date.
- One grammar: hairline horizontal grid (`--tm-chart-grid`), ink baseline, mono 10.5 px ticks, unit label above the y-axis, no chart frame.
- Series order is fixed: chart-1 blue/copper, chart-2 orange/blue, chart-3 green… Series 2–4 are dashed/dotted lines, alternate series use solid vs hollow point markers.
- Thresholds are dashed `--tm-chart-threshold` lines, targets dotted `--tm-chart-target`. The line is named in the legend; the on-chart label is the short value (“750 ms”, “75% target”) at the left end, on a surface-colored background, away from the latest values.
- Point labels that collide are moved apart by ECharts with a leader line; bar value labels that would overlap are hidden rather than drawn over each other. Time-axis labels may be thinned (order is implicit); named categories (routes, models, ranges) always show every label — bars switch to horizontal layout when names do not fit. Data value labels are never hidden. Value axes get one tick per ~64 px.
- Bars with long category labels automatically switch to horizontal layout so labels are never rotated or clipped. Zero values render as a visible 1 px tick.
- Donuts always carry a value + percentage legend; heatmaps print the value in every cell. Cell labels have two viewer-selectable modes, switched by clicking the unit in the panel head (“%”, a `.tm-meta-toggle`; accessible name “Auto label color”) and remembered per browser: **off (default)** — every value in the text color, one calm color across the grid (owner choice); **on** — per cell, whichever of text/surface contrasts more. No halos. Neither mode reaches 4.5:1 on every cell (a continuous ramp crosses mid luminance; figures in `docs/verification/contrast-derived.md`), so every value is also available in the tooltip, the keyboard readout and the live region.
- Tooltips sit below sticky app chrome (`--tm-z-sticky`) and close on scroll or a tap outside the chart, so they never drift away from their data.
- Tooltips: hover **or** keyboard (focus chart, ←/→/Home/End, Escape); values are mirrored into a polite live region.
- **Building blocks for composite time charts** (e.g. “cost over time, stacked by model, 30-minute buckets”): rich panel head `.tm-panel__head--rich` (`.tm-panel__heading` = title + `.tm-panel__subtitle`, `.tm-panel__actions` = legend + metric `.tm-segmented--sm`); ECharts adapter helpers `peakMarker()` (boxed “peak 12:00 285 k tokens” callout), `rangeArea()` (labelled band for quota windows / incidents, tone neutral|warning|danger|success), `rangeBrush()` (drag along x to select a span) and `dayBoundaryLabels()` (midnight shown as the bold date); `.tm-chart-readout` for the live sum of a selection and `.tm-chart-options` for overlay toggles under the chart. Leave ~25 % headroom above the tallest value for the peak callout. A working composition is in the Component Gallery › Data graphics (fixture data only); concrete product charts are assembled from these pieces when needed.
- States: data, no data (`.tm-state`), stale (“Delayed N min” status plate + hatched body + reduced opacity), error (`.tm-state--error` + retry).

## 7. Permitted variations

- Applications may choose comfortable density, wider reading width, or `.tm-panel-grid` vs separate panels depending on task.
- A product may add a fourth theme by supplying every color role in `themes.css`; it must pass `check:contrast`.
- Display font may be switched to the UI font for content-heavy products (`--tm-font-display`).
- Accent hue may be re-tuned per product; status and chart semantics may not.

## 8. Anti-patterns (do not)

- Rounded (>3 px) cards, pill buttons, pill badges.
- Repeating a 2 px ink border on every nested card; nested headers that are bolder/darker than their parent.
- Drop shadows on regions or panels; blurred/frosted layers.
- White-on-white Light surfaces; near-black (#000–#1a1a1a) dark surfaces; neon lime as a default accent.
- Color-only status, selection or series encoding.
- Truncating product titles or data that the user must read; rotating axis labels.
- Random or live chart data in fixtures or screenshots.
- Importing Tailwind/daisyUI into a consumer just to get these styles.

## 9. Decisions for owner review

Settled with the owner: structure rule (2px ink frames and dividers); fill-only selection with one item grammar; status plates vs. badges; compact tables = one line per record; `.tm-panel-grid` with 2px dividers; own date/time picker; menus, side nav and the green primary button kept as they are; Coffee alert/warning hues (reviewed in the gallery, kept); the current dialog weight (reviewed in Coffee, kept — no lighter variant); focus drawn in ink as a “lift” shadow instead of a blue ring; heatmap labels in the text color by default with a viewer toggle for per-cell color (halo rejected); in-page navigation (`.tm-toc`) uses the item grammar instead of a rail with a side bar; collapsible side-nav groups (added for review). Display headings stay **Plex Mono bold**: titles are one or two lines with room to spare, so there is no space to win by switching to the narrower Plex Sans (Sans was compared side by side and declined).

Still open: nothing. (Possible later: a stepped heat ramp that skips the mid-luminance band, if all cells must reach 4.5:1 without a halo.)
