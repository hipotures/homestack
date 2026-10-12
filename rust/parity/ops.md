# Operation vocabulary

Parity tests compare what the Python and Rust implementations ask Proxmox to
do. Both sides describe each call as a canonical operation:

```
name{key1=value1,key2=value2}
```

- `name` is lower case with underscores.
- Parameters are sorted by key and written as `key=value`, separated by commas,
  without spaces or quoting.
- The same text is produced by `pve-sim` (`rust/crates/pve-sim/src/ops.rs`)
  for every authenticated API call it serves, and by the Python translator
  (`rust/parity/python/homestack_parity/ops.py`) for every shell command
  HomeStack runs.

Routing does not change an operation. A command HomeStack sends to another node
through `ssh -o BatchMode=yes root@<node> '...'` translates to the same
operation as the command run locally, because the API proxies
`/nodes/{node}/...` itself (mapping T2).

A command without a translation raises `UnsupportedCommand` in Python; the run
fails instead of skipping the call. Each phase adds the operations its
functions need.

## Operations

| Operation | API call | Python command | Since |
|---|---|---|---|
| `get{path=P,...}` | `GET /api2/json/P?...` | `pvesh get P [--key value]... --output-format json` | phase 0 |

For `get`, every query parameter (every `pvesh` option except
`--output-format`) becomes a parameter, e.g.
`pvesh get /cluster/resources --type vm --output-format json` is
`get{path=/cluster/resources,type=vm}`.

## How pvesh failures are reproduced

`FixtureTransport` answers like `pvesh` on a PVE 9.2 node (observed on the real
cluster):

| API response | `pvesh` exit code | output |
|---|---|---|
| 2xx | 0 | `data` as compact JSON with sorted keys |
| 501 (no handler) | 1 | `No 'get' handler defined for '<path>'` |
| any other error | 255 | the `message` of the error, without the trailing newline |
