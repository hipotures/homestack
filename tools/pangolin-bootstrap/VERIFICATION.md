# Sanitized verification report

Date: 2026-10-20.

Result: **PASS — 30 offline tests**, Python syntax compilation, and CLI `--help`.

Verification was delegated to the repository's `test_runner` agent according to `.codex/agents/test_runner.toml`. Test output was retained in task-specific `/tmp` logs rather than streamed into the conversation:

- `/tmp/homestack-pangolin-loopback-tests.log`
- `/tmp/homestack-pangolin-loopback-compile.log`
- `/tmp/homestack-pangolin-loopback-help.log`

Commands used:

```bash
UV_CACHE_DIR=/tmp/homestack-bootstrap-uv-cache uv run --no-project python \
  -m unittest discover -s tools/pangolin-bootstrap -p 'test_*.py' -v
UV_CACHE_DIR=/tmp/homestack-bootstrap-uv-cache uv run --no-project python \
  -m py_compile tools/pangolin-bootstrap/bootstrap.py tools/pangolin-bootstrap/test_bootstrap.py
UV_CACHE_DIR=/tmp/homestack-bootstrap-uv-cache uv run --no-project python \
  tools/pangolin-bootstrap/bootstrap.py --help
```

The initial restricted execution could not bind a local socket. Final verification ran with permission to bind the fake server to loopback and create a pseudo-terminal. An intermediate failure exposed an empty `/tmp/.git` sandbox marker; repository detection now checks actual Git metadata and still explicitly rejects this HomeStack checkout. Final checks completed successfully after these corrections.

Coverage:

- Successful bootstrap authentication and Root authorization; GET-only check mode.
- Invalid credentials, missing Root privileges, and each missing bootstrap management permission.
- Metadata-only inference of write privileges, and explicit uncertainty about organization existence when `getOrg` is unavailable.
- Successful optional organization validation when permission is present.
- Creation, exact action assignment, independent authentication, allowed collection reads, denied key administration, and denied cross-organization access.
- Malformed JSON, invalid envelopes, incompatible creation data, unexpected extra permissions, and independent new-key authentication failure.
- Interrupted assignment and lost creation response: credential/recovery evidence preserved, no automatic retries or deletion.
- Existing names, conflicts on later pages, conflicts arising during confirmation, and repeated runs without duplication.
- Mode `0700` directories, mode `0600` files, exclusive creation, refusal to overwrite, and rejection of symlinks, insecure directories, repository paths and relative paths.
- Suppression of synthetic secrets in errors, reports and recovery state.
- HTTPS by default; explicit HTTP loopback opt-in for literal `127.0.0.1` and `::1`.
- URL matrix covering IPv4/IPv6, hostname and numeric aliases, LAN addresses, paths, credentials, queries and fragments; redirects remain refused.
- Successful loopback check mode with hostile proxy environment variables, proving proxy settings are ignored.
- A real pseudo-terminal test of hidden synthetic secret input and interactive confirmation.

All credentials were generated synthetically at runtime. Test HTTP was confined to a random `127.0.0.1` port; production code requires verified HTTPS unless HTTP loopback is explicitly enabled for a permitted literal host. No request was made to the actual SSH tunnel on port `18080`. No actual Pangolin endpoint or API key was used. No production Sites, Resources, Targets, policies, accounts, containers or HomeStack installation were accessed or changed.

Public source research was pinned to Pangolin revision `0842e6df53e898759ed05b77086ddd3c673b6149`. Source-level conclusions and deployment assumptions are documented in [README.md](README.md). The fake server tests verify client behavior; they do not certify compatibility with an uninspected installed Pangolin version, actual network reachability, certificate configuration or production write authorization.

No commit, push, release tag or deployment was performed.
