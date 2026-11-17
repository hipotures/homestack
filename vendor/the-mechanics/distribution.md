# Distribution and integration guide

The Mechanics ships as **plain, compiled CSS** with no runtime JavaScript. Tailwind CSS and daisyUI are used only inside this playground; consumers do not need them.

> Integration into existing applications (Intelitex, HomeStack, Gitiary, …) is **not** part of this work and requires owner approval. The snippets below describe the intended path only.

## 1. Get the distribution

**From a release (recommended).** Every release attaches `the-mechanics-css-<tag>.zip`, `the-mechanics-skill-<tag>.zip`, `SHA256SUMS` and `install.sh`. From the consuming project's root:

```sh
curl -fsSL https://github.com/hipotures/the-mechanics/releases/latest/download/install.sh | sh -s -- --version v0.1.5 --css public/vendor/the-mechanics
```

This puts the files below into `--css` (default `vendor/the-mechanics`) and the agent skill into `.claude/skills/the-mechanics` (`--no-skill` to skip). Commit the vendored files in the consuming project so builds do not depend on GitHub. See `scripts/install.sh --help` for all options.

**From source.** Clone, then:

```sh
corepack pnpm install
corepack pnpm build:css      # → dist/the-mechanics/
```

| File | Contents |
|---|---|
| `the-mechanics.css` | tokens + three themes + scoped base + all `.tm-*` components, in cascade layers `tm.tokens < tm.themes < tm.base < tm.components` |
| `fonts.css` + `fonts/*.woff2` | IBM Plex Sans/Mono 400–700, Latin + Latin Extended (Polish), SIL OFL 1.1 license files included |
| `the-mechanics.daisyui.css` | optional bridge that maps daisyUI 5 variables to `--tm-*` tokens |

Source files (`src/the-mechanics/*.css`) are also importable directly by any bundler.

`dist/` is git-ignored; consumers should install a pinned release (above), or pin a commit and build.

## 2. Public contract

- **Theme selection:** `data-tm-theme="light" | "dark" | "coffee"` on `<html>` (any subtree also works). Light is the default.
- **Base styles:** opt-in via `class="tm-root"` on `<body>` or an app root. Nothing global is restyled without it except `--tm-*` variables on `:root`.
- **Tokens:** every `--tm-*` custom property listed in `DESIGN.md §4` is public. Others (`--_bg`, `--_series`, …) are private.
- **Components:** classes prefixed `tm-` with BEM-style elements (`tm-panel__head`) and modifiers (`tm-btn--primary`). State is expressed through native attributes: `disabled`, `aria-invalid`, `aria-busy`, `aria-pressed`, `aria-selected`, `aria-current`, `aria-sort`, `:checked`, `:indeterminate`, `[open]`, `:popover-open`.
- **Input-modality hook (optional, recommended with scripted popups):** `data-tm-input="pointer" | "key"` on `<html>`, set from capturing `pointerdown` / `keydown` listeners (`trackInputModality()` in `src/playground/lib.js`). With it, popup items focused by script after a mouse action show no focus frame, and a select clicked open shows no lift. Without it, browsers may paint that focus as keyboard focus before the first click on the page.
- **Overrides:** consumer CSS that is not in a layer always wins over the library; no `!important` needed.
- **Behavior:** provided by native elements (`<dialog>`, `<details>`, `popover`, form controls). Small optional behaviors used in the playground (menu arrow keys, roving lists, toasts, chart tooltips, select fallback `enhanceSelects`, date/time picker `enhanceDateTime`, restorable alerts `dismissibleAlert`, clear buttons `enhanceClearable`, `data-tm-tip` tooltips `enhanceTips`, inline form validation `validateInline` / `setFieldError`) live in `src/playground/lib.js` and `charts.js` as references, not as part of the CSS contract.

## 3. Plain HTML / vanilla JS

Working example: [`examples/plain-html/index.html`](../examples/plain-html/index.html) (verified rendering in Light and Coffee with only the compiled CSS — see `docs/screenshots/states/plain-html-*.jpg`).

```html
<html lang="en" data-tm-theme="coffee">
<link rel="stylesheet" href="/vendor/the-mechanics/fonts.css">
<link rel="stylesheet" href="/vendor/the-mechanics/the-mechanics.css">
<body class="tm-root">
  <section class="tm-region">
    <header class="tm-region__head"><h2 class="tm-region__title">Workers</h2></header>
    <div class="tm-region__body">
      <button class="tm-btn tm-btn--primary">Run</button>
    </div>
  </section>
</body>
```

Theme switch: `document.documentElement.dataset.tmTheme = "dark"`.

## 4. React

