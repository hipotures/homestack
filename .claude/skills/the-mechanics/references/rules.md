# The Mechanics — rules with reasons

Condensed from `DESIGN.md` (bundled as `references/DESIGN.md` in release installs). Each rule says *why*, so you can apply it to cases the list does not name. Section numbers point to `DESIGN.md`.

## Contents
1. Structure and hierarchy
2. Item grammar
3. Two "on" signals
4. Toggles and stability
5. Focus
6. Status, alerts, feedback
7. Fields
8. Tables and selection
9. Layout patterns
10. Charts
11. Themes and tokens
12. Anti-patterns

## 1. Structure and hierarchy (§2)
- Free-standing objects — regions, panels, tables, menus, dialogs, toasts, controls — have a **2px frame in ink** (`--tm-line-1` / `--tm-ink`). Why: the style is "drawn with a technical pen"; strength comes from a few confident lines.
- Dividers that split one container into cells (panel grid, KPI strip, product grid, pipeline) are also 2px ink; the cells share the outer frame instead of each drawing their own.
- Lines that touch other lines (row separators, header rules inside a table) are thin (1px, `--tm-line-2/3`). Why: stacked thick lines turn into noise.
- Three levels: **region** (strongest head band) > **panel** (lighter head) > **record** (subtle). A nested header must never look stronger than its parent.
- Radius ≤ 3px. No pills, no rounded cards, no soft drop shadows on regions or panels. The only shadows are hard offset shadows on floating objects (menus, dialogs, toasts) and the focus lift.
- No colored side bars (left accent strips) on anything — alerts, selected rows, nav, toasts, code. They were removed by owner decision.

## 2. Item grammar (§2)
Every pickable item is the same inset block (4px from the container edge, 2px reserved frame, 8px padding):
| State | Look |
|---|---|
| hover | grey `--tm-hover` fill |
| selected / current | `--tm-select-bg` fill, **no frame** (`--tm-select-frame: transparent`) |
| keyboard focus | the block's frame in `--tm-focus` (ink) |
Applies to side nav, in-page nav (`.tm-toc`), list rows, menu items, select/multi-select options, command palette results, date/time picker cells. Why: one selection language across the whole app; nothing moves when state changes. Select options have no tick. Selectable rows have no separator lines (the blocks separate them). On touch screens the hover tint is dropped so a tapped link does not look selected.

## 3. Two "on" signals (§2, architectural rule)
| Signal | Meaning | Examples |
|---|---|---|
| selection fill `--tm-select-bg` | this item is the chosen one among siblings | nav item, segmented option, button group choice, category chip, selected row, pass cell in view, the bulk-action bar |
| ink fill `--tm-ink` (+ `--tm-inset` text) | this control itself is on | checked checkbox, radio dot, switch, count badge, `.tm-btn--toggle`, `.tm-meta-toggle` |
Test for anything new: *choice among siblings* → selection fill; *own on/off state* → ink. Why ink for controls: they sit inside selected areas (checkbox in a selected row, toggle in the green bulk bar). Exception: chart legend items fade hidden series instead of filling.

## 4. Toggles and stability
- A toggle keeps its label and icon; only the fill changes (`aria-pressed`). Swapping "Show selected only" ↔ "Show all rows" made neighbouring buttons jump and was rejected.
- Reserve width for changing numbers (counter badge sized for two digits), append optional buttons at the end of a row, never insert them in the middle.
- Do not add a whole toolbar row for one small display option. Put it on an existing element — e.g. the unit in a panel head becomes a `.tm-meta-toggle` (empty 2px ink frame; on = ink fill; 22px so the head keeps its height).

- View toggle vs action vs setting: a *view toggle* (filter/display option) keeps a fixed label and uses the ink fill; an *action* on a record ("Disable", "Resume") may name the action; a persistent on/off *setting* of a record is a labelled `.tm-switch`. Do not rename product actions to satisfy the toggle rule — that changes the product.

- Busy buttons keep their size: `aria-busy="true"` hides the label in place and centres a spinner after 250 ms; never change the label or put a spinner beside it.
- Record problems: a text-sized marker beside the record name lives exactly as long as its toast; dismissing acknowledges this occurrence, a new failed attempt shows both again.

