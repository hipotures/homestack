# Standalone Pangolin API bootstrap

This Python 3.10+ POSIX utility creates one organization-scoped Integration API key named `homestack`. It is independent of the production HomeStack integration and has no third-party Python dependencies. It does not deploy anything.

Run it yourself in a separate terminal. Never give the temporary key to Codex, paste it into chat, put it on a command line, export it, or save it in a file. The hidden prompt expects the complete `ID.secret` credential. The program refuses getpass's echoed-input fallback.

## Manual commands

From the repository root, replace these two **non-secret** configuration values with your Integration API URL and the exact organization ID from your Pangolin dashboard:

```bash
PANGOLIN_ENDPOINT='https://api.example.com/v1'
PANGOLIN_ORG='your-organization-id'
python3 tools/pangolin-bootstrap/bootstrap.py \
  --endpoint "$PANGOLIN_ENDPOINT" --org "$PANGOLIN_ORG"
```

For an already established, authenticated SSH tunnel forwarding local port `18080` to `127.0.0.1:3003` on the VPS, use:

```bash
python3 tools/pangolin-bootstrap/bootstrap.py \
  --endpoint 'http://127.0.0.1:18080/v1' \
  --org 'example-org' \
  --allow-http-loopback
```

IPv6 uses a bracketed literal, for example `http://[::1]:18080/v1`. The flag does not establish or verify an SSH tunnel. Environment proxy settings are ignored, including for loopback HTTP, and redirects remain refused. If using apply mode through the tunnel, include the same flag alongside `--apply` and `--output`.

Check mode makes GET requests only and writes no local files. It confirms authentication, Root access and successful API key/action listing. It reads the bootstrap key's assigned actions, detects existing `homestack` keys across all pages, and optionally reads organization metadata. Creation and assignment privileges are **inferred from metadata**, never exercised in check mode.

With exactly the four stated bootstrap permissions, `GET /org/{orgId}` returns 403. The key-listing route does not check that the organization exists: an empty list is not evidence of existence. Consequently, the report explicitly marks organization existence as unconfirmed. Check the ID in the dashboard yourself. Independent organization validation follows assignment in apply mode. No extra bootstrap permission is required; `getOrg` would allow an earlier confirmation if already granted.

After reviewing the check report, run:

```bash
python3 tools/pangolin-bootstrap/bootstrap.py \
  --endpoint "$PANGOLIN_ENDPOINT" --org "$PANGOLIN_ORG" \
  --apply \
  --output "$HOME/.config/homestack/pangolin-bootstrap/homestack.key"
```

This specific output path is an example of an explicitly approved path: choose it yourself before invoking the command. The utility displays it and the complete action list. Confirm by typing `CREATE homestack IN your-organization-id`, substituting your actual ID. A mismatching confirmation cancels creation.

The final directory must be owned by you with mode `0700`; missing directories are created privately. The credential and its adjacent `homestack.key.state.json` recovery report are created exclusively with mode `0600`. Existing files, symlinked ancestors, insecure ancestors, and paths inside Git checkouts are refused. The credential file contains the new `ID.secret` followed by a newline. Do not display or upload it.

## Apply effects and recovery

Apply performs the following operations:

1. Repeat check-mode reads, show the exact profile, and obtain interactive approval of the organization, permissions and local path.
2. Reserve the private credential/recovery files, then check for a conflicting name again.
3. Record `creation_requested_outcome_unknown` before sending exactly one creation request. Never automatically retry a write.
4. Create the key, record its non-secret ID, immediately save and fsync the new credential, then assign the approved actions.
5. Read back the complete action set with the bootstrap key; require exact equality. Confirm membership in the non-Root organization key listing and require exactly one `homestack` name.
6. Use the new key independently to read the organization, Sites, Resources and domains. Require HTTP 403 when listing API keys, listing key actions, and reading Sites under a randomly generated different organization ID.
7. Save the final sanitized report. Success requires every check to pass.

Only API key creation and action assignment mutate the server. No Sites, Resources, Targets, policies or users are created, changed or deleted. The bootstrap key is never changed, revoked or deleted. The permanent key remains in place even when assignment or validation fails.

On failure, retain the private output files and the terminal's sanitized report. The recovery file records the last stage, key ID when known, and whether the credential was persisted. Never assume an empty credential file means no remote key was created. A lost creation response can leave a key whose secret is unavailable; investigate `homestack` in the intended organization's dashboard. Do not rerun creation with another path to bypass a conflict. This tool deliberately provides no automatic rollback, rotation, deletion or resume. An administrator must decide how to recover a partial key. Once the situation is resolved manually, remove only obsolete local artifacts yourself before any new attempt.

