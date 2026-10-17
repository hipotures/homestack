# Local Rust builds and tests

- Use `python3 scripts/cargo.py build ...` and `python3 scripts/cargo.py test ...` instead of invoking `cargo build` or `cargo test` directly. This applies to the coding agent's own verification commands, including commands redirected to log files.
- Use the same wrapper for `cargo check` and `cargo clippy`: `python3 scripts/cargo.py check ...` or `python3 scripts/cargo.py clippy ...`.
- `scripts/check.sh` already runs through the wrapper and cleans up after the entire suite succeeds.
- Do not run unwrapped Cargo builds or tests concurrently with wrapped commands. The wrapper serializes its commands and cleanup; Cargo itself releases its build lock before running tests.
- Successful wrapped commands remove disposable hashed executables and incremental state while preserving top-level applications, dependency libraries and E2E data. Failed or interrupted commands retain their artifacts for diagnosis.
- Do not replace this cleanup with `cargo clean` or removal of `target/`: that directory also contains running E2E labs, credentials, fixtures and diagnostic reports.
