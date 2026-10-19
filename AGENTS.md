# Local Rust builds and tests

- Use `python3 scripts/cargo.py build ...` and `python3 scripts/cargo.py test ...` instead of invoking `cargo build` or `cargo test` directly. This applies to the coding agent's own verification commands, including commands redirected to log files.
- Use the same wrapper for `cargo check` and `cargo clippy`: `python3 scripts/cargo.py check ...` or `python3 scripts/cargo.py clippy ...`.
- `scripts/check.sh` already runs through the wrapper and cleans up after the entire suite succeeds.
- Do not run unwrapped Cargo builds or tests concurrently with wrapped commands. The wrapper serializes its commands and cleanup; Cargo itself releases its build lock before running tests.
- Successful wrapped commands remove disposable hashed executables and incremental state while preserving top-level applications, dependency libraries and E2E data. Failed or interrupted commands retain their artifacts for diagnosis.
- Do not replace this cleanup with `cargo clean` or removal of `target/`: that directory also contains running E2E labs, credentials, fixtures and diagnostic reports.

# Quiet verification

- Delegate build, test and lint execution to the project-local `test_runner` agent defined in `.codex/agents/test_runner.toml`.
- Use `gpt-6-luna` with reasoning effort `high` for that agent.
- Keep command output in task-specific files under `/tmp`; do not stream complete logs into the conversation.
- The verification agent returns only `UDANY` or `NIEUDANY`. Inspect its log privately when a failure needs diagnosis.

# API image releases

- Agents must decide whether each completed change needs a new API image and perform the release automatically when required, without waiting for the user to request a tag.
- Ordinary branch pushes run checks only. Publish the API container by pushing a numeric Git tag (`0.2.0`, `0.2.1`, ...); the image tag is exactly the Git tag, without a `v` prefix or commit hash. Image publication waits for successful checks.
- After testing, committing and pushing a completed change, create a release tag if deploying that change requires replacing the API container: server routes, jobs, authentication, persistence, server-side operations or models, dependencies used by the server, or the container's runtime, Caddy and service files. Judge shared-crate changes by their effect on the server.
- Do not tag changes confined to the TUI, CLI, client-side installer/deployer, documentation, tests or development tooling when the running API container is unaffected.
- Use `python3 scripts/tag-api.py` to fetch upstream tags, automatically choose the next patch version (starting at `0.2.0`), create an annotated tag on the clean, already pushed HEAD, and push that tag to the branch's upstream remote. Use `--dry-run` to preview the next version without creating or pushing a tag.
- Never move, overwrite or force-push release tags. If a tag push is rejected, stop and report it. A release publishes an image; replacing running containers remains a separate deployment action.
