# Changelog

## 0.2.0

All additions; nothing existing changes unless a consumer opts in.

- `--ds-control-font-size` hook: control text size independent of the page's body size (unset: inherited, as before).
- Icon and text layout: an `<svg>` child of `.ds-btn` or a `.ds-segmented` button lays out inline with a gap.
- `.ds-panel-overflow-visible` and `.ds-panel-overflow-auto` panel modifiers.
- Badge tones: `data-tone="warning"`; `data-tone="neutral"` documented as the default.
- `.ds-input`, `.ds-select`, `.ds-textarea`: the field look for controls outside `.ds-field`, with disabled,
  invalid, placeholder and focus-ring states.
- `.ds-dialog` (+ `-title`, `-actions`) for the native `<dialog>` opened with `showModal()`.

## 0.1.0

- First portable version: `ds.css` entry point, tokens, dark and light themes, primitives (buttons, icon button,
  panel, table card, table, progress, badge, count, status dot, notice, field, segmented control, empty state) and a
  focus ring scoped to the primitives.
- Global element rules removed; `base.css` is optional page defaults with zero specificity.
- Distribution to other applications through `git subtree` split commits, pinned by tag (`design-system-v0.1.0`).
