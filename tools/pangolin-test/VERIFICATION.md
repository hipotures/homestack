# Verification status

Sixteen focused offline tests and syntax compilation passed for the detached Pangolin CLI lifecycle. Logs:

- `/tmp/homestack-pangolin-lifecycle-tests.log`
- `/tmp/homestack-pangolin-lifecycle-compile.log`

The tests use a fake local Pangolin HTTP API, runtime-generated synthetic credentials and a tiny compiled fake ELF CLI. They prove that the connector survives the process that starts it, retains its lifetime lock, and is reused without duplicate startup. They also verify private files, no secrets in arguments/environment/control metadata, blocked regular-file writes, idempotent targeted stop, PID reuse/executable mismatch rejection, missing ownership record rejection, unsafe lock rejection, refusal to start an already Online foreign connection, recovery after health timeout, current Site/Target observations and separation of public TLS results.

An independent status test runs after its synthetic start command has exited. It checks two live API observations and refuses a positive result when Site connectivity or Target health changes. Three existing provisioning/configuration/cleanup checks also passed. The broad suite was not repeated.

The agent did not run start/status/stop against live Pangolin, read real credential/settings files, query OVH or change the VPS during this lifecycle work. Offline success is not a live Site/Resource health claim.

Run `start` manually, let it return, then run `status`. Only `healthy_after_start_return: true` from that independent live status establishes the requested post-return Site/Target health. Public TLS is reported separately; authenticated public health still requires a user browser session and is never inferred from an unauthenticated rejection.

Earlier, separately approved VPS work fixed public and upstream TLS for the existing test hostname. That result predates this lifecycle change. The existing test objects, DNS record and VPS configuration are reused untouched by lifecycle operations. No commits, pushes or releases were performed.
