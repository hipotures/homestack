# Frozen regression references

The Rust migration is complete. No check executes or installs the retired Python CLI. Its last source is available in Git history at `cf3f0e0`.

| Path | Purpose |
|---|---|
| `manifest.toml` | Original Python test IDs mapped to Rust tests or explicit exclusions |
| `fixtures/python-test-ids.txt` | IDs discovered from the final Python suite before removal |
| `fixtures/scenarios/*.json` | Original Python output and canonical API operations for every scenario request |
| `fixtures/config-verdicts.json` | Original Python verdict for every configuration corpus case |
| `fixtures/config.example.toml` | Neutral installation input shared by regression tests |
| `fixtures/guest-assets-sha256.json` | SHA-256 of the retained guest programs and backup units |
| `normalize.toml` | Reviewed differences between reference and Rust behavior, each with a reason |
| `config-corpus.toml` | Valid and invalid configuration cases and expected Rust verdicts |
| `../scenarios/*.toml` | Simulated clusters, reference paths, Rust requests and explicit expectations |

The reference outputs were captured from the Python implementation against `pve-sim`; the capture checked its translated operations against actual simulator calls. Configuration verdicts and test IDs also came from the original Python readers and discovery, rather than from the Rust implementation. Asset hashes preserve the existing byte-for-byte checks without retaining duplicate programs.

`crates/hs-parity/tests/scenarios.rs` runs each Rust request against its simulated cluster and compares normalized output and ordered operations with the frozen reference. Explicit scenario expectations remain additional checks. The slow scenario suite remains opt-in:

```sh
python3 scripts/cargo.py test --locked -p hs-parity --test scenarios -- --ignored
```

Configuration corpus tests run normally without `uv` or Python CLI dependencies. `scripts/check.sh` validates the manifest against the frozen Python test IDs and the current Rust test list. No `todo` entry remains.

Historical `python` fields in the manifest, corpus and normalization rules identify reference provenance and documented differences; they do not select executable code. The operation vocabulary is in [ops.md](ops.md). Historical migration reports describe the removed implementation; current usage is in [README.md](../README.md).
