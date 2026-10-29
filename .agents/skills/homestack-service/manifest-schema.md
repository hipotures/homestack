# HomeStack service manifests, schemas 1 and 3

Schema 1 remains supported with the rules below. It requires no migration.
Schema 3 is an additional, opt-in three-file contract documented at the end
of this file. Neither manifest version changes the independent HTTP
service-status telemetry response schema 3.

The contract between an application repository and HomeStack: portable facts
about the service's image, its default resources, persistence, instance
topology and Pangolin exposure. HomeStack fills its deployment form from the
manifest; the operator may change any value there. HomeStack has no built-in
resource defaults of its own.

## Discovery

- Manifests are the regular files matching `.homestack/*.yaml` in the root of
  the repository's default branch. Nothing else in the repository is read as a
  deployment instruction. Other files in `.homestack/` are ignored.
- One file holds exactly one service definition (one YAML document). Several
  files mean several independently deployable services.
- Several processes or ports inside one LXC are still one service: list the
  ports in one manifest; do not split them into files.
- Symlinks in `.homestack/` and files larger than 64 KiB are rejected.
- A repository without `.homestack/*.yaml` is not deployable through HomeStack.

## Top-level sections

Every manifest contains exactly these top-level keys, all of them always
written: `schema`, `id`, `name`, `image`, `runtime`, `persistence`,
`deployment` and `exposure`. A missing, misspelled or additional top-level key
makes the whole file invalid: HomeStack lists it as invalid and offers no
service from it.

`persistence` and `exposure` describe features a service may not use. To
switch one off, keep the key and leave it empty:

```yaml
persistence:
exposure:
```

An empty section is a deliberate decision that the feature is off. A missing
section is an error, so a section deleted by accident never silently turns a
feature off.

## Validation inside sections

Inside the sections, an unknown key (e.g. `sizes` instead of `size`) or an
invalid value (e.g. `1GB` instead of `1G`) does not invalidate the file.
HomeStack still reads the manifest and shows the service, marks every such key
and the section containing it as an error, and refuses to deploy the service
until the manifest is fixed. Unknown keys are never ignored silently.

## Fields

| Field | Type | Required | Rule |
| :--- | :--- | :--- | :--- |
| `schema` | integer | yes | exactly `1` |
| `id` | string | yes | `^[a-z0-9][a-z0-9-]{0,62}$`; the stable identity, unique within the repository; independent of the file name |
| `name` | string | yes | display label, 1–64 characters; not an identity |
| `image.repository` | string | yes | OCI repository without tag or digest, e.g. `ghcr.io/owner/name`; lowercase; HomeStack picks the version at deployment time |
| `runtime.resources.cores` | integer | yes | default vCPU count per instance, 1–64 |
| `runtime.resources.memory` | size | yes | default memory limit per instance, at least `64M` |
| `runtime.resources.swap` | size | yes | default swap per instance; `0M` disables swap |
| `runtime.resources.rootfs` | size | yes | default size of the disposable root filesystem, at least `64M` |
| `runtime.ports` | list | no (default empty) | ports inside the container |
| `runtime.ports[].name` | string | yes | `^[a-z][a-z0-9-]{0,31}$`, unique within the service |
| `runtime.ports[].port` | integer | yes | 1–65535, unique within the service |
| `runtime.ports[].protocol` | string | yes | `tcp` |
| `runtime.healthcheck` | mapping | no | omitted: HomeStack reports health as `unverified` |
| `runtime.healthcheck.type` | string | yes | `http` or `tcp` |
| `runtime.healthcheck.port` | string | yes | name of an entry in `runtime.ports` |
| `runtime.healthcheck.path` | string | `http` only | absolute URL path starting with `/` |
| `runtime.healthcheck.expected_status` | integer | `http` only | 100–599 |
| `runtime.healthcheck.heartbeat_seconds` | integer | no (default `60`), `http` only | heartbeat interval, 10–3600; passed to the container as `HOMESTACK_HEARTBEAT_SECONDS`; health fails after three missed beats |
| `persistence` | mapping or empty | yes | empty: no home volume, the service is stateless |
| `persistence.mount` | string | yes | exactly `/home/app` |
| `persistence.size` | size | yes | default size of a new home volume, at least `1M` |
| `persistence.directories` | list of strings | no | relative paths inside the mount, no `..`, no leading `/`, no duplicates |
| `persistence.mappings` | list | no | application paths the image itself redirects into the mount; metadata for review only |
| `persistence.mappings[].source` | string | yes | absolute path inside the container, outside `/home/app` |
| `persistence.mappings[].destination` | string | yes | relative path inside the mount; must be one of `persistence.directories` or below one |
| `deployment.hostname` | string | no (default the `id`) | DNS label `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`; the instances' host name: alone for one instance, `<hostname>-1`, `<hostname>-2`… for several; the domain is chosen at deployment |
| `deployment.instances` | integer | yes | production instances one deployment creates, 1–16 |
| `deployment.placement` | string | no (default `spread`) | `spread` (one instance per node) or `single` (all on one node) |
| `deployment.state_model` | string | yes | `stateless`, `local` or `external` |
| `exposure` | mapping or empty | yes | empty: the service is not exposed through Pangolin and is reachable only inside the LAN, e.g. a management API used by clients on the private network |
| `exposure.pangolin` | mapping | yes, when `exposure` is not empty | Pangolin HTTP resource for the service |
| `exposure.pangolin.port` | string | yes | name of an entry in `runtime.ports` |
| `exposure.pangolin.name` | string | yes | Pangolin resource name, 1–64 characters |
| `exposure.pangolin.subdomain` | string | yes | DNS label `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`; the domain is chosen at deployment |
| `exposure.pangolin.bypass_paths` | list of strings | no | at most 16 unique Pangolin path patterns starting with `/` (e.g. `/mcp/*`) reachable without Pangolin login |

