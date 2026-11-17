# The Mechanics — markup recipes

Copy-ready markup taken from the playground. Icons are omitted (`<svg class="tm-icon">…</svg>` where shown as `[icon]`); use the app's own icon set at 16px (`tm-icon--sm` = 14px). Behaviour for the marked items lives in the playground's reference helpers (`src/playground/lib.js`, `p1.js`, `charts.js`) — port them, do not reinvent them.

## Contents
- Page shell and page head
- Region, panel, panel with meta toggle
- Status plate, badge, alert, empty state
- Buttons, toggle, segmented control
- Fields: text with error, clear button, stepper, select
- Table with bulk bar
- Section head with filter, collapsible, danger zone
- Dialog

## Page shell and page head
```html
<html lang="en" data-tm-theme="light">
<body class="tm-root">
<div class="tm-shell" data-sidebar="expanded">
  <header class="tm-topbar"><a class="tm-brand" href="/"><span class="tm-brand__mark">TM</span>App</a></header>
  <nav class="tm-sidebar" aria-label="Destinations"><div class="tm-sidebar__inner">
    <div class="tm-nav__group" data-open="true">
      <button type="button" class="tm-nav__section tm-nav__toggle tm-label" aria-expanded="true" aria-controls="nav-main">Main [icon chevron]</button>
      <ul class="tm-nav" id="nav-main">
        <li><a class="tm-nav__item" href="/runs" aria-current="page">[icon]<span class="tm-nav__label">Runs</span></a></li>
      </ul>
    </div>
  </div></nav>
  <main class="tm-main" id="main">
    <header class="tm-page-head">
      <div class="tm-page-head__text">
        <div class="tm-eyebrow">Area · Section</div>
        <h1 class="tm-page-title">Page title</h1>
        <p class="tm-subtitle">One line of context.</p>
      </div>
      <div><button class="tm-btn tm-btn--primary">[icon]Primary action</button></div>
    </header>
    …
  </main>
</div>
```
A collapsed nav group (`data-open="false"`, `aria-expanded="false"`) still shows its current page.

## Region, panel, panel with meta toggle
```html
<section class="tm-region" aria-labelledby="r1">
  <header class="tm-region__head"><h2 class="tm-region__title" id="r1">Accounts</h2></header>
  <div class="tm-region__body">…</div>
</section>

<article class="tm-panel" aria-labelledby="p1">
  <header class="tm-panel__head">
    <h3 class="tm-panel__title" id="p1">Latency p95</h3>
    <span class="tm-panel__meta">ms</span>
  </header>
  <div class="tm-panel__body">…</div>
</article>

<!-- a small display option lives on the unit, not in a new row -->
<header class="tm-panel__head">
  <h3 class="tm-panel__title">Worker utilization</h3>
  <button type="button" class="tm-meta-toggle" aria-pressed="false" aria-label="Auto label color (values in %)" title="Label color — off: text color · on: per cell">%</button>
</header>
```
Several panels side by side: `.tm-panel-grid` (cells share the outer frame, 2px dividers).

## Status plate, badge, alert, empty state
```html
<!-- standalone: page head, panel head, KPI tile, image overlay -->
<span class="tm-status tm-status--sm"><span class="tm-mark" data-state="warning" aria-hidden="true"></span>Delayed 17 min</span>

<!-- inline: table cell, list row, line of text -->
<span class="tm-badge tm-badge--success">Active</span>
<span class="tm-badge tm-badge--count">3</span>

<div class="tm-alert tm-alert--warning" role="status">
  [icon alert]
  <div><div class="tm-alert__title">Telemetry delayed</div><div class="tm-alert__body">Data is 17 minutes old.</div></div>
</div>
<!-- hideable warning + restore button in the page head: dismissibleAlert() -->

<div class="tm-state">
  <div class="tm-state__icon">[icon]</div>
  <div class="tm-state__title">No accounts match “zzz”</div>
  <div class="tm-state__body">Try a different search term or clear the filters.</div>
  <button class="tm-btn tm-btn--sm tm-btn--outline">Clear filters</button>
</div>
```
`.tm-mark` states: complete, active, info, review, warning, error, pending, skipped, none. Alert tones: info, success, warning, danger.

## Buttons, toggle, segmented control
```html
<button class="tm-btn tm-btn--primary">Save</button>
<button class="tm-btn">Secondary</button>
<button class="tm-btn tm-btn--outline">Outline</button>
<button class="tm-btn tm-btn--ghost">Ghost</button>
<button class="tm-btn tm-btn--danger-outline">[icon]Deactivate</button>
<button class="tm-btn tm-btn--primary" aria-busy="true">Saving</button>

<!-- on/off: fixed label, ink fill when on -->
<button class="tm-btn tm-btn--sm tm-btn--outline tm-btn--toggle" aria-pressed="false">[icon filter]Selected only</button>

<!-- one of N: selection fill -->
<fieldset class="tm-segmented tm-segmented--sm" aria-label="Range">
  <legend class="tm-sr-only">Range</legend>
  <label class="tm-segmented__option"><input type="radio" name="range" value="24h" checked>24 h</label>
  <label class="tm-segmented__option"><input type="radio" name="range" value="7d">7 d</label>
</fieldset>
```

