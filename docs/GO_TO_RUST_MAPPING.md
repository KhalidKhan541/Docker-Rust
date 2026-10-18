# Go → Rust Mapping

Concrete file-level correspondences between pinned upstream sources and the
Rust implementation. Paths verified against the pinned commits in the root
`README.md`.

## Milestone A (implemented)

| Upstream (Go)                                            | Rust                                        | Notes |
|----------------------------------------------------------|---------------------------------------------|-------|
| `moby/errdefs/defs.go`, `helpers.go`, `http_helpers.go`, `is.go` | `crates/errors/src/lib.rs`          | Same classification set; HTTP mapping via `http_status()`; 409 for `Conflict`/`Busy`; 499 only for internal `Cancelled`. |
| `moby/container/state.go` (state descriptors/transitions) | `crates/container-model/src/state.rs`     | Transition table port; `Restarting` requires policy. |
| `moby/container/memory_store.go` (id/name/prefix lookup)  | `crates/container-model/src/store.rs`     | `resolve()` mirrors short-id/name resolution; ambiguous → `InvalidArgument` with match count, exact-id wins. |
| `moby/container/container.go` + `daemon/container.go`     | `crates/container-model/src/config.rs`    | ContainerConfig/HostConfig validation side. |
| `moby/daemon/names.go` (name format)                      | `crates/common/src/names.rs`              | Same charset; 255-char cap documented (upstream has none) in RUST_DESIGN.md. |
| `image-spec/specs-go/v1/descriptor.go` (digest model)     | `crates/common/src/digest.rs`             | Digest parse/normalize; custom serde → `algo:hex` string. |
| reference grammar (`moby` dist client `reference` pkg vendored under `moby/vendor/github.com/distribution/reference/`) | `crates/common/src/reference.rs` | Registry detection (dot/port/localhost), uppercase host allowed / path rejected, tag + digest. |
| daemon `config.go` (daemon.json + env layering)           | `crates/configuration/src/lib.rs`         | `FileConfig` kebab-case keys, `deny_unknown_fields` (mirrors upstream's unknown-option rejection), `DRC_*` env layering. |
| `moby/daemon/logger/jsonfilelog/jsonfilelog.go`           | `crates/logging/src/lib.rs`               | `{"log","stream","time"}` wire shape; `JsonLinesDriver`. |
| `moby/daemon/events/events.go`                            | `crates/events/src/lib.rs`                | Event wire casing `type/action/scope/time/timeNano`, uppercase `ID`; bounded broker (drop-oldest) vs upstream stream closure. |

## Planned mapping (later milestones; verified source anchors)

| Upstream (Go) | To be replaced by | Milestone |
|---------------|-------------------|-----------|
| `moby/daemon/container.go`, `daemon/container_operations.go` | `docker-containerd` orchestration layer | D |
| `containerd/core/runtime` + `plugins/services` | gRPC service clients / server shims | D |
| `runc/libcontainer/container.go` | `docker-runtime` (own OCI implementation; bundles of `runtime-spec/specs-go`) | E |
| `runtime-spec/schema/*.json` | bundle validation tests | E |
| `image-spec/specs-go/v1/*.go` | `docker-image` types + content-addressable store | B |
| `distribution-spec/spec.md` endpoints | `docker-distribution` (push/pull client) | C |
| `moby/api/common.go`, `api/server/server.go`, `api/server/middleware/version.go` | `docker-api` HTTP routing + version middleware | F/G |
| `buildkit/frontend/dockerfile/dockerfile2llb/*.go` | `docker-buildkit` frontend (LLB-equivalent DAG in Rust) | H/I |
| `cli/cmd/docker/docker.go` and subcommands | `dockerrs` CLI | J |
| `moby/daemon/rootless*` / security paths | `docker-security` | K |

## Translation conventions

1. Go interfaces that exist purely for swapping implementations at runtime
   (e.g. log drivers) become Rust traits with the same method names.
2. Go "option structs" become plain Rust structs with `Default`; the deny-
   unknown-fields discipline replaces reflection-based validators.
3. Go's stringly-typed enums (states, levels, policies) become non_exhaustive
   enums with explicit `Unsupported`-grade errors on unknown variants.
4. No panics across crate boundaries; constructors return `Result` where the
   Go code panics on programmer error.
