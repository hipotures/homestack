# Operation vocabulary

Parity tests compare what the Python and Rust implementations ask Proxmox to
do. Both sides describe each call as a canonical operation:

```
name{key1=value1,key2=value2}
```

- `name` is lower case with underscores.
- Parameters are sorted by key and written as `key=value`, separated by commas,
  without quoting. In values, `%`, `,`, `=`, `{` and `}` are written `%25`,
  `%2C`, `%3D`, `%7B` and `%7D`, so a shell command fits in one parameter.
- The same text is produced by `pve-sim` (`crates/pve-sim/src/ops.rs`)
  for every authenticated API call it serves, and by the Python translator
  (`parity/python/homestack_parity/ops.py`) for every shell command
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
| `guest_ping{vmid=V}` | `POST /nodes/N/qemu/V/agent/ping` | `qm guest cmd V ping` (also behind `timeout -k 2s 5s`) | phase 2 |
| `guest_exec{cmd=C,vmid=V}` | `POST /nodes/N/qemu/V/agent/exec` with `command=["/bin/sh","-lc",C]` | `qm guest exec V -- /bin/sh -lc C` | phase 2 |
| `guest_exec_status{vmid=V}` | `GET /nodes/N/qemu/V/agent/exec-status?pid=...` | the same `qm guest exec`, which waits for the command | phase 2 |
| `get{path=/nodes/N/qemu/V/agent/file-read,file=F,offset=O,count=C,decode=0}` | `GET .../agent/file-read` | none: the server's guest verification (decision 5) | phase 3 |

The node is not part of the guest operations: `qm` runs on the control node
or, through `ssh root@<node>`, on the VM's node, which the translator passes
to the simulator call. One `qm guest exec` is two operations because Proxmox
answers `exec` with a PID and `qm` then polls `exec-status`; the PID is left
out because it differs between runs. Other `POST` calls are recorded as
`post{path=P}` until a phase gives them a name.

For `get`, every query parameter (every `pvesh` option except
`--output-format`) becomes a parameter, e.g.
`pvesh get /cluster/resources --type vm --output-format json` is
`get{path=/cluster/resources,type=vm}`. A query parameter called `path` is
written `query_path`, so `GET /access/permissions?path=/vms` is
`get{path=/access/permissions,query_path=/vms}`. Python never makes that call
(it runs as `root@pam`); the server's capability probe does (phase 1).

## How pvesh failures are reproduced

`FixtureTransport` answers like `pvesh` on a PVE 9.2 node (observed on the real
cluster):

| API response | `pvesh` exit code | output |
|---|---|---|
| 2xx | 0 | `data` as compact JSON with sorted keys |
| 501 (no handler) | 1 | `No 'get' handler defined for '<path>'` |
| any other error | 255 | the `message` of the error, without the trailing newline |
