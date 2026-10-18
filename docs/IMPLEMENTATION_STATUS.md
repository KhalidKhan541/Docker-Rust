# Implementation Status

Snapshot: Milestone A complete and committed. Honest state of every layer.

## Milestone status

| # | Milestone | Status | Evidence |
|---|-----------|--------|----------|
| A | Foundation (errors, primitives, config, logging, events, container model) | **done** | 83 tests green; fmt/clippy clean; crates listed below |
| B | Image & content addressable store | not started | planned: `docker-image`, `docker-storage`; sources verified (`image-spec/specs-go/v1/`) |
| C | Registry (distribution client) | not started | distribution-spec v1.1.1 text in `upstream/distribution-spec/spec.md` |
| D | Daemon core (containerd-equivalent orchestration) | not started | moby daemon/container.go paths mapped in GO_TO_RUST_MAPPING.md |
| E | OCI runtime (container execution) | not started | runc v1.5.2 libcontainer surface mapped; runtime-spec schemas present and pinned |
| F/G | Engine HTTP API | not started | API_COMPATIBILITY.md plan; constants verified from `moby/api/common.go` |
| H/I | Builder (buildkit-equivalent) | not started | dockerfile2llb module verified in pinned tree |
| J | CLI | not started | `upstream/cli/cmd/docker/` verified |
| K | Security hardening | not started | SECURITY_ANALYSIS.md review hooks defined |
| L | Conformance + differential testing | skeleton only | TESTING.md; rea MCP connected and callable (verified via live tool round-trip) |

## Milestone A — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-errors` | `Error`/`ErrorKind` taxonomy cloned from `moby/errdefs` (defs, helpers, http_helpers, is), HTTP status mapping incl. 409 for Conflict/Busy, internal-only 499 for Cancelled, `serde_json::Error` bridging |
| `docker-common` | rng/hex 64-char ids; `Digest` (sha256-only + `Unsupported` on others, custom serde); `Name` (255 cap documented); `ImageRef` (registry detection grammar, digests, tags); `SafePath` (traversal/drive-letter/NUL rejection); `Platform` + capability probes with honest `Unsupported`; `ChildProcess`/`Signal` Windows-gnu scaffolding |
| `docker-configuration` | `DaemonConfig` + `FileConfig` (kebab-case daemon.json, deny_unknown_fields) + `DRC_*` env overlay; validation (log levels, data roots, api host, storage driver) |
| `docker-logging` | `DaemonLevel` + tracing init; `JsonLinesDriver` with upstream json-file record shape and rotate ceilings |
| `docker-events` | `Event`/`Actor` upstream wire casing; filter model; bounded `EventBroker` (drop-oldest, drop counter surfaced) |
| `docker-container-model` | `LifecycleState` + transition table (upstream `container/state.go` shape); `ContainerConfig`/`HostConfig` validation suite; `ContainerStore` with id/name/prefix resolution and ambiguity errors |
| `tests/integration` | cross-crate vertical slice |

## Toolchain (environment)

- stable-x86_64-pc-windows-gnu; `tools/env.ps1` PATH preloader mandatory.
- MSYS2 binutils 2.47 + runtime DLLs in `tools/mingw64` (gitignored) plus
  sysroot `self-contained` shim (as.exe copies) — fixed and reproducible.

## Reverse-engineering tooling

- All 8 upstream repos pinned at listed revisions under `upstream/`.
- rea MCP server: **connected and verified callable** (startup-timeout fix
  applied to `~/.config/opencode/opencode.json`, rea now answers tool
  calls; reserved for compiled-artifact RE in milestone L, naked for text
  phases where the Go sources answer directly).

## Honest gaps

- No HTTP server, no execution, no images, no registry, no build yet —
  Milestones B–L are untouched source work beyond the A-layer.
- Windows host has no container backend for differential-at-runtime tests
  (see TESTING.md constraints).
- Store is not yet thread-safe (planned synchronous model in D).
