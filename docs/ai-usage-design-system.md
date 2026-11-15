# AI Usage on the shared design system

The AI Usage dashboard (`crates/hs-usage`) takes part of its UI from the design system shared with Intelitex and
Gitiary. The design system is plain CSS: tokens, dark and light themes and opt-in `ds-` primitives. It has no
JavaScript and no build step. This is a pilot: the header controls, the three model panels, their data table, the
tool-call badge, the live status dot and the error notice use it. The rest of the dashboard is unchanged.

## Source and version pin

- **Location:** `crates/hs-usage/assets/design-system/`, a `git subtree` with squashed updates.
- **Version:** **v0.2.0**, tag `design-system-v0.2.0` on `https://github.com/hipotures/intelitex.git`.
- **Pin:**
  - split commit `40b876a321281e7397deb25e0262f6e90f2fb1b9`, recorded as `git-subtree-split` in the subtree merge;
  - tree `9251b8997220990b27cb47b31cb4991f914e585c`, equal to the tag's tree.
- **Never edit the subtree here.** Change the package in Intelitex and release a new tag there. Tags are immutable,
  so a fix is a new version. Then pull it:

  ```sh
  git subtree pull --prefix=crates/hs-usage/assets/design-system https://github.com/hipotures/intelitex.git design-system-vX.Y.Z --squash
  git rev-parse HEAD:crates/hs-usage/assets/design-system   # must equal: git rev-parse 'design-system-vX.Y.Z^{tree}'
  ```

  Read `crates/hs-usage/assets/design-system/CHANGELOG.md` for breaking changes, then repeat the checks below. If a
  file is added to the package's `ds.css`, add it to the embedded route as well (see below).

## Embedding and serving

The binary stays self-contained, with no runtime downloads and no frontend build step:

- `src/main.rs` embeds `ds.css`, `tokens.css`, `themes.css`, `components.css` and `state.css` with
  `include_bytes!`.
- It serves them at `/assets/design-system/{name}`, with the same ETag and `must-revalidate` caching as the other
  local assets. Unknown names return 404.
- The page links `ds.css`, the package's public entry point. The browser resolves its relative `@import`s to the
  four other files on the same route.
- The container build copies the repository root, so the subtree is compiled in like any other asset.

Stylesheet order in `src/index.html`:

1. `design-system/ds.css`: package tokens, themes, primitives and focus ring.
2. `aiusage-tokens.css`: AI Usage tokens and base styles. It is loaded after the package, so the shared names
   `--bg`, `--text` and `color-scheme` keep their AI Usage values on every element.
3. `aiusage-ds.css`: the AI Usage adapter for the package (below).
4. The page's own `<style>`.

## The adapter: `assets/aiusage-ds.css`

**Palette bridge.** The package colors (`--panel`, `--panel-2`, `--panel-3`, `--line`, `--text-soft`, `--text-dim`,
`--accent`, `--green`, `--red`, `--amber` and their `-soft` variants) map to AI Usage tokens. The primitives
therefore:

- draw in AI Usage's palette;
- follow both of its themes, including the OS preference that applies before `dashboard.js` sets `data-theme`.

| Package color | AI Usage token |
| --- | --- |
| `--panel`, `--panel-2`, `--panel-3` | `--surface`, `--surface-2`, `--surface-3` |
| `--line`, `--text-soft`, `--text-dim` | `--border-strong`, `--text-2`, `--muted` |
| `--accent` (focus ring, primary button, running dot) | `--codex`, the accent AI Usage already used for focus and the live dot |
| `--amber`, `--red`, `--green` | `--warn`, `--up`, `--down` |

The package's accent hooks (`--ds-accent-dark`, `--ds-accent-light`) are not used. They switch only with
`data-theme="light"`, while AI Usage also follows `prefers-color-scheme`.

**App-specific layout.** The adapter also holds the layout rules the package does not provide:

