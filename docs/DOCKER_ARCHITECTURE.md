# Docker Architecture (reverse-engineered from pinned sources)

This document describes the Docker ecosystem as it exists in the pinned
upstream trees under `upstream/`, and where each `docker-rs` crate replaces a
piece of it. All paths below were verified against the pinned commits listed in
the root `README.md`.

## The upstream stack, end to end

```
 docker CLI          dockerd (Engine daemon)              containerd              kernel
+-------------+     +----------------------+     +-------------------+  +-------------+
| cli/        | --> | moby/api/server      | --> | containerd client |->| namespaces, |
| cmd/docker/ |HTTP | moby/api/types       |gRPC | core/runtime      |  | cgroups,    |
|             |     | moby/daemon          |gRPC | plugins/{...}     |  | seccomp ... |
+-------------+     +----------+-----------+     +----+---------+----+  +-------------+
                               |    buildkit         |         |
                               |   (builder)         |         +-- runc (libcontainer):
                               v                     v             OCI runtime implementation
                       registry (distribution      content store           (OCI bundles, rootfs)
                       protocol: OCI dist spec)    snapshotter             child of containerd or
                                                                          docker-rs runtime crate
```

Component responsibilities, as seen from the sources:

1. **CLI** (`upstream/cli/cmd/docker/docker.go` and siblings
   `builder.go`, `completions.go`, `aliases.go`, `cloud.go`) — command
   parsing, cloud auth helpers, shell completion. Talks to the Engine over
   HTTP (and to builds via buildkit over grpc).
2. **Engine HTTP server** — `upstream/moby/api/server/server.go` enforces a
   minimum API version of `1.24` (`versions.LessThan(v, "1.24")` at startup),
   and `upstream/moby/api/server/middleware/version.go` stamps the
   `Api-Version` response header from the default API version. Version
   constants live in `upstream/moby/api/common.go`:
   `DefaultVersion = "1.51"`, `MinSupportedAPIVersion = "1.24"`.
3. **Engine daemon** — `upstream/moby/daemon/` orchestrates containers:
   - `daemon/container.go`, `daemon/container_operations.go` — per-container
     orchestration.
   - `daemon/names.go` — name format validation for containers/volumes/etc.
   - `daemon/events/events.go` — the event broker that customers subscribe to
     (`Events: type actor filters` API).
   - `daemon/logger/jsonfilelog/jsonfilelog.go` (+ `read.go`) — the default
     `json-file` log driver's wire format: each log line is a JSON object with
     (`log`, `stream`, `time`) — see `docker-logging`.
4. **Container model** — `upstream/moby/container/` holds the container
   object itself, independent of the daemon:
   - `container/state.go` — the lifecycle state machine (`State` with
     running/paused/restarting/… transitions).
   - `container/memory_store.go`, `container/store.go` — in-memory container
     index (by id, by name, prefix resolution).
   - `container/monitor.go` — supervises a child process and drives state
     transitions; `container/health.go` — exec-based health checks.
5. **containerd** (v2.4.2 layout) — daemon-side plugins under
   `upstream/containerd/plugins/` (content, snapshots, metadata, events,
   server, sandbox, streaming, transfer, nri, …) and the foundational
   packages under `upstream/containerd/core/` (containers, content, images,
   leases, runtime, remotes, …). The daemon talks to containerd services
   (gRPC); containerd in turn spawns `runc` (or another OCI runtime) via
   `core/runtime`.
6. **runc** — `upstream/runc/libcontainer/container.go` is the OCI container
   abstraction; it consumes OCI bundles produced to `runtime-spec`
   (`upstream/runtime-spec/spec.md`, `config.md`; schemas in
   `runtime-spec/schema/config-schema.json`) and produces the state document
   (`runtime-spec/specs-go/state.go`).
7. **Builder** — buildkit: the Dockerfile frontend under
   `upstream/buildkit/frontend/dockerfile/frontend/` and the Dockerfile→LLB
   conversion in `upstream/buildkit/frontend/dockerfile/dockerfile2llb/`
   (`convert.go` and the per-instruction `convert_{runmount,secrets,ssh,
   runnetwork,runsecurity,rundevice,expose,copy}.go` files), with source
   metadata in `image.go` and target-platform resolution in `platform.go`.
8. **Registry protocol** — the OCI distribution spec
   (`upstream/distribution-spec/spec.md`, `content-negotiation.md`)
   defines push/pull endpoints; the daemon's distribution client speaks it.

## docker-rs mirrors of these layers (Milestones A–L)

| docker-rs layer            | crate(s)                                | replaces (upstream)                                  |
|----------------------------|-----------------------------------------|------------------------------------------------------|
| Error taxonomy             | `docker-errors`                         | `moby/errdefs/` (defs, helpers, http_helpers)        |
| Primitives (ids, refs)     | `docker-common`                         | `moby/container/state.go` id rules, reference grammar; `image-spec/specs-go/v1/descriptor.go` digest model |
| Daemon config              | `docker-configuration`                  | daemon `config.go` JSON/env layering (`daemon.json`)  |
| Logging (default driver)   | `docker-logging`                        | `moby/daemon/logger/jsonfilelog/`                    |
| Event broker               | `docker-events`                         | `moby/daemon/events/events.go`                       |
| Container model/store      | `docker-container-model`                | `moby/container/{state,memory_store,container}.go`   |
| Image/content store        | `docker-image`, `docker-storage`         | image-spec v1.1.1 types; containerd `core/content`-style CAS        |
| (next) Exec/runtime        | planned `docker-runtime`                | runc `libcontainer` (OCI runtime)                    |
| (next) HTTP API            | planned `docker-api`                    | `moby/api/server`, `api/common.go` version gates     |
| (next) Builder             | planned `docker-buildkit`               | buildkit `frontend/dockerfile/dockerfile2llb`        |
| Registry client            | `docker-distribution`                   | distribution-spec v1.1.1 client (pull/push/auth)     |
| (next) CLI                 | planned `dockerrs` binary               | `cli/cmd/docker/`                                    |

## Design principles carried over from upstream

- **Errors are messages returned verbatim**; classification (NotFound,
  Conflict, InvalidArgument, …) is metadata added on top — never re-written
  by a server layer. `moby/errdefs/` exists exactly to keep that separation;
  `docker-errors` mirrors it with an HTTP status mapping.
- **State transitions are validated once, centrally** — upstream funnels
  everything through `container/state.go`-owned transitions; we do the same
  in `docker-container-model::state`.
- **Events are best-effort for slow consumers.** Upstream closes/resets
  streams that cannot keep up; our bounded broker drops the oldest event and
  reports it in the stream (documented divergence, chosen for drop-free
  synchronization-free drops).
- **Version gating is integral**: like `moby/api/server`, unsupported client
  API versions get a structured `versionUnsupported` error, not silent
  degradation.
