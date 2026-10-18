# Phase 15: retire the Python CLI

The Rust desktop client and API server are the application. The Python CLI, its package metadata, its unit suite and the executable parity drivers have been removed. The last source is available in Git history at `cf3f0e0`; no compatibility launcher or fallback was added.

## Frozen reference data

Before deleting the implementation, the final Python code ran against the scenario simulator to record 34 request outputs and ordered API operations. Every translated operation was checked against the actual calls received by the simulator. The capture also recorded the Python configuration reader's verdicts and all 593 discovered test IDs. These live under `parity/fixtures/`, together with the neutral installation input and hashes of the retained guest assets.

Rust scenario tests now read these files, apply the existing reviewed normalization rules and compare the output and ordered operations with the Rust request against the same simulated cluster. The configuration corpus checks Rust verdicts against its expected values and the frozen Python verdicts. The manifest is checked against the frozen original IDs and current Rust test names. The 477 ported and 116 excluded entries contain no `todo`.

The old optional scenario comparison had drifted from current status behavior: Rust additionally inventories pool members. Reference output was captured independently rather than being replaced with Rust output. Any reviewed differences belong in `parity/normalize.toml`, with their rationale. Native status tests check the current pool inventory behavior.

## Application and checks

README now documents the Rust build, first installation, profiles, API server connections and current commands. Earlier phase reports retain historical comparisons; they are not instructions for installing the removed CLI. `parity/README.md` describes the frozen references.

CI and `scripts/check.sh` no longer install or execute the Python application. Native Rust live gates remain; the two scripts requiring the retired CLI have been removed. The slow scenario suite retains its explicit opt-in setting requested by the owner.

Python remains where it is part of the current implementation: the repository build wrapper, terminal/E2E helpers, and workspace-side setup and backup programs. Asset regression tests now check the final reference hashes instead of silently skipping comparison when an old source file is missing.

This phase does not deploy a new API image or change a running E2E lab. Phases 16 (live resize coverage for disk layouts) and 17 (space checks before migration shutdown) remain separate work.

## Verification

The project-local Luna verification agent returned `UDANY` for:

- all `hs-parity` tests, including the explicitly enabled scenario suite;
- `hs-config` tests and the API server's read-route tests;
- affected client setup, Herdr, definitions, TUI, guest-program and backup tests;
- workspace Clippy with warnings denied, formatting and check-script shell syntax;
- manifest validation against the discovered original IDs and current Rust test list.

Socket-based tests ran with local simulator access after sandbox restrictions were identified. No live cluster gate or deployment was run in this phase.