- the range control scrolls sideways on phones, with a visible scrollbar;
- the status dot sits in a flex row without its own margin;
- the error notice is a single line after a "⚠" sign;
- the model panels have a heading block and wrapping tools;
- the data table has right-aligned numbers, a sticky header, wrapping column titles and columns sized to their
  content.

## Components on the design system

| Element | Primitive |
| --- | --- |
| Time-range group (1h … 90d, More ▾) | `.ds-segmented`, `role="group"`, `aria-pressed` from Datastar `data-attr` |
| Agent, server and project filters | `.ds-select` (native select; Datastar `data-bind` unchanged) |
| Theme button | `.ds-btn` |
| Custom range: From and To | `.ds-input` (`type="date"`) |
| Custom range: Apply | `.ds-btn .ds-btn-primary .ds-btn-compact`; disabled until the dates form a range |
| Model tokens over time, Cache hit by model, Tokens by effort | `.ds-panel` with `.ds-panel-head`, `.ds-panel-title`, `.ds-panel-body` |
| Model picker | `.ds-select` |
| Table / chart toggle | `.ds-btn .ds-btn-ghost .ds-btn-compact` |
| Copy data as JSON | `.ds-icon-btn` |
| Model panel data table | `.ds-table` (set by `renderTable` for tables inside a `.ds-panel`); its scrolling wrapper is a focusable, labelled `role="region"`, so arrow keys scroll it |
| Codex "failed" tool calls | `.ds-badge` with `data-tone="warning"` |
| Live status dot | `.ds-status-dot`: `data-status="running"`, or `"waiting"` when the live connection is stale |
| Error banner | `.ds-notice` with `data-tone="danger"`, `role="status"` |

Datastar attributes, signal names, element IDs, the SSE request, ECharts code and every backend interface are
unchanged. `dashboard.js` changes only presentation:

- the status dot's `data-status`;
- the badge markup;
- the `ds-table` class.

Not on the design system yet:

- the KPI cards and the other cards (`.card`): Usage over time, the share and scope charts, composition, models,
  activity and agents;
- the card metric switches (`.seg`) and the chips;
- the range menu's surface and menu items;
- the footer links;
- the raw chart cards and their tables.

## Collisions and workarounds

| Collision | Workaround |
| --- | --- |
| **Unprefixed token names.** The package and AI Usage both define `--bg` and `--text`. | AI Usage's file loads later and wins. The bridge maps the rest, so nothing outside the pilot changes color. |
| **Focus ring.** AI Usage gave every `button`/`select` a 2px outline at offset 2px. That element rule outweighs the package ring, whose selector is wrapped in `:where()`. | The AI Usage rule now excludes design-system controls with `:not(...)`. They get the package ring (2px accent, offset 3px); every other control keeps its ring. |
| **Global table rules.** `th`/`td` rules come later in the page and override the package's zero-specificity `.ds-table` rules. | They are scoped to `table:not(.ds-table)` with unchanged specificity, so the raw chart tables look the same. |
| **`.copy` and the `.menu button` rules.** These would restyle `.ds-icon-btn` and the Apply button. | Narrowed to `.card .copy` and `.menu [role=menuitem]`. |
| **Dead rules.** | `.badge`, `.error`, `.live-dot`, `.menu input[type=date]` and `.menu .custom .apply` are removed. |
| **`.scope`.** It sized the header controls. | It keeps only the 200px maximum width and the phone layout. |

## Intentional visual differences

Images, with the same file names in `docs/ai-usage-design-system/before/` and `after/`:

- `header-*`
- `range-menu-*`
- `model-panels-*`
- `model-table-*`
- `tools-badge-*`

Each comes in desktop (1440px) and mobile (390px), dark and light. The numbers differ between the two sets because
the screenshot data follows the clock.

- **Header:**
  - The range group is the package's segmented control: 1px border, 12px labels, a bold pressed segment.
  - The filters use the field look: filled `--surface-2`, 8px radius, 14px text.
  - The theme button is a bold `.ds-btn`.