## Fields
```html
<div class="tm-field">
  <label class="tm-field__label" for="email">Email <span class="tm-req">*</span></label>
  <input class="tm-input" id="email" type="email" aria-invalid="true" aria-describedby="email-h email-e">
  <span class="tm-field__hint" id="email-h">Use the example.test domain.</span>
  <span class="tm-field__error" id="email-e">“alex@” is not a complete email address.</span>
</div>

<!-- search with affix; enhanceClearable() adds the ✕ (.tm-input-clear) to text fields -->
<div class="tm-input-group">
  <span class="tm-affix">[icon search]</span>
  <input class="tm-input" type="search" placeholder="Search name, email, id…" aria-label="Search">
</div>

<!-- number: stepper, never native spinners -->
<div class="tm-stepper">
  <button type="button" class="tm-stepper__btn" aria-label="Decrease quantity">[icon minus]</button>
  <input class="tm-input" type="number" inputmode="decimal" id="qty" value="42" min="0" max="99">
  <button type="button" class="tm-stepper__btn" aria-label="Increase quantity">[icon plus]</button>
</div>

<select class="tm-select" id="team"><option>Research</option><option>Sales</option></select>
<!-- Firefox: enhanceSelects() opens the same styled list; dates: enhanceDateTime() -->
```

## Table with bulk bar
```html
<div class="pg-bulk" role="region" aria-label="Bulk actions"><!-- app layout class; background = --tm-select-bg -->
  <span class="tm-badge tm-badge--count">3</span><span>selected</span>
  <button class="tm-btn tm-btn--sm tm-btn--outline tm-btn--toggle" aria-pressed="false">Selected only</button>
  <button class="tm-btn tm-btn--sm tm-btn--outline">Export selected</button>
  <button class="tm-btn tm-btn--sm tm-btn--danger-outline">Deactivate selected</button>
  <!-- optional "Select all N matching" goes here, last -->
  <span class="tm-toolbar__spacer"></span>
  <button class="tm-btn tm-btn--sm tm-btn--ghost">Clear selection</button>
</div>
<div class="tm-table-wrap">
  <table class="tm-table tm-table--stack">
    <caption class="tm-sr-only">Accounts</caption>
    <thead><tr>
      <th scope="col" class="tm-col-check"><input class="tm-check" type="checkbox" aria-label="Select all on this page"></th>
      <th scope="col" aria-sort="ascending"><button class="tm-sort">Account</button></th>
      <th scope="col" class="tm-col-num">Balance</th>
    </tr></thead>
    <tbody>
      <tr aria-selected="true">
        <td class="tm-col-check"><input class="tm-check" type="checkbox" checked aria-label="Select Marta"></td>
        <td class="tm-cell-primary" data-label="Account">Marta Kowalska</td>
        <td class="tm-col-num tm-num" data-label="Balance">−$12.60</td>
      </tr>
    </tbody>
  </table>
</div>
```

## Section head with filter, collapsible, danger zone
```html
<div class="tm-section-head">
  <h2 class="tm-section-head__title">Sections</h2>
  <span class="tm-section-head__meta">22 detected · 3 excluded</span>
  <fieldset class="tm-segmented tm-segmented--sm" aria-label="Filter sections">…</fieldset>
</div>

<details class="tm-collapsible">
  <summary><span class="tm-collapsible__title">Pipeline models</span><span class="tm-collapsible__desc">Assign profiles to P1–P5</span></summary>
  …
</details>

<section class="tm-panel" aria-labelledby="dz">
  <header class="tm-panel__head"><h2 class="tm-panel__title" id="dz">Archive workspace</h2></header>
  <div class="tm-panel__body tm-danger-zone">
    <p>What happens and what is kept.</p>
    <button class="tm-btn tm-btn--danger-outline" disabled aria-describedby="dz-n">Archive</button>
    <p class="tm-danger-zone__note" id="dz-n">Why it is disabled right now.</p>
  </div>
</section>
```

## Dialog
```html
<dialog class="tm-dialog" aria-labelledby="d1"><form method="dialog">
  <div class="tm-dialog__head"><h2 class="tm-dialog__title" id="d1">Deactivate 3 accounts?</h2>
    <button class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" aria-label="Close">[icon x]</button></div>
  <div class="tm-dialog__body">They lose access until reactivated.</div>
  <div class="tm-dialog__foot">
    <button class="tm-btn tm-btn--outline" value="cancel">Cancel</button>
    <button class="tm-btn tm-btn--danger" value="confirm">Deactivate</button>
  </div>
</form></dialog>
```
Drawer: `<dialog class="tm-drawer">`. Non-modal side panel: `<aside class="tm-sheet" popover="manual">`. Command palette: `.tm-palette` (see `p1.js`).
