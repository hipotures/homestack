# HomeStack service manifest, schema 1

The exact contract between an application repository and HomeStack. HomeStack
reads it; the application repository only declares portable facts about its
image and runtime.

## Discovery

- Manifests are the regular files matching `.homestack/*.yaml` in the root of
  the repository's default branch. Nothing else in the repository is read as a
  deployment instruction. Other files in `.homestack/` are ignored.
- One file holds exactly one service definition (one YAML document). Several
  files mean several independently deployable services, each its own LXC.
- Several processes or ports inside one LXC are still one service: list the
  ports in one manifest; do not split them into files.
- Symlinks in `.homestack/` and files larger than 64 KiB are rejected.
- A repository without `.homestack/*.yaml` is not deployable through HomeStack.

## Fields

Unknown fields are rejected, so a typo does not silently change meaning.

| Field | Type | Required | Rule |
| :--- | :--- | :--- | :--- |
| `schema` | integer | yes | exactly `1` |
| `id` | string | yes | `^[a-z0-9][a-z0-9-]{0,62}$`; the stable identity, unique within the repository; independent of the file name |
| `name` | string | yes | display label, 1–64 characters; not an identity |
| `image.repository` | string | yes | OCI repository without tag or digest, e.g. `ghcr.io/owner/name`; lowercase; HomeStack picks the version at deployment time |
| `runtime.ports` | list | no (default empty) | ports inside the container |
| `runtime.ports[].name` | string | yes | `^[a-z][a-z0-9-]{0,31}$`, unique within the service |
| `runtime.ports[].port` | integer | yes | 1–65535, unique within the service |
| `runtime.ports[].protocol` | string | yes | `tcp` |
| `runtime.healthcheck` | mapping | no | omitted: HomeStack reports health as `unverified` |
| `runtime.healthcheck.type` | string | yes | `http` or `tcp` |
| `runtime.healthcheck.port` | string | yes | name of an entry in `runtime.ports` |
| `runtime.healthcheck.path` | string | `http` only | absolute URL path starting with `/` |
| `runtime.healthcheck.expected_status` | integer | `http` only | 100–599 |
| `persistence.mount` | string | yes | exactly `/home/app` |
| `persistence.directories` | list of strings | no | relative paths inside the mount, no `..`, no leading `/`, no duplicates |
| `persistence.mappings` | list | no | application paths the image itself redirects into the mount; metadata for review only |
| `persistence.mappings[].source` | string | yes | absolute path inside the container, outside `/home/app` |
| `persistence.mappings[].destination` | string | yes | relative path inside the mount; must be one of `persistence.directories` or below one |
| `deployment.multi_node` | boolean | no (default `false`) | `true` only with `state_model` `stateless` or `external` |
| `deployment.state_model` | string | yes | `local`, `stateless` or `external` |

For HTTP services the path is the readiness endpoint of the health endpoint
standard in [SKILL.md](SKILL.md) (`/health/ready`, `application/health+json`).
HomeStack judges health by the status code; the body is for people and tools.

`tcp` health means only that the port accepts connections; HomeStack must show
it as such, not as an application-level healthy result.

## Image versions

`image.repository` carries no version. HomeStack lists the image's tags,
offers the release tags (`MAJOR.MINOR.PATCH`, no prefix) to the operator, and
records the resolved digest of the chosen one. Other tags (e.g. `dev-<commit>`)
are not releases. Published release tags are immutable. The image must be
pullable anonymously by the Proxmox node (a public package). The Git tag that
produced release `X.Y.Z` is `<id>-X.Y.Z`.

## What never goes in a manifest

Nodes, CTIDs, IP addresses, storage IDs, host paths, volume sizes, CPU or
memory, domain names, Pangolin resources, credentials and secrets. These are
HomeStack deployment inputs or private settings.

## Runtime expectations of the image

- It runs as a Proxmox OCI application container (unprivileged LXC): one
  `linux/amd64` image whose `ENTRYPOINT`/`CMD` starts the service.
- `/home/app` exists in the image as an empty directory. HomeStack attaches
  a persistent volume there before the first start; the image never mounts
  anything itself.
- At start the image creates the declared `persistence.directories` if they
  are missing and never resets, overwrites or migrates away existing data.
- Everything that must survive replacing the container lives under
  `/home/app`; the rest of the root filesystem is disposable.
- Logs go to stdout/stderr unless the service has a real need to keep them.