The server does not enforce unique key names or support an idempotency token here. Avoid concurrent provisioning from different terminals or tools. The second check reduces the race window but cannot eliminate it. A concurrent duplicate discovered after creation causes a recovery-required failure, never silent deletion.

## Approved HomeStack action profile

The planned scope is blueprint application, Site CRUD, **public** Resource CRUD, Target CRUD, connection-health inspection, existing resource policy inspection/update, rule-based access control, and organization/domain reads. “Manage” means create, read, update and delete; optional operational controls are excluded.

| Purpose | Exact action IDs |
| --- | --- |
| Apply blueprints | `applyBlueprint` |
| Sites and health | `createSite`, `getSite`, `listSites`, `updateSite`, `deleteSite` |
| Public Resources | `createResource`, `getResource`, `listResources`, `updateResource`, `deleteResource` |
| Targets | `createTarget`, `getTarget`, `listTargets`, `updateTarget`, `deleteTarget` |
| Required metadata | `getOrg`, `listOrgDomains`, `getDomain` |
| Existing resource policies | `getResourcePolicy`, `updateResourcePolicy`, `setResourcePolicyRules` |
| Inline resource rules | `createResourceRule`, `listResourceRules`, `updateResourceRule`, `deleteResourceRule` |

There are **26 actions**. Site listing exposes connection `online` state where applicable; there is no additional health permission in this profile. A local Site may not have `online`. This is connection-health inspection, not health-check configuration or metrics/log collection.

The profile excludes API key administration, organization modification/deletion, users, role administration, identity providers, domains administration, AI operations, restarts, site provisioning keys, direct private/site resource management, shared-policy lifecycle, password/PIN/header/email authentication setup, and assignment of users/roles to policies. Resource policy rules and existing policy attachment through resource update cover the selected rule-based plan. If later integration requirements include identity-based grants, private resources or other authentication methods, review a new exact profile before provisioning; this tool does not speculate about those requirements or grant every related action.

`applyBlueprint` also authorizes the operations expressed by a blueprint, including private resources; review blueprint contents before applying them. No separate blueprint read/list actions are granted.

Read probes confirm only the listed safe operations. Mutating privileges and object-specific Target/policy reads are checked by exact assigned-action metadata and source analysis; no production object is touched to prove them. Organization-scoped credentials can manage all objects covered by those actions in their organization; this is not per-Site isolation.

## Verified API behavior and sources