A **size** is a string of a positive decimal integer followed by `M` (MiB) or
`G` (GiB), e.g. `256M`, `2G`. No bare numbers, fractions or other units.

Cross-field rules:

- `state_model: local` requires a non-empty `persistence`: local state
  without a home volume would be lost on every update.
- `state_model: stateless` requires an empty `persistence`.
- `exposure.pangolin` requires an `http` healthcheck on the same port, which
  becomes the Pangolin resource's health check.

For HTTP services the healthcheck path is the readiness endpoint of the health
endpoint standard in [SKILL.md](SKILL.md) (`/health/ready`,
`application/health+json`). HomeStack judges health by the status code; the
body is for people and tools. The endpoint fails when the service's heartbeat
is older than three `heartbeat_seconds` intervals, so a stuck service is
detected by status code alone. `tcp` health means only that the port accepts
connections; HomeStack shows it as such, not as an application-level healthy
result.

## Resources

Every resource value is the single default for one instance, measured on the
built image (see [image-build-preflight.md](image-build-preflight.md)). There
are no minimum/recommended pairs. The operator may override each value per
deployment; a later manifest change does not silently resize running
instances. The home volume size applies to newly created volumes; HomeStack
never shrinks an existing one.

## Instances

One instance is named `<hostname>`, several `<hostname>-1` … `<hostname>-<n>`
(`deployment.hostname`, by default the `id`). The name is the container's host
name and, under the domain chosen at deployment, its record in the local DNS;
clients reach a service by that name, never by its address or CTID. With `placement: spread` each
instance goes to a different node; a cluster with fewer nodes gets one
instance per available node, never two on one node. With `placement: single`
all instances go to the node the operator selects. The operator can add
further, separately named instances (e.g. for testing); they do not count
towards `instances` and get their own empty home volume.

## Exposure

With `exposure.pangolin`, HomeStack creates one Pangolin HTTP resource named
`name` at `<subdomain>.<domain>`, pointing at the declared port of every
instance, with the healthcheck as the resource's health check. The telemetry
path `/.well-known/homestack/status` is never routed publicly. Each
`bypass_paths` pattern becomes a Pangolin `ACCEPT`/`PATH` rule on the
resource's own policy, and rule evaluation is turned on; Publish reviews only
the rules still missing. The service itself must authenticate those paths,
for example with a generated `key` input.

## Image versions

`image.repository` carries no version. HomeStack lists the image's tags,
offers the release tags (`MAJOR.MINOR.PATCH`, no prefix) to the operator, and
records the resolved digest of the chosen one. Other tags (e.g. `dev-<commit>`)
are not releases. Published release tags are immutable. The image must be
pullable anonymously by the Proxmox node (a public package). The Git tag that
produced release `X.Y.Z` is `<id>-X.Y.Z`.

