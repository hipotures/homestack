# Intelitex design system (`web/design-system/`)

A small design system written in plain CSS. It has no framework, no JavaScript and no build step, and it styles
only what opts in through `ds-` classes. This folder is self-contained, so it can move to its own repository
unchanged (see [Extraction](#extraction)).

## Quick start

```html
<html data-theme="light">          <!-- omit for the dark default -->
<link rel="stylesheet" href="design-system/ds.css">
<link rel="stylesheet" href="design-system/base.css">   <!-- optional page defaults -->

<button type="button" class="ds-btn ds-btn-primary">Save</button>
```

To see it, open `examples/standalone.html` (a consumer page) or `gallery.html` (every primitive) straight from disk.

## Files

| File | Required | Contents |
| --- | --- | --- |
| `ds.css` | entry point | Imports `tokens.css`, `themes.css`, `components.css`, `state.css`, in that order. |
| `tokens.css` | yes | Scale tokens (`--ds-*`). |
| `themes.css` | yes | Dark (default) and light color themes. |
| `components.css` | yes | The primitives. |
| `state.css` | yes | The keyboard focus ring. |
| `base.css` | optional | Page defaults for a page the design system owns: `html` colors and font, `body` margin, border-box sizing. Every rule is `:where()`, so specificity is zero. |
| `gallery.html`, `examples/` | no | Documentation pages. |

An application that has its own CSS can import the parts one by one and put its rules in between. The only
ordering rule is that `state.css` must come after any component CSS that sets `outline`.

## Supported browsers

The package targets **Baseline 2023**: Chrome and Edge 111+, Firefox 121+, Safari 16.2+. It relies on these
features:

- custom properties;
- `:where()` and `:is()`;
- `color-mix(in srgb, …)` (Chrome 111, Firefox 113, Safari 16.2);
- `:focus-visible`;
- `:has()` (Firefox 121), used only to drop panel padding around a table. Without it the padding simply stays.

## What the package touches globally

`ds.css` sets custom properties and `color-scheme` on the root element and nothing else. Tests check that
unrelated HTML (headings, links, form controls, tables, lists) computes exactly as it does without the package.

The primitives also include zero-specificity resets (box sizing, fonts, margins, icon-button padding). With them,
the primitives look the same with or without a CSS reset such as Tailwind preflight, and any application rule
still wins.

## Theme contract

- **Dark** is the default and lives on `:root`.
- **Light** applies with `<html data-theme="light">`.
- Each theme sets `color-scheme` (`dark` or `light`), so native controls and scrollbars follow it.
- Additional themes belong to the application. Define the same color tokens under your own selector, for example
  `html[data-theme="sepia"] { --bg: …; … }`. Intelitex keeps its sepia theme this way in
  `src/styles/intelitex/themes.css`.

Both themes define these **color tokens**:

| Group | Tokens |
| --- | --- |
| Surfaces | `--bg`, `--bg-soft`, `--bg-inset`, `--panel`, `--panel-2`, `--panel-3` |
| Lines | `--line`, `--line-strong` |
| Text | `--text`, `--text-soft`, `--text-dim` |
| Accent | `--accent`, `--accent-soft` |
| Status | `--success`, `--danger`, `--warning` and their `-soft` variants |
| Status colors | `--green`, `--red`, `--amber` and their `-soft` variants (the color names behind the aliases) |
| Effects | `--shadow`, `--scrollbar-thumb`, `--scrollbar-thumb-hover` |

## Token API (`tokens.css`)

The color tokens above are shared with the themes. The scale tokens are:

| Group | Tokens |
| --- | --- |
| Font | `--ds-font-sans`, `--ds-font-serif`, `--ds-font-mono` |
| Size | `--ds-text-2xs` (10px), `-xs` 11, `-sm` 12, `-md` 13, `-base` 14, `-lg` 16, `-xl` 18, `-2xl` 20, `-3xl` 24, `-4xl` 27 |
| Weight | `--ds-weight-regular` 400, `-medium` 600, `-semibold` 650, `-bold` 700, `-heavy` 800 |
| Space | `--ds-space-1` … `--ds-space-11`: 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28px |
| Radius | `--ds-radius-xs` 6, `-control` 8, `-sm` 9, `-md` 10, `-lg` 12, `--ds-radius` 14, `-xl` 16, `-pill` 999px |
| Focus | `--ds-focus-ring-width` 2px, `--ds-focus-ring-offset` 3px |
| Motion | `--ds-duration-fast` .12s, `-base` .16s, `-slow` .2s, `--ds-ease` |
| Layering | `--ds-z-header` 30, `-drawer` 70, `-debug` 100, `-toast` 120 |
| Layout | `--ds-header-h` 64px, `--ds-content-max` 1480px, `--ds-content-max-wide` 1520px, `--ds-shadow-drawer` |

The new tokens use the `--ds-` prefix because Tailwind v4 owns the `--text-*`, `--font-*`, `--radius-*` and
`--shadow-*` namespaces.

## Component class API (`components.css`)

| Class | Element | Notes |
| --- | --- | --- |
| `.ds-btn` | `<button>`, `<a>` | Secondary by default. |
| `.ds-btn-primary`, `.ds-btn-ghost`, `.ds-btn-danger` | with `.ds-btn` | Variants. |
| `.ds-btn-compact` | with `.ds-btn` | Smaller padding and text. |
| (icon) | `<svg aria-hidden="true">` child of `.ds-btn` or a `.ds-segmented` button | The control becomes `inline-flex` with a 6px gap. Badges already lay out icons. |
| `.ds-icon-btn` | `<button aria-label>` | 36×36 square. |
| `.ds-panel` | `<section>` | Contains `.ds-panel-head` (row: title plus actions), `.ds-panel-title` (heading) and `.ds-panel-body`. A body that holds a table has no padding. Clips overflow by default. |
| `.ds-panel-overflow-visible`, `.ds-panel-overflow-auto` | with `.ds-panel` | Show or scroll content that sticks out instead of clipping it. |
| `.ds-table-card` | wrapper | Raised card for a table. |
| `.ds-table` | `<table>` | Header and cell styles; selectors weigh like element rules. |
| `.ds-table-interactive` | with `.ds-table` | Rows show a pointer cursor and a hover background. |
| `.ds-progress` | `<progress>` | Thin accent bar. |
| `.ds-badge` | `<span>` | Neutral by default; add `data-tone`. |
| `.ds-count` | `<span>` | Small count pill. |
| `.ds-status-dot` | `<span aria-hidden="true">` | Add `data-status`. Always pair it with text. |
| `.ds-notice` | `<div>`, `<p>` | Warning by default; add `data-tone="danger"`. Direct `strong` and `p` children are styled. |
| `.ds-field` | wrapper | Holds a `<label>` and an `input`, `select` or `textarea`. |
| `.ds-field-row` | wrapper | Two-column row of fields. |
| `.ds-field-hint`, `.ds-field-error` | `<small>` | Link with `aria-describedby`. |
| `.ds-input`, `.ds-select`, `.ds-textarea` | `<input>`, `<select>`, `<textarea>` outside `.ds-field` | The field look. Input and select keep their intrinsic width; the textarea fills its container and resizes vertically. Label each one. The select keeps the native picker. |
| `.ds-segmented` | `<div role="group" aria-label>` | Holds `<button aria-pressed>` options. |
| `.ds-dialog` | `<dialog aria-labelledby>` | Modal card opened with `showModal()` or `command="show-modal"`. Contains `.ds-dialog-title` (heading) and `.ds-dialog-actions` (right-aligned buttons, usually a `<form method="dialog">`). |
| `.ds-empty` | `<div>` | Dashed empty-state box. |
| `.ds-focus-ring` | any focusable | Opts an element outside the primitives into the focus ring. |

## State attributes

| State | How to set it | Effect |
| --- | --- | --- |
| Disabled | native `disabled` | Buttons: opacity .45, `not-allowed` cursor, no hover. Field and standalone controls: opacity .6. |
| Pressed or selected | `aria-pressed="true"` on `.ds-segmented button` | Raised background. |
| Invalid | `aria-invalid="true"` on a field or standalone control | Red border. Pair it with `.ds-field-error`. |
| Tone | `data-tone="neutral"` (the default), `"success"`, `"accent"`, `"warning"` or `"danger"` on `.ds-badge`; `data-tone="danger"` on `.ds-notice` | Colors. |
| Open | `showModal()` on `.ds-dialog` | Modal in the top layer with a backdrop; `Esc` closes it and focus returns to the opener. `closedby="any"` adds click-outside dismissal where supported. |
| Status | `data-status="running"`, `"waiting"`, `"error"` or `"done"` on `.ds-status-dot` | Colors; `running` adds a halo. |
| Keyboard focus | `:focus-visible` | 2px accent outline with a 3px offset. |
| Reduced motion | `prefers-reduced-motion: reduce` | The primary button does not lift on hover. |

## Customization

- **Accent:** set the accent hooks. You don't need theme selectors, and when the hooks are unset the default blue
  is used.

  ```css
  :root { --ds-accent-dark: #9a6bff; --ds-accent-soft-dark: rgba(154,107,255,.16);
          --ds-accent-light: #6d3fd8; --ds-accent-soft-light: rgba(109,63,216,.12); }
  ```

- **Control text size:** buttons and form controls inherit the page's font size. When the body text is larger
  than the controls should be, set the hook; `.ds-btn-compact` keeps its own size.

  ```css
  :root { --ds-control-font-size: var(--ds-text-base); }
  ```

- **Fonts:** set `--ds-font-sans` (and the `serif` and `mono` tokens). The default stack starts with Inter, which
  the package does not ship. Load it yourself or let the stack fall back to the system font. The fallback is
  whatever the operating system maps `sans-serif` to. Some minimal Linux images map every generic family to a
  monospace face: the test VM's fontconfig picks Liberation Mono even for `sans-serif`. That is why the tests load
  Inter from bytes before taking screenshots.
- **Overrides:** application rules win over the primitives at equal specificity when they load later, and always
  win over the zero-specificity resets.

## Dialog

```html
<button type="button" class="ds-btn ds-btn-danger" id="open">Delete…</button>
<dialog class="ds-dialog" id="confirm" aria-labelledby="confirm-title" closedby="any">
  <h2 class="ds-dialog-title" id="confirm-title">Delete this project?</h2>
  <p>This cannot be undone.</p>
  <form method="dialog" class="ds-dialog-actions">
    <button class="ds-btn" value="cancel">Cancel</button>
    <button class="ds-btn ds-btn-danger" value="delete">Delete</button>
  </form>
</dialog>
<script>
  open.addEventListener('click', () => confirm.showModal())
  confirm.addEventListener('close', () => { /* confirm.returnValue is 'delete', 'cancel' or '' */ confirm.returnValue = '' })
</script>
```

- The native modal provides the dialog role, the inert page, focus containment, `Esc` and focus return. Do not add
  `role`, `aria-modal`, a focus trap or an `Esc` handler. Put the safe action first so it gets the initial focus.
- `command="show-modal" commandfor="confirm"` opens it without script in browsers with Invoker Commands (Baseline
  since December 2025). Within this package's browser range, use `showModal()` or load a polyfill.
- `closedby="any"` (click outside to close) is not in Safari. Without it, `Esc` and the back gesture still close the
  dialog. Add a pointer check on the backdrop if click-outside is required there.
- The backdrop and the dialog use `overscroll-behavior: contain`, so scrolling does not reach the page behind.
  Where that is not enforced, the dialog still keeps the page inert. Never lock scrolling with `overflow: hidden`
  on `html` or `body`.

## Usage

**Plain HTML:** see `examples/standalone.html`.

**React** (Intelitex is a React consumer; see `src/components/ui/common.tsx`):

```jsx
import '../design-system/ds.css'   // or <link> in index.html

export function Toolbar({ filter, setFilter, busy }) {
  return <div className="ds-segmented" role="group" aria-label="Filter">
    {['All', 'Failed'].map(f => <button key={f} type="button" aria-pressed={filter === f} onClick={() => setFilter(f)}>{f}</button>)}
    <button type="button" className="ds-btn ds-btn-primary" disabled={busy}>Run</button>
  </div>
}
```

**Svelte:**

```svelte
<script>
  import '../design-system/ds.css'
  let filter = $state('All')
</script>
<div class="ds-segmented" role="group" aria-label="Filter">
  {#each ['All', 'Failed'] as f}
    <button type="button" aria-pressed={filter === f} onclick={() => (filter = f)}>{f}</button>
  {/each}
</div>
<span class="ds-badge" data-tone={filter === 'Failed' ? 'danger' : undefined}>{filter}</span>
```

**Datastar** (state lives in signals; the styling reads only attributes). The example uses Datastar 1.0
attribute syntax; release candidates before 1.0 spelled it `data-on-click` and `data-attr-aria-pressed`:

```html
<link rel="stylesheet" href="/design-system/ds.css">
<div data-signals:filter="'All'">
  <div class="ds-segmented" role="group" aria-label="Filter">
    <button type="button" data-on:click="$filter = 'All'" data-attr:aria-pressed="$filter === 'All'">All</button>
    <button type="button" data-on:click="$filter = 'Failed'" data-attr:aria-pressed="$filter === 'Failed'">Failed</button>
  </div>
</div>
```

Only the plain HTML and React uses are exercised by tests. The Svelte and Datastar snippets show the class and
attribute contract and have not been run here.

## Versioning

This package follows semantic versioning (`package.json`, `CHANGELOG.md`). These count as **breaking**:

- renaming or removing a class, a token, a `data-*` value or a state attribute;
- changing which elements a rule targets (for example, making a selector global);
- raising selector specificity;
- changing a default token value in a way that consumers can see.

Adding classes, tokens or tones is a minor change. Fixing a value so that it matches the documentation is a patch.

## Origin and Intelitex

The values reproduce the Intelitex interface exactly. Intelitex imports the parts one by one in `src/styles.css`.
Its own element defaults (headings, tables with clickable rows, form controls, scrollbars, a global focus ring)
live in `src/styles/intelitex/`.

Intelitex keeps its current production behavior where the primitives deliberately differ. The file
`src/styles/intelitex/ds-compat.css` does this:

- hover still styles disabled buttons;
- disabled danger and icon buttons keep full opacity;
- the hover lift stays under reduced motion;
- every label inside a field gets the field-label style;
- disabled field controls are not dimmed;
- `.active` marks the Sections filter option, because adding `aria-pressed` there would change what assistive
  technologies announce.

Deleting a block from that file is how one of those improvements would ship.

## Tests

Run these from `web/`, one at a time (`--test-concurrency=1`):

- `tests/ds-portable.test.mjs` checks package contract and portability. The package must be plain CSS with only
  `ds-` classes and no undefined properties. The standalone consumer must load only package files, and every token
  must resolve in both themes. It also covers themes, accent hooks, states, focus on every tab stop, reduced
  motion, no leaks into unrelated HTML, and identical primitives with and without a CSS reset.
- `tests/ds-states.test.mjs` checks interaction states. It forces hover, focus-visible and active through the
  DevTools protocol, for the bare package and for the Intelitex compatibility layer. It also compares every
  migrated Intelitex component with its frozen legacy CSS in `tests/ds-legacy/`.
- `tests/ds-gallery.test.mjs` checks the gallery: it loads, keyboard and theme controls work, and the default
  styles equal the Intelitex originals.

## Distribution to other applications

Until the package has its own repository, other applications take it from Intelitex with `git subtree`. The
consumer gets the files as ordinary commits (no submodule, no npm package), pinned to one split commit, and
updates are a single reviewed merge. Never edit the vendored copy; make changes here and pull them.

1. In Intelitex, split this folder, tag the result with the version from `package.json` and push the tag. The split
   is deterministic: the same Intelitex commit always produces the same split commit. Tags are immutable: never
   move, recreate or force-push one; a fix is a new version.

   ```sh
   git subtree split --prefix=web/design-system <intelitex-commit>   # prints the split commit
   git tag design-system-vX.Y.Z <split-commit>
   git push origin refs/tags/design-system-vX.Y.Z
   ```

2. In the consumer, add it once with a squash merge. The merge commit records the split commit as
   `git-subtree-split: <hash>`, which is the version pin.

   ```sh
   git subtree add --prefix=vendor/design-system https://github.com/hipotures/intelitex.git design-system-vX.Y.Z --squash
   ```

3. To update, tag a newer split and run `git subtree pull` with the same prefix and `--squash`.
4. In the consumer's CSS, import `ds.css` before the application's own styles. Customize only through the
   documented hooks (the accent variables, or color tokens under an application theme selector).

Released tags live on the Intelitex remote (`design-system-v0.1.0`, …), so any machine can fetch them. A separate
repository is still a separate decision.

## Extraction

1. Create the new repository from this folder: `git subtree split --prefix=web/design-system -b design-system`,
   then push that branch to the new remote. This keeps the history of these files from their move here onward.
2. Move the tests:
   - `web/tests/ds-portable.test.mjs` goes with the package as is.
   - The parts of `ds-states.test.mjs` that cover the bare package go too.
   - The Intelitex parity parts (`ds-gallery` parity, `ds-states` legacy parity, `tests/ds-legacy/`) stay in
     Intelitex.
3. In the new repository:
   - Add `playwright` as a dev dependency.
   - Point the test paths at the repository root.
   - Remove `"private": true` once publishing is decided.
4. In Intelitex:
   - Replace `../design-system/*.css` in `src/styles.css` with the package (a git dependency, an npm package or a
     vendored copy).
   - Keep the import order.
   - Run the ui-snapshot comparison and the approved visual tests. Both must show zero differences.
5. Gallery-to-application parity tests need both projects. Either keep them in Intelitex against the installed
   package or drop them once the legacy classes are gone.

## License

The package uses the same license as the Intelitex repository, CC0 1.0 Universal (see `LICENSE`, a copy of the
repository's license file).