Research date: 2026-10-20. Source: `fosrl/pangolin` main at [`0842e6df53e898759ed05b77086ddd3c673b6149`](https://github.com/fosrl/pangolin/tree/0842e6df53e898759ed05b77086ddd3c673b6149). This pins the research, not the user's installed server version. The public [Integration API documentation](https://docs.pangolin.net/manage/integration-api) describes Bearer authentication, key types, one-time secret visibility and potentially breaking changes. The [common routes guide](https://docs.pangolin.net/manage/common-api-routes) describes core provisioning flows. Check your own server's `/v1/docs` for deployment-specific differences without entering secrets into third-party documentation.

| Question | Verified behavior |
| --- | --- |
| Create organization key | `PUT /v1/org/{orgId}/api-key`, body `{"name":"homestack"}`. Root plus `createApiKey` required. Returns `data.apiKeyId` and **raw secret** `data.apiKey`; combine as `ID.secret`. The row defaults to non-Root and gets an organization association. |
| List keys | `GET /v1/org/{orgId}/api-keys?limit=100&offset=0`; Root plus `listApiKeys`. Returns organization-associated keys filtered to `isRoot=false`. Does not establish org existence. |
| Assign actions | `POST /v1/org/{orgId}/api-key/{apiKeyId}/actions`, body `{"actionIds":["getOrg", "..."]}`; Root plus `setApiKeyActions`. Replaces existing actions. Validates that the action IDs exist; does not require the caller to possess each granted action. |
| Retrieve actions | GET on the same actions path, with `limit` and `offset`; Root plus `listApiKeyActions`. The utility queries its own ID from the credential prefix and later the new key ID. |
| Organization enforcement | Organization, Site, Resource, Target and policy middleware checks the relevant organization association; Root bypasses organization restrictions. API key admin routes are Root-only. Their action handlers use `apiKeyId`, not `orgId`, as the action target; the organization in that URL is not a containment guard. |
| Validate the new key | Bearer `ID.secret`; independently read the intended organization and allowed collections, compare exact actions, confirm non-Root listing membership, and require denied admin/cross-org reads. `/v1/` is unauthenticated health and cannot validate a key. |
| Remove every action | The current setter uses a nonempty tuple: `actionIds: []` is rejected. Removing all permissions is **not a supported operation through this endpoint**. Do not attempt it as a reversible disable. A zero-action database row would still authenticate; actionless routes exist. |
| Disable or regenerate | No integration-key enabled/disabled field, disable/enable route or regeneration route was found. Creation and deletion exist; the utility uses only creation. Virtual API keys and Site provisioning keys are different key types. No rotation or disabling is assumed. |

Relevant implementation sources:

- [Integration router and guards](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/routers/integration.ts), [edition-specific router extensions](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/private/routers/integration.ts).
- [Organization key creation](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/routers/apiKeys/createOrgApiKey.ts), [filtered key listing](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/routers/apiKeys/listOrgApiKeys.ts).
- [Action assignment](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/routers/apiKeys/setApiKeyActions.ts), [action retrieval](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/routers/apiKeys/listApiKeyActions.ts), [action definitions](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/auth/actions.ts).
- [Authentication](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/middlewares/integration/verifyApiKey.ts), [organization access](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/middlewares/integration/verifyApiKeyOrgAccess.ts), [SQLite key schema](https://github.com/fosrl/pangolin/blob/0842e6df53e898759ed05b77086ddd3c673b6149/server/db/sqlite/schema/schema.ts).

## Security and compatibility limits

The bootstrap secret exists only in process memory, including the necessary Authorization header. It is never accepted through CLI arguments, environment variables or files, and is never saved to disk by the program. Core dumps are disabled. HTTP tracing and environment proxy discovery are disabled; redirects are refused. Normal Python HTTPS certificate and hostname verification is used. HTTPS remains required by default. The explicit `--allow-http-loopback` flag permits HTTP only for literal `127.0.0.1` or `[::1]`; hostnames (including `localhost`), other loopback addresses, and private LAN addresses are refused. Use this only with a trusted local endpoint such as an authenticated SSH tunnel. No insecure TLS switch exists. Server response messages, headers, raw bodies and exception details are never printed, including during failures. Only fixed diagnostic text, counts/booleans, known action names and the new key ID are reported.

Use a trusted machine and terminal without input/session recording, a trusted TLS endpoint, and trusted Pangolin/reverse-proxy logging configuration. The client cannot control server-side or operating-system logging. Filesystem permissions **do not isolate secrets from other processes running under the same OS account**. Python cannot reliably erase immutable secret strings or prevent swap, hibernation, debugger or privileged process inspection. This is not a hardened secret broker.

POSIX facilities (`/dev/tty`, directory FDs, `O_NOFOLLOW`, `resource`, file modes) are required. This is not a Windows utility. Standard output contains sanitized reports; you may retain those reports, but never print the credential file. The recovery report is updated and fsynced at each stage, but abrupt power loss, SIGKILL, a disk failure or a lost creation response can still leave incomplete local recovery information. No automatic cleanup deletes evidence.

The code expects current Pangolin response envelopes (`success`, `error`, `data`), action names and pagination. Key/action listing pagination is exhausted rather than trusting `total`, which may be only a page count. Independent collection checks read one page using each endpoint's defaults. No Root-key listing endpoint or general Integration API action catalog is assumed. Missing routes, mismatched schemas, unknown actions, inconsistent metadata and unexpected authorization results fail closed. Deployment version/edition behavior must be reviewed if it differs from the pinned source.

## Offline verification

Run from the repository root:

```bash
UV_CACHE_DIR=/tmp/homestack-bootstrap-uv-cache \
  uv run --no-project python -m unittest discover \
  -s tools/pangolin-bootstrap -p 'test_*.py' -v
```

The tests start a fake HTTP server on a random `127.0.0.1` port. The tests use the production HTTP loopback exception. The shipped CLI requires HTTPS unless `--allow-http-loopback` is explicitly supplied for a permitted literal loopback host. Credentials are generated synthetically during each test run, never stored as fixture values. No live Pangolin infrastructure or real API key is used. See [VERIFICATION.md](VERIFICATION.md) for the recorded result.