## What never goes in a manifest

Nodes, CTIDs, IP addresses, storage IDs, host paths, the domain, Pangolin
site or organisation IDs, credentials and secrets. These are deployment
inputs or private settings.

## Runtime expectations of the image

- It runs as a Proxmox OCI application container (unprivileged LXC): one
  `linux/amd64` image whose `ENTRYPOINT`/`CMD` starts the service.
- With a non-empty `persistence`: `/home/app` exists in the image as an empty directory.
  HomeStack attaches the home volume there before the first start; the image
  never mounts anything itself. At start the image creates the declared
  `persistence.directories` if they are missing and never resets, overwrites
  or migrates away existing data. Everything that must survive replacing the
  container lives under `/home/app`.
- With an empty `persistence`: the service starts on a fresh root with only the
  image and the settings supplied at deployment.
- The rest of the root filesystem is disposable.
- Logs go to stdout/stderr unless the service has a real need to keep them.

## Examples

A stateless API on every node, used only inside the LAN:

```yaml
schema: 1
id: example-api
name: Example API
image:
  repository: ghcr.io/example/example-api
runtime:
  resources:
    cores: 1
    memory: 128M
    swap: 0M
    rootfs: 128M
  ports:
    - name: http
      port: 8080
      protocol: tcp
  healthcheck:
    type: http
    port: http
    path: /health/ready
    expected_status: 200
persistence:
deployment:
  instances: 3
  placement: spread
  state_model: stateless
exposure:
```

A single instance with a local SQLite database, published through Pangolin:

```yaml
schema: 1
id: example-app
name: Example App
image:
  repository: ghcr.io/example/example-app
runtime:
  resources:
    cores: 1
    memory: 512M
    swap: 0M
    rootfs: 256M
  ports:
    - name: http
      port: 8080
      protocol: tcp
  healthcheck:
    type: http
    port: http
    path: /health/ready
    expected_status: 200
persistence:
  mount: /home/app
  size: 1G
  directories:
    - data
deployment:
  instances: 1
  state_model: local
exposure:
  pangolin:
    port: http
    name: Example App
    subdomain: app
```

The values in these examples are illustrative, not measurements.

## Manifest schema 3

One service consists of exactly these YAML members under one directory:

```
.homestack/services/<id>/service.yaml
.homestack/services/<id>/defaults.yaml
.homestack/services/<id>/inputs.yaml
```

Discovery reads the directory and all blobs from the same immutable Git
commit/tree. Its tree SHA is the combined source revision. Components are
never separate services. Directories and members cannot be symlinks;
members must be regular UTF-8 files, each at most 64 KiB. Additional YAML
members, missing members, conflicting identifiers, invalid types and unknown
fields are refused. IDs must also be unique across legacy schema-1 files.
The existing `.homestack/*.yaml` discovery and schema-1 release policy remain
unchanged.

`service.yaml` requires `schema: 3`, `id` equal to the directory name, `name`,
`image: {repository: REGISTRY/PATH}`, `runtime`, `persistence` and `exposure`.
Runtime supports the same `ports` and `healthcheck` fields as schema 1, plus
optional structured configuration delivery below. Resources belong only in
defaults. Empty persistence/exposure can be `{}` or null. Nonempty sections
follow schema-1 rules. The image owns its command, runtime user and writable
directories; no arbitrary commands, hooks or directory preparation are
accepted from the manifest.

`defaults.yaml` requires `schema: 3`, `service: <id>`, `resources` with the
schema-1 size/core fields, and `deployment` with the schema-1 fields.
`deployment.instance_naming` optionally selects `always-numbered` or
`automatic` (the default). Always-numbered names remain `<hostname>-1` even
for one instance; numbering never incorporates the Proxmox node. Operators
can override the hostname, domain, network, resources, count (1..64) and
selected nodes/storages. Each selected node has one selected storage; new
instances distribute across these selections in order, including several
instances on one node. Placement and instance defaults are suggestions.
Standard deployment fields do not belong in application inputs.

`inputs.yaml` requires `schema: 3`, `service: <id>`, and `inputs`, a list of
at most 64 fields. Each field accepts exactly:

