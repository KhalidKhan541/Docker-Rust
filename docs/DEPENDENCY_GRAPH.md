# Dependency Graph

## Rust workspace (current)

Milestones A–B depend only on the crates below; the graph is acyclic and
leaves target-specific code (`getrandom`, `std::process`) in `docker-common`.

```
docker-common  ←+← docker-configuration
                |← docker-logging
                |← docker-events
                |← docker-container-model
                |← docker-image                          (Digest in config diff_ids/descriptors)
                |← docker-storage                        (Digest + rng for blob addressing)
docker-errors  ←+← docker-configuration, docker-logging, docker-events,
                    docker-container-model, docker-image, docker-storage
                    (all middle-layer crates depend on both roots)
tests/integration (docker-rs-integration) → uses all eight crates
```

Crate-level rules enforced by review:

1. `docker-errors` and `docker-common` never depend on each other or on
   anything in the workspace (roots).
2. Middle-layer crates may depend on both roots and nothing else.
3. Integration tests may depend on everything.
4. No `default` features-on features, no workspace crate depends on the
   integration crate.

External dependencies (pinned in root `Cargo.toml`): `thiserror 2`, serde +
serde_json (features `derive`), chrono (RFC 3339 image-config and logging
timestamps), tracing + tracing-subscriber (feature `local-time`),
`getrandom 0.2` (IDs), `base64 0.22` (descriptor `data` codec mirroring Go
`encoding/json`), `sha2 0.10` (content addressing).

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
distribution-spec v1.1.1: specification only (spec.md, content-negotiation.md)
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
