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
- the header filters, theme button and range group keep the AI Usage control height of 40px (see
  [Layout stability](#layout-stability));
- the model panel body has 16px block padding and the panel heading grows from a 320px basis;
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

Each comes in desktop (1440px) and mobile (390px), dark and light. Both sets were captured from the same
deterministic fixture (see [Reproducing the validation](#reproducing-the-validation)): "before" is the `main` page,
"after" this branch, so equal numbers render equally and only the migrated elements differ.

- **Header:**
  - The range group is the package's segmented control: 1px border, 12px labels, a bold pressed segment.
  - The filters use the field look: filled `--surface-2`, 8px radius, 14px text.
  - The theme button is a bold `.ds-btn`.
  - All header controls stay 40px tall, as before.
- **Custom range:**
  - The date fields use the field look, so the menu is about 13px taller.
  - Apply is a primary (Codex green) button and dims to 45% when disabled. Before, it was a flat grey button.
- **Model panels:**
  - A 1px border, 12px radius, and a header strip with a divider.
  - A 14px bold title.
  - Copy is a 36px bordered icon button; the table toggle is a ghost button instead of an underlined link.
- **Model table:**
  - Uppercase column titles on the `--surface-2` strip.
  - Taller rows with `--border-strong` dividers.
  - Columns sized to their content; titles may wrap.
- **Badge:** a bordered pill with bold text instead of a filled square chip.
- **Error notice:** a tinted danger notice with a border, instead of an outline only.
- **Focus ring on the migrated controls:** offset 3px instead of 2px.
- **Live dot:** amber when the connection is stale, as before, now through `data-status`.

### Active time range

The active range is shown as before:

- a preset (1h … 90d) is the pressed segment;
- a menu range (180 days, 1 year, All) highlights its menu item and presses More ▾, which shows the range's label;
- a custom range presses More ▾, which shows the dates.

Before the migration, Apply carried an `active` class while its dates were the active range. That class drew
nothing: `.menu button.active` and `.menu .custom .apply` both set the same `--surface-2` background, so Apply looked
the same either way. Dropping it removed no visible or accessible indication, and it is not restored.

### Layout stability

The F12 comparison found a vertical shift of about 4px. Its causes and fixes:

| Shift | Cause | Fix in `aiusage-ds.css` |
| --- | --- | --- |
| Phone header 4px shorter (185px → 181px), moving the whole page up | `.scope { min-height: 40px }` was replaced by `.ds-select` (38px), and `.seg` (40px) by `.ds-segmented` (37px). The controls wrap into three rows on phones. | `.controls > :is(.ds-select, .ds-btn), .range > .ds-segmented { min-height: 40px }`; the select and button use `padding-block: 0`, because the button's own padding made it 40.3px. |
| Desktop model panels 4px shorter (379px → 375px), moving everything below them | Panel chrome: 1px borders, 13px head padding and 14px body padding total 57px, against the card's 22px padding and 16px gap (60px); the 14px title is 1px lower than the 15px heading. | `.ds-panel-body { padding-block: 16px }` brings the panels back to 379px. |
| Panel tools wrapped below a long heading | Flex wrapping places the heading at its full text width. The ghost toggle is 13px wider than the old link, so the tools moved to a second line. | `.panel-heading { flex: 1 1 320px }`: the tools stay beside the heading, and only the subtitle wraps. |

Measured with the deterministic fixture, everything from the header to the model panels has the same position and
height to the fraction of a pixel at 1440px and 390px. Remaining differences, all from intended primitives:

- **Larger controls:** at 390px the panel headings wrap their tools into a second row, as before, but the 33px ghost
  toggle and the 38px model picker make the panels 3px shorter to 13px taller than before.
- **Font-dependent subtitle wrap:** at 1440px with the DejaVu fonts of the reproduction setup, the "Cache hit by
  model" subtitle lacks 1px beside the wider toggle. It wraps to two lines, so the model row grows from 379px to
  396px. With the fonts of the F12 environment it fit on one line.
- **Badge width:** the bold, bordered badge is about 10px wider. On a 390px screen with the fixture numbers, it moves
  below the Tool calls heading, so that card grows by 44px. Where it wraps depends on the numbers.

The "width of one data value" in the F12 comparison came from synthetic data that followed the clock. The fixture
below uses constant rates on absolute 300-second boundaries, so every number is the same on each load. The
production styles were not changed for it.

## Verification

This change adds no permanent tests (see `AGENTS.md`). It was verified with temporary diagnostics against the
compiled binary.

### F12 (original environment, not rerun here)

These results come from the F12 session. Its scripts were kept in the git-ignored `./tmp` and are not in the
repository.

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

### Stabilization rerun (2026-11-15, development VM)

Run against the stabilized branch, with the fixture and checks described below:

- **Rust, `hs-usage` only:**
  - `python3 scripts/cargo.py build --locked -p hs-usage --bin homestack-usage`: passes.
  - `python3 scripts/cargo.py clippy --locked -p hs-usage --all-targets -- -D warnings`: passes.
  - `python3 scripts/cargo.py test --locked -p hs-usage`: passes; the crate has no unit tests.
  - `cargo fmt -p hs-usage --check`: passes.
- **Subtree pin:** `git rev-parse HEAD:crates/hs-usage/assets/design-system` equals the tree of the published
  `design-system-v0.2.0` tag, `9251b8997220990b27cb47b31cb4991f914e585c`; its commit is the split commit
  `40b876a321281e7397deb25e0262f6e90f2fb1b9`.
- **Layout probe:** header and sections above the model panels match `main` exactly at 1440px and 390px (see
  [Layout stability](#layout-stability)).
- **59 smoke checks, all passing.** They are a subset of the F12 checks, plus HTTP checks for the assets:
  - **Assets:** each of the five design-system files, `aiusage-ds.css` and `aiusage-tokens.css` returns 200,
    `text/css`, the exact bytes of the file in the repository, an ETag with `must-revalidate`, and 304 on
    `If-None-Match`. An unknown design-system name returns 404. The page links `ds.css`, `aiusage-tokens.css` and
    `aiusage-ds.css` in that order.
  - **Determinism:** two independent loads render the same KPI and badge values.
  - **At 1440px and 390px, in dark and light:** no horizontal overflow; no migrated element outside the viewport;
    the theme follows the OS; the panels have the card background, and the theme button switches both; no page or
    console errors.
  - **No JavaScript:** the field color follows the OS dark and light preference.
  - **Keyboard:** the Tab order and the focus rings, as in F12.
  - **Datastar:** `aria-pressed` follows `$range`; the range change and a keyboard agent change each send a new SSE
    request with the new value; `<html data-agent>` follows.
  - **Range menu:**
    - More opens the menu.
    - 1 year highlights its item and presses More with its label.
    - Apply is at 45% opacity and disabled for an inverted range.
    - A valid custom range closes the menu and leaves More as the only pressed segment, showing the dates.
    - Esc closes the menu.
  - **Model panel:** `.ds-table` with a sticky header; the focusable, labelled region scrolls with PageDown; raw
    chart tables keep their style; Copy puts JSON on the clipboard.
  - **Status and ARIA:**
    - the failed-calls badge has the warning tone in the `--warn` color, and the live dot reports `running`;
    - the range group is `role="group"` with a label, and the error notice is `role="status"`;
    - the live text is `aria-live="polite"`, the dot is `aria-hidden`, and every panel is labelled.
- **Not rerun here:** the full computed-style diff of 20 views, the error-packet notice and the hover-lift check.

### Reproducing the validation

The checks are temporary diagnostics, so their scripts stay outside the repository, in a task directory under the
git-ignored `./tmp` (here `tmp/f12-stabilize/`). This section is enough to recreate them.

**Requirements:**

- Prometheus 3.x and `promtool`; the release tarball is enough.
- Python with Playwright, installed with `uv pip install playwright`.
- Chromium for Playwright.
- The DejaVu fonts, through a `FONTCONFIG_FILE`, so text metrics match between runs.

Keep the Python environment, the browser and every temporary file on disk under `./tmp`, never in `/tmp`.

**Fixture**, generated as OpenMetrics and imported with `promtool tsdb create-blocks-from openmetrics`:

- **Timing:** samples every 300s on absolute multiples of 300, from 35 days ago to 10 minutes ahead.
- **Labels:** every series has `host_name="fixture-host",project="fixture-project"`.
- **Claude Code:** `claude_code_token_usage_tokens_total`, a constant-rate counter for each `model` and `type`:
  - models `claude-opus-5-5`, `claude-sonnet-5-5` and `claude-haiku-5-5`, scaled 3, 2 and 1;
  - types `input` 400, `cacheRead` 3000, `cacheCreation` 200 and `output` 250 per step, times the scale.
- **Claude Code cost:** `claude_code_cost_usage_USD_total` per model, 0.01 times the scale per step.
- **Codex tokens:** `codex_response_token_usage` events for model `gpt-6.1-sol`, with `effort` `medium` and
  `high`. Values by `token_type`: `input` 1000, `cached_input` 800, `cache_write_input` 50, `output` 200,
  `reasoning_output` 120, `total` 1200.
- **Codex tools:** `codex_tool_call_total` counters by `tool` and `success`: `shell` (6 true and 1 false per step),
  `apply_patch` 3 and `read_file` 4.
- **Antigravity:** `agy_token_usage` events for model `gemini-fixture`: `input` 300, `output` 60.

**Run**, from the repository root, one step at a time:

1. Build the binary:

   ```sh
   python3 scripts/cargo.py build --locked -p hs-usage --bin homestack-usage
   ```

2. Generate the fixture and import it into a fresh TSDB.
3. Start Prometheus with `--storage.tsdb.retention.time=400d --web.listen-address=127.0.0.1:19190` and an empty
   `scrape_configs`.
4. Start the dashboard:

   ```sh
   HOMESTACK_PROMETHEUS_URL=http://127.0.0.1:19190 HOMESTACK_LISTEN=127.0.0.1:19191 \
     target/debug/homestack-usage
   ```

   It is ready when `curl -fs http://127.0.0.1:19191/health/ready` succeeds.
5. Run the Playwright scripts with `FONTCONFIG_FILE` and `TMPDIR` pointing into the task directory:
   - **Smoke checks:** load with `localStorage` cleared and `range=24h`. Wait for
     `usage.debug().snapshot && !document.getElementById('model-tokens').hidden`, then for `document.fonts.ready`.
     Check the list above. Wait for events or states, such as `page.expect_request` for SSE requests or
     `wait_for_function` for attributes and computed styles; do not use fixed sleeps. The package's `.ds-btn`
     transitions every property, so wait until the focus ring and the 45% opacity reach their final computed values.
   - **Layout probe and screenshots:** for "before", route `/`, `/assets/dashboard.js` and
     `/assets/aiusage-tokens.css` to `git show main:<path>`; the API and data are the same binary. Compare
     `getBoundingClientRect()` of the header, its controls and every section. Set the live clock text to a fixed
     value. Repeat each screenshot until two consecutive frames are identical, so the charts have finished drawing.
6. Stop only the Prometheus and `homestack-usage` processes started for the run.

**Expected results:**

- every smoke check passes;
- the layout probe shows no difference above the model panels;
- the screenshots match the images in `docs/ai-usage-design-system/`, apart from chart axis labels, which follow
  the time of day.

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
