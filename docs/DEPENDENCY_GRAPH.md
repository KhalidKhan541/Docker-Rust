# Dependency Graph

## Rust workspace (current)

Milestones A–C; the graph is acyclic and leaves target-specific code
(`getrandom`, `std::net`, `std::process`) in `docker-common` and
`docker-distribution`.

```
docker-errors  (workspace root)
docker-common               → docker-errors
docker-configuration        → docker-common, docker-errors
docker-logging              → docker-common, docker-errors
docker-events               → docker-common, docker-errors
docker-container-model      → docker-common, docker-errors
docker-image                → docker-common, docker-errors
docker-storage              → docker-common, docker-errors
docker-distribution         → docker-common, docker-errors, docker-image,
                              docker-storage
tests/integration           → all nine crates
```

Crate-level rules enforced by review:

1. `docker-errors` depends on no workspace crate (the only true root).
2. `docker-common` depends only on `docker-errors`.
3. Middle-layer crates depend on both roots. `docker-distribution` is the
   only crate that composes other middle-layer crates (`docker-image` wire
   types, `docker-storage` blob streaming); this introduces no cycle.
4. Integration tests may depend on everything.
5. No `default` features-on features, no workspace crate depends on the
   integration crate.

External dependencies (pinned in root `Cargo.toml`): `thiserror 2`, serde +
serde_json (features `derive`), chrono (RFC 3339 image-config and logging
timestamps), tracing + tracing-subscriber (feature `local-time`),
`getrandom 0.2` (IDs), `base64 0.22` (descriptor `data` codec mirroring Go
`encoding/json`), `sha2 0.10` (content addressing), `native-tls 0.2`
(SChannel on Windows, OpenSSL on Linux).

## Upstream (Go) dependency shape — abridged, verified layouts

```
moby/api, moby/daemon ──> moby/errdefs            (error classification)
moby/daemon ──> moby/container                    (container object)
moby/container ──> moby/errdefs
moby/daemon ──> containerd/client (vendored)      (gRPC to containerd)
containerd v2.4.2:
    plugins/*  (content, snapshots, metadata, events, server, sandbox,
                streaming, transfer, nri, restart, leases, gc, ...)
      └──> core/* (containers, content, images, leases, runtime,
                   metadata, remotes, sandbox, snapshot, streaming, events)
           └──> api/* (protobuf/grpc definitions)
    core/runtime ──> spawns runc
runc v1.5.2:
    libcontainer ──> runtime-spec/specs-go         (OCI config/state types)
image-spec v1.1.1:
    specs-go/v1 (descriptor, config, manifest, index, layout, mediatype)
distribution-spec v1.1.1: specification only; docker-distribution implements
    the client on top of the image-spec types and the blob store
buildkit:
    frontend/dockerfile/frontend  ──> frontend/dockerfile/dockerfile2llb
      └──> llb (DAG) ──> solver ──> containerd content/snapshots
cli v29.9.1:
    cmd/docker ──> client (Engine HTTP client)
```

Notable upstream vendor chains that inform our Rust choices:

- `moby` vendors `github.com/containerd/errdefs` (`upstream/moby/vendor/
  github.com/containerd/errdefs/errors.go`) — the same classification family
  we cloned into `docker-errors`.
- buildkit solver errors (`vendor/github.com/moby/buildkit/solver/errdefs/`)
  reuse the same "wrap + classify" pattern.

## What depends on what, desired end-state

By Milestone L every user-facing binary (daemon, CLI, builder, registry
client) must be reachable from `docker-rs` root with no circular
dependencies, and every crate must be independently `cargo test`-able.
