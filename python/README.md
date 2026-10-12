# HomeStack (Python)

The original Python implementation of HomeStack. It stays the reference
behaviour for the Rust migration and is removed in its last phase
(`../docs/migration/PLAN.md`, phase 15). Usage is described in the
[main README](../README.md); run commands from this directory:

```bash
uv sync
uv run homestack --help
uv run python -m unittest discover -s tests
```