| Field | Meaning |
| --- | --- |
| `id` | Unique application ID: lowercase letter, then lowercase letters, digits, `_` or `-`, maximum 64 characters |
| `label`, `description` | Required nonempty display label; optional printable description |
| `type` | `string`, `integer`, `number`, `boolean`, `url`, `enum` (alias `select`), `key`, or protected-only `secret` (alias `password`) |
| `required` | Boolean, default false |
| `default` | Optional value of the declared type; secret and key defaults are forbidden |
| `min`, `max` | Numeric bounds, for integer/number only |
| `min_length`, `max_length`, `regex` | Text constraints; regex uses Rust regex syntax |
| `choices` | Required nonempty unique string list for enum only |
| `allowed_schemes` | Explicit nonempty subset of `http`, `https` for URL only |
| `target` | One unique destination, described below |

URLs require a host and forbid embedded credentials and fragments. Input
values and defaults are validated on editing, Review and Apply; numeric and
boolean choices stay typed in private storage. Empty optional fields are
omitted; empty defaulted fields select their declared default. Unknown input
IDs or targets, inverted/inapplicable constraints and unresolved required
values are errors. Secret/password delivery is deliberately refused until
a protected runtime secret mechanism exists; it never falls back to plain
PVE environment. Do not disguise credentials as ordinary string inputs.

A `key` input is a random access key the operator never types: it must be
`required`, has no text constraints and targets an environment variable,
whose name may look secret (e.g. `HOMESTACK_ACCESS_KEY`). In the deployment
form Enter generates 64 hex characters; replacing an existing key needs a
second Enter after a warning, because URLs carrying it stop working. It is
stored with the other operator selections and injected on every deployment,
update and container replacement, so it stays stable. It is visible in the
container's PVE environment; use it only for URL-path or bearer keys whose
exposure to Proxmox administrators is acceptable. Services accept 32–256
characters of `A-Z`, `a-z`, `0-9`, `-` and `_`.

Environment delivery is `target: {type: environment, variable: APP_SETTING}`.
Names are unique ASCII environment identifiers. Values are serialized as
plain text for strings/URLs and JSON scalars for numbers/booleans. Control
characters are forbidden, and the final merged environment is limited to
64 KiB. Existing OCI environment entries, including PATH, are preserved.

Optional `runtime.configuration` contains exactly `delivery` and `document`.
Delivery is `{type: environment-json, variable: APP_CONFIG}`. An input may
instead target `{type: config-json-pointer, pointer: /path/to/existing/leaf}`.
RFC 6901 pointers must resolve to existing scalar/null leaves and cannot
collide. Required unresolved null leaves are refused. Resolved documents are
serialized as single-line compact JSON, never multiline YAML. There is no
expression or script evaluation. The derived Collector uses individual
environment inputs and a baked configuration, so it needs neither this
structured delivery option nor runtime entrypoint overrides.

The application section is generated dynamically, after the standard
controls. Actual operator selections are atomically stored in owner-only
JSON files under the active XDG HomeStack profile's `services/` directory
(normally `$XDG_CONFIG_HOME/homestack/services/`). Names are hashes of source
repository, service ID, deployment hostname and domain. Files contain typed
nonsecret values, generated keys and the combined source revision. Directory/file modes are
0700/0600. Missing or invalid configuration for existing instances is an
explicit error: restore the private file before maintenance or update.
Do not commit these files or copy their values into PVE descriptions or logs.

Review shows image and linux/amd64 digests. The existing published numeric
release policy and PVE OCI import are retained. Schema-3 creation uses a
digest-named template cache and rechecks resolution before/after pulling;
replacement retains its existing reviewed digest protections. The derived
image carries its complete OCI command: PVE combines its inherited
ENTRYPOINT with its baked CMD. No shell or runtime command override is added.
Only individual instance FQDNs are managed by DNS integration. An
always-numbered service never claims the owner's shared unnumbered
round-robin record. That record does not imply high availability.

Image replacement keeps the CTID, instance identity and selected environment.
Review explicitly warns that rootfs-local data (such as an exporter queue)
is discarded; normal CT restart keeps the same rootfs. Reconcile changed
application values before changing the image. The example under
`.homestack/services/otel/` and `deploy/otel/README.md` describe the concrete
Collector runtime; `docs/e2e-schema3.md` records the isolated-lab scenario.