## 5. Focus (§3)
- `--tm-focus` is the ink color in every theme. Controls (buttons, fields, selects, segmented options, chips, pagination, checkbox/radio/switch, meta toggles) keep their frame and get `box-shadow: var(--tm-focus-lift)` (3px 3px 0, no blur) — "lifted off the page".
- Composite controls (stepper, input group with affixes, date/time field, multi-select, drop zone) lift as one piece; the focused part inside is marked with the press tint `--tm-press`, never with a second frame.
- Pickable items: ink frame on the block (item grammar). Tabs, accordion summaries, charts and links: 2px ink outline.
- Never a blue/teal ring, never two different focus looks on one screen. Do not move focus programmatically into a text field after a mouse action (text fields always show focus).
- Popups that move focus when they open (select list, menu, picker) must look the same opened by mouse the first time and every later time. Browsers paint scripted focus as keyboard focus before the first click on a page, so keep the input-modality hook: `data-tm-input="pointer"|"key"` on `<html>` (reference `trackInputModality()` in `lib.js`, or the same few lines in the app). With it, a mouse-opened list shows the selection fill but no focus frame, and the first key press brings the frame back.

## 6. Status, alerts, feedback (§2, §5)
- Standalone status → `.tm-status` plate (`--sm` in panel heads and overlays) with a `.tm-mark` (`data-state`: complete, active, info, review, warning, error, pending, skipped, none). Inline status in rows/cells/text → `.tm-badge` with its marker shape. Never color alone.
- Alerts: 2px ink frame, tone tint, icon. Warnings may be hidden; a status-plate button in the page head restores them. Blocking errors cannot be hidden.
- Toasts: transient, same frame; errors persist until dismissed.
- Empty / error / stale states: `.tm-state` with title, explanation and one recovery action.

- Dialogs, never browser popups: `window.confirm/alert/prompt` are replaced by `<dialog class="tm-dialog">`. Destructive confirmation: title states the action and object, body says what is lost and what is kept, Cancel (outline, focused by default) + danger confirm; Escape and backdrop cancel; focus returns to the trigger. Reference: `confirmDialog` in `src/playground/lib.js`.
- Emphasis follows frequency: rare page-level actions (add, settings) are ghost icon buttons with a `.tm-tip` label; a filled primary only for the page's main everyday action, if any.
- Action weight: one strong action per record or panel; other actions ghost; destructive last and quiet (ghost) in rows, red in the confirmation dialog; `tm-btn--danger-outline` only for a page-level action in a danger zone. Equal frames on every action flatten the hierarchy.
- Emphasis budget (DESIGN.md §1): decide the two or three things the screen must say; normal state is quiet (muted text, `.tm-badge--plain`), deviations are loud; frames for structure and controls only, never around data; color for identity, data and attention only. Restyling is a redesign of emphasis, not a translation of the old DOM.
- Panel grids vs separate panels: a framed grid with shared dividers only for parts of one topic (one island per topic, space between islands); independent records with own actions are separate panels with a gap.
- Tooltips: always `.tm-tip` or `data-tm-tip` (+ `enhanceTips`; `-overflow` for truncated text, `-collapsed` for the icon rail) (hover + focus, Escape); never the browser `title` popup for meaningful information — two tooltip looks for the same kind of data read as two systems. Mark tooltips open from the mark toward the content.
- Text-sized marks are SVG: solid disc in the state color (grey for info) with a surface-colored glyph drawn as shapes; no font glyph in a stroked ring (blurs at 1×); optical overshoot like round letters: ~0.82em, ~0.06em below the baseline, size rounded to whole pixels (`round(0.82em, 1px)`).
- Markers are never taller than the text they annotate; a per-item problem goes on that item's line, smaller than its label. Record-level transient problems (failed check, timeout, stale) are condition toasts naming the record: shown while the condition holds, auto-removed when it clears, ✕ hides until a new problem; no history pile-up. The record shows no problem message or marker; hovering/focusing the toast gives the record's frame the toast's tone at double weight (border color + same-width outer outline, nothing moves) — a same-weight recolor is invisible in Dark and Coffee. A failed action is a danger toast too (stays until closed, the same message is not stacked twice), and a lost connection is a condition toast that leaves on reconnect — not an alert inserted above the content.
- Details on demand: panel bodies hold only what is read every visit; secondary facts go behind a text-sized info mark (tooltip). Color a value only when it signals (newer version), otherwise grey. Identity colors come from the subject (app/brand colors), not chart slots; same hue in every state (exhausted = darker).
- Stable facts (provider, name, plan) in the card head, lighter text next to the name; the body holds values that change between visits.
- Sibling cards share levels: common rows start at the same height across a row of cards; controls only some cards have sit on an existing line in their small size, and the line reserves that height in every card.
- Say each thing once: no text for what the visual state already shows; normal states are not stated, deviations are.
- Busy starts after the decision: no spinner on a trigger while its confirmation dialog is open; busy only after confirm, work in the background, result as a toast.
- Action feedback: a success toast after each user action (names object + result, leaves after ~4 s, pauses on hover); failures persist until closed; actions that open a dialog need no toast.
- Inactive records (disabled, paused, expired, stale): the whole record goes quiet, warnings included — neutral bars, muted figures, the dimmed treatment is the message — no extra "disabled" note that changes the card height (marker badges are for mixed-state table columns); actions that need an active record are hidden/disabled, the rest stay normal; never opacity on the whole card.

