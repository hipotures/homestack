# Bundled dashboard assets

Datastar 1.0.4 (MIT) and Apache ECharts 6.1.0 (Apache-2.0) are pinned
upstream release bundles. `vendor.json` records download URLs and SHA-256
checksums. Associated licenses and notices are retained here. The URLs are
build-time provenance, never runtime requests. Both bundles and their
reproducible gzip representations are embedded with `include_bytes!`.

To update, verify the upstream release, download its exact bundle and license,
update `vendor.json`, asset route, HTML and documentation, and regenerate gzip
with Python `gzip.compress(data, mtime=0)`. No Node runtime or frontend build
step is required in the Alpine image. `dashboard.js` is the local presentation
bridge; it contains no PromQL, pricing or telemetry accounting.
The UI uses The Mechanics (CSS-only, MIT) from the repository-wide
`vendor/the-mechanics/`, shared with Agent Accounts. Never edit it there;
update it with the upstream installer, which writes that directory and records
the version in `.the-mechanics-version`. `main.rs` embeds the stylesheet,
`fonts.css` and the IBM Plex fonts and serves them at
`/assets/the-mechanics/`. `aiusage.css` holds the AI Usage colors (agent and
model identity hues, token kinds from the chart palette) and the page layout
around The Mechanics components; `aiusage-echarts-theme.js` builds the ECharts
theme and series colors from those tokens, so colors are defined in one place.
Design rules for changes are in `.claude/skills/the-mechanics/`.

Upstream references: [Datastar actions](https://data-star.dev/reference/actions),
[SSE protocol](https://data-star.dev/reference/sse_events),
[morph exclusions](https://data-star.dev/reference/attributes#data-ignore-morph),
and [ECharts options](https://echarts.apache.org/en/option.html).
