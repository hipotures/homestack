# AI Usage quota overlays

The timeline has two unchecked controls below the plot: observed quota exhaustion
markers and quota windows/reset boundaries. Hourly windows are shown below seven
days; seven-day windows are shown for ranges of seven days or more. Only a fresh,
enabled account measurement may extend the axis beyond the selected end to its
next reset. Unchecking the window control restores the ordinary time axis.

Quota data comes from Agent Accounts Prometheus gauges, independently of token
telemetry. Host and project filters do not scope account-wide quota observations;
the agent filter does. Each marker identifies the provider, profile and window.
It is a sampled exhaustion observation, not an exact provider request failure.
Maximum utilization/exhaustion over each query interval preserves brief observed
exhaustion in long views; reset and duration use the last observation. Historical
windows require recorded duration metadata; absent metadata is not inferred from
window names. Stale current measurements do not extend the plot into the future.

The overlay uses bounded shared query caches and snapshot/export provenance.
Quota query failures or excessive cardinality disable the overlay
without removing the ordinary usage charts. Antigravity is selectable, with usage
reported as unavailable until a telemetry adapter is defined for its instruments.
No token counts or prices are invented for it.

Focused local verification used the compiled Rust HTTP application with temporary
Prometheus responses, and Chromium with temporary quota/usage snapshots. It
covered default unchecked controls, exhaustion markers, hourly/weekly windows,
future-axis removal on uncheck, provider filtering, fresh current resets and the
Antigravity empty state. Browser checks included desktop and a 390-pixel viewport.
This verification used fixtures, not live provider exhaustion or production data.

Prometheus OTLP unit translation appends `_ratio` to the dimensionless enabled,
stale and exhausted gauges. Queries use those translated names; normalized
exhaustion data is exposed as `exhausted`. A focused check with an actual local
Prometheus OTLP receiver, compiled AI Usage and Chromium confirmed fresh 5-hour
areas on 1h/24h, weekly areas on 7d and future reset boundaries. Samples at 30%
and 65% utilization with an explicit false exhaustion flag produced no markers.