## 7. Fields (§5)
- Label above, hint below, error message with a "!" glyph; invalid = `aria-invalid="true"` (red frame). Read-only = dashed frame.
- Native number spinners are hidden; use `.tm-stepper`. Native date/time popups are replaced by `.tm-picker` (Monday-first, 24h, no weekend colors).
- Text/search/email/url/tel fields get a clear ✕ (`.tm-input-clear`) while they hold text: space reserved, "Clear" tooltip, click clears and keeps focus, Escape clears search. Not on number/date/code/read-only/disabled fields, nor where a cancel ✕ already exists.
- Selects: styled option list (base-select or scripted fallback), no tick, option text aligned with the closed field.
- One caret for "opens a list below": a menu button ("More", "Actions") ends with `.tm-caret`, the select's own solid triangle in the same color — never a stroked chevron icon. Why: side by side, two different down-arrows read as two systems. Chevrons stay for back/next and in-place expansion.

## 8. Tables and selection (§5)
- Compact = one line per record (details in tooltip); Comfortable = full detail. The toggle must make a visible difference.
- Numbers right-aligned, tabular, mono; negatives in danger color with a real minus.
- Selection across pages: the bulk bar counts the whole selection; the header checkbox acts on the visible page; "Select all N matching" appears (last) when a full page is selected; a fixed-label "Selected only" toggle shows the whole selection and appears as a removable filter chip. The bar does not repeat what the table already shows ("2 on this page" was rejected).
- Under 640px use `.tm-table--stack` (rows become labelled records).

## 9. Layout patterns (§5 Layout patterns)
- Overview: page head (back link, eyebrow, title, meta, status plate) → stage pipeline → main table → collapsible secondary sections → page-level/destructive actions last.
- List + preview: two columns split by a 2px divider; the list's search sits in the list head; preview pages with "Load next N", no tabs.
- Panel grids never leave an empty cell: balance the column count to the item count or let the last panel span the gap (`balanceGrid`).
- A filter sits next to what it filters (section head, list head), not in a distant toolbar.
- Report state, not invented progress — no progress bars for work that does not report partial progress.
- Prefer one view plus collapsible sections over tabs.

## 10. Charts (§6)
- Apache ECharts (SVG renderer) themed via `adapters/echarts.js` (`readChartTokens`, `createEChartsTheme`); re-create charts on theme change.
- Text: IBM Plex Mono, medium 500, 11px; names and values in full text color, axis numbers in `--tm-text-2`.
- Named categories always show every label (switch bars to horizontal instead of rotating/hiding); time axes may thin. Value labels are never hidden.
- Thresholds dashed, targets dotted, labelled with the short value at the line start. Series differ by dash/marker as well as color.
- Every value reachable by tooltip **and** keyboard; tooltips sit below sticky chrome and close on scroll.
- Heatmap labels: text color by default; viewer can switch to per-cell color via the unit `.tm-meta-toggle`.

## 11. Themes and tokens (§3, §4)
- Light / Paper-Stone, Dark / Graphite, Coffee / Espresso-Mocha via `data-tm-theme` on `<html>`. Themes change color only — sizes, spacing and layout must be identical (verify by comparing page dimensions across themes).
- Use `--tm-*` tokens for every color, space, radius, border and font. Private `--_*` variables are not API.
- Display headings stay IBM Plex Mono bold; body/UI text IBM Plex Sans 13px; numbers, labels and metadata mono.
- New colors must pass contrast: 4.5:1 text, 3:1 graphics/boundaries, in all three themes.

## 12. Anti-patterns (§8) — do not
- Rounded (>3px) cards, pill buttons/badges; blurred or frosted layers; drop shadows on panels.
- A 2px frame repeated on every nested card; nested headers bolder than their parent.
- Colored side bars; blue focus rings; ticks in select options; selection frames.
- Color-only status, selection or series encoding.
- Relabelling toggles; inserting buttons mid-row; layout that shifts on state change.
- Truncating data the user must read; rotated axis labels; random or live data in fixtures/screenshots.
- Importing Tailwind/daisyUI into a consumer just to get these styles.