```jsx
// main.jsx
import "the-mechanics/dist/the-mechanics/fonts.css";
import "the-mechanics/dist/the-mechanics/the-mechanics.css";

export function Panel({ title, meta, children }) {
  return (
    <section className="tm-panel">
      <header className="tm-panel__head">
        <h3 className="tm-panel__title">{title}</h3>
        {meta && <span className="tm-panel__meta">{meta}</span>}
      </header>
      <div className="tm-panel__body">{children}</div>
    </section>
  );
}

export const Button = ({ variant = "secondary", busy, ...p }) => (
  <button className={`tm-btn ${variant !== "secondary" ? `tm-btn--${variant}` : ""}`} aria-busy={busy || undefined} {...p} />
);
```

Wrap with whatever component library the app already has; The Mechanics only needs class names and ARIA attributes. Use `useEffect` to set `document.documentElement.dataset.tmTheme`.

## 5. Svelte

```svelte
<script>
  import "the-mechanics/dist/the-mechanics/the-mechanics.css";
  export let invalid = false;
  export let error = "";
</script>
<div class="tm-field">
  <label class="tm-field__label" for="email">Email</label>
  <input class="tm-input" id="email" aria-invalid={invalid} aria-describedby="email-err" />
  {#if invalid}<span class="tm-field__error" id="email-err">{error}</span>{/if}
</div>
```

## 6. Rust / Datastar (server-rendered)

Serve the three files as static assets (e.g. `tower-http::services::ServeDir` or `include_str!`) and emit the same markup from templates (Askama/Maud/minijinja). Datastar signals map naturally to the attribute-based state contract:

```html
<html data-tm-theme="light" data-attr-data-tm-theme="$theme">
<body class="tm-root" data-signals="{theme: 'light', busy: false}">
  <fieldset class="tm-segmented" aria-label="Theme">
    <label class="tm-segmented__option"><input type="radio" name="t" value="dark" data-bind-theme>Dark</label>
  </fieldset>
  <button class="tm-btn tm-btn--primary" data-attr-aria-busy="$busy" data-on-click="@post('/run')">Run</button>
</body>
```

Because state is plain attributes (`aria-busy`, `aria-invalid`, `aria-selected`), server-sent fragment patches restyle correctly without any client component runtime.

## 7. Tailwind CSS 4 + daisyUI 5 applications (optional)

```css
@layer theme, base, tm, components, utilities;
@import "tailwindcss";
@plugin "daisyui" { themes: false; }
@import "the-mechanics/src/the-mechanics/index.css";
@import "the-mechanics/src/the-mechanics/adapters/daisyui.css";
```

daisyUI components (`btn`, `toggle`, `range`, `alert`, `tabs`, …) then use The Mechanics colors, radii and border width in all three themes (see “daisyUI bridge” in the Component Gallery; the playground loads daisyUI with `prefix: "d-"` to avoid name clashes). Declaring `tm` before `components, utilities` lets Tailwind utilities still adjust layout.

## 8. Charts (Apache ECharts, optional)

`src/the-mechanics/adapters/echarts.js` is a dependency-free ES module (it does not import ECharts):

```js
import * as echarts from "echarts";
import { readChartTokens, createEChartsTheme, referenceLine } from "the-mechanics/src/the-mechanics/adapters/echarts.js";

const tokens = readChartTokens(document.documentElement);   // resolves --tm-* for the active theme
echarts.registerTheme("tm", createEChartsTheme(tokens));
const chart = echarts.init(el, "tm", { renderer: "svg" });
chart.setOption({ /* … */ series: [{ type: "line", data, markLine: referenceLine(tokens, 750, "750 ms") }] });
```

Composite-chart helpers from the same module: `peakMarker(tokens, { coord, label })` → `series.markPoint`, `rangeArea(tokens, [[from, to, name]], tone)` → `series.markArea`, `rangeBrush(tokens)` → top-level `brush` (activate with `dispatchAction({ type: "takeGlobalCursor", key: "brush", brushOption: { brushType: "lineX" } })` and listen to `brushSelected`), `dayBoundaryLabels(tokens, dateFor)` → category `axisLabel`. CSS: `.tm-panel__head--rich`, `.tm-panel__heading`, `.tm-panel__subtitle`, `.tm-panel__actions`, `.tm-chart-readout`, `.tm-chart-options`.

After switching `data-tm-theme`, re-read tokens, register the theme again and re-create the charts (see `mountChart` in `src/playground/charts.js` for a complete example with keyboard tooltips and HTML legends).

## 9. Versioning

The package is not published. Treat `--tm-*` tokens and `.tm-*` class names as the stable surface from v0.1.0; breaking renames should bump the minor version until 1.0.