- **Custom range:**
  - The date fields use the field look.
  - Apply is a primary (Codex green) button and dims to 45% when disabled. Before, it was a flat grey button.
  - Apply no longer turns grey while its range is the active one. The More ▾ segment, which is pressed and shows
    the range's label, already marks it.
- **Model panels:**
  - A 1px border, 12px radius, and a header strip with a divider.
  - A 14px bold title, tighter padding.
  - Copy is a 36px bordered icon button; the table toggle is a ghost button instead of an underlined link.
- **Model table:**
  - Uppercase column titles on the `--surface-2` strip.
  - Taller rows with `--border-strong` dividers.
  - Columns sized to their content; titles may wrap.
- **Badge:** a bordered pill with bold text instead of a filled square chip.
- **Error notice:** a tinted danger notice with a border, instead of an outline only.
- **Focus ring on the migrated controls:** offset 3px instead of 2px.
- **Live dot:** amber when the connection is stale, as before, now through `data-status`.

Outside the pilot, nothing changes except vertical position, because the model panels are about 4px shorter.

## Verification

This change adds no permanent tests (see `AGENTS.md`). It was verified with temporary diagnostics against the
compiled binary:

- **Setup:**
  - `homestack-usage` built with `scripts/cargo.py`;
  - a local stand-in for Prometheus serving synthetic Claude Code, Codex and Antigravity series;
  - Playwright Chromium with reduced motion and fixed fonts.
- **Computed styles of every element:** the dashboard, open range menu, model table, Codex tab and raw charts, at
  desktop and mobile, dark and light (20 views). The old and the new binary were captured back to back.
  - Outside the migrated elements, the only difference was the width of one data value.
  - Two captures of the old binary showed the same kind of difference.
- **96 functional checks, all passing:**
  - **Themes:** the panel, field and accent colors resolve to the AI Usage tokens in both themes; the theme button
    switches them; with JavaScript disabled they follow the OS light and dark preference.
  - **Keyboard:** Tab reaches the range buttons, More, the three filters and the theme button in order, each with
    the 2px Codex-green ring at offset 3px. An unmigrated button keeps its 2px ring at offset 2px.
  - **Datastar:**
    - The range group updates `aria-pressed` and re-requests the SSE stream with the new range.
    - Choosing an agent by keyboard reaches `$agent`, the SSE payload and `<html data-agent>`.
  - **Custom range:**
    - More opens the menu (`aria-expanded`).
    - Apply is disabled (45% opacity) for an inverted range; with a valid range it sets the range and closes the
      menu.
    - Esc closes the menu.
  - **Model panel:**
    - The toggle shows a `.ds-table` with a sticky header inside the panel.
    - Copy puts JSON on the clipboard.
    - The table wrapper takes keyboard focus and scrolls with the arrow keys.
    - The raw chart tables keep their own style.
  - **Status:**
    - the Codex failed-calls badge has the warning tone and color;
    - the live dot reports `running`;
    - an error packet shows the one-line danger notice with `role="status"`.
  - **Layout and errors:**
    - no horizontal page overflow at 390px or 1440px;
    - no migrated element outside the viewport;
    - no page or console errors;
    - under reduced motion, the primary button does not lift on hover.

## Gaps in the package (input for a future version)

Recorded here; v0.2.0 is not changed:

- `.ds-btn` and `.ds-icon-btn` use `transition` on every property, so the focus outline animates for 160ms from the
  browser default to the ring. A transition limited to colors, border, filter and transform would avoid it.
- **Colors without a prefix.** `--bg` and `--text` collide with application tokens of the same name. AI Usage gets
  away with it only because of the load order.
- **Theme.** Light applies only with `data-theme="light"`. There is no `prefers-color-scheme` fallback.
- **Table.** `.ds-table` has no numeric-column alignment and no content-sized (auto) layout.
- **Notice.** `.ds-notice` has no single-line variant with an icon.
- **Control heights.** The default heights differ: select about 38px, button about 40px, segmented control about
  37px.
- **Pressed icon button.** A pressed `.ds-icon-btn` state (Gitiary's fork marker) is still app-specific.
