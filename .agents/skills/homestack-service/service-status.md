# HomeStack service telemetry, version 3

This is HomeStack's application-side wire contract, independent of language
or web framework. It needs no manifest field. HTTP services serve
`GET /.well-known/homestack/status` on the same HTTP port referenced by
`runtime.healthcheck`. Services without HTTP have no telemetry endpoint.
No arbitrary URL or extra listener is inferred from a TCP healthcheck or port
name.

The endpoint reports only what the application process alone knows. The
container's CPU, memory, pressure, I/O and filesystem space are measured by
`homestack-pve-agent` on the PVE node, and its uptime by Proxmox; HomeStack
reads both from Prometheus. A service never measures its cgroup, mounts or
host for HomeStack.

## HTTP and JSON

Return `200`, `Content-Type: application/json`, `Cache-Control: no-store`.
The endpoint is read-only and requires no deployment credentials. It reports
process liveness, not dependency readiness; `/health/ready` remains the
application readiness authority.

This is a private management-network endpoint, not authorization to expose
telemetry publicly. Keep it on the existing listener and prevent public
proxy routing to this path when public application exposure is configured.

```json
{
  "schema": 3,
  "status": "pass",
  "process_rss_bytes": 61640704
}
```

- `schema`: integer `3`. Consumers ignore additional fields but reject an
  unsupported version or malformed fields. This is separate from YAML schema.
- `status`: `pass` or `fail`, the same verdict as `/health/live`, including
  the heartbeat age. `fail` is available when the application can answer but
  considers its process unhealthy.
- `process_rss_bytes`: the service process's own resident set, `VmRSS` from
  `/proc/self/status` in bytes; `null` when unavailable, never zero. Always
  present.

Read the value when a request arrives; no timer, sampler or background worker
runs for this endpoint. Keep the response at most 4 KiB.

## HomeStack consumption

New common-managed deployments record `status-port <number>` in non-secret
PVE ownership metadata from the manifest's HTTP healthcheck port. The fixed
path is not a YAML field. Read from the actual configured container IP, not
an address calculated from CTID. No probing every port, redirects, proxies,
credential forwarding or TLS-verification bypasses.

HomeStack queries a service only while a client is requesting status (TUI or
web interface open); it never polls services in the background. It bounds
timeout and response size, checks the version and exposes the report as
`service_status.application` in its authenticated `/v1/status` response, next
to the container measurement in `service_status.container`. A missing or
unsupported endpoint leaves these application fields unknown; it never
changes lifecycle ownership or health.
