# HomeStack service manifest, schema 1

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
| `persistence` | mapping or empty | yes | empty: no home volume, the service is stateless |
| `persistence.mount` | string | yes | exactly `/home/app` |
| `persistence.size` | size | yes | default size of a new home volume, at least `1M` |
| `persistence.directories` | list of strings | no | relative paths inside the mount, no `..`, no leading `/`, no duplicates |
| `persistence.mappings` | list | no | application paths the image itself redirects into the mount; metadata for review only |
| `persistence.mappings[].source` | string | yes | absolute path inside the container, outside `/home/app` |
| `persistence.mappings[].destination` | string | yes | relative path inside the mount; must be one of `persistence.directories` or below one |
| `deployment.instances` | integer | yes | production instances one deployment creates, 1–16 |
| `deployment.placement` | string | no (default `spread`) | `spread` (one instance per node) or `single` (all on one node) |
| `deployment.state_model` | string | yes | `stateless`, `local` or `external` |
| `exposure` | mapping or empty | yes | empty: the service is not exposed through Pangolin and is reachable only inside the LAN, e.g. a management API used by clients on the private network |
| `exposure.pangolin` | mapping | yes, when `exposure` is not empty | Pangolin HTTP resource for the service |
| `exposure.pangolin.port` | string | yes | name of an entry in `runtime.ports` |
| `exposure.pangolin.name` | string | yes | Pangolin resource name, 1–64 characters |
| `exposure.pangolin.subdomain` | string | yes | DNS label `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`; the domain is chosen at deployment |

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
body is for people and tools. `tcp` health means only that the port accepts
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

Hostnames are `<id>-1` … `<id>-<instances>`. With `placement: spread` each
instance goes to a different node; a cluster with fewer nodes gets one
instance per available node, never two on one node. With `placement: single`
all instances go to the node the operator selects. The operator can add
further, separately named instances (e.g. for testing); they do not count
towards `instances` and get their own empty home volume.

## Exposure

With `exposure.pangolin`, HomeStack creates one Pangolin HTTP resource named
`name` at `<subdomain>.<domain>`, pointing at the declared port of every
instance, with the healthcheck as the resource's health check. The telemetry
path `/.well-known/homestack/status` is never routed publicly.

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
