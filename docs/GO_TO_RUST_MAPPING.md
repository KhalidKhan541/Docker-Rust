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

## Milestone B (implemented)

| Upstream (Go)                                            | Rust                                     | Notes |
|----------------------------------------------------------|------------------------------------------|-------|
| `image-spec/specs-go/v1/mediatype.go`                    | `crates/image/src/mediatype.rs`          | Constant values verbatim; non-distributable types kept for round-tripping only. |
| `image-spec/specs-go/v1/descriptor.go`                   | `crates/image/src/descriptor.rs`         | `Descriptor`/`Platform` JSON tags verbatim; `data` base64 = Go `encoding/json` `[]byte` codec; `DescriptorEmptyJSON` ported (constants + `{}` blob). |
| `image-spec/specs-go/v1/manifest.go` + `specs-go/versioned.go` | `crates/image/src/manifest.rs`      | schemaVersion 2 embedded; `validate()` conformance suite. |
| `image-spec/specs-go/v1/index.go` + `specs-go/versioned.go` | `crates/image/src/index.rs`           | Same shape as manifest with the `manifests` list. |
| `image-spec/specs-go/v1/config.go`                       | `crates/image/src/config.rs`             | PascalCase execution parameters; `map[string]struct{}` → `EmptyObject` with `{}` wire values; Go embedding → top-level `architecture`/`os`; `RootFS`/`History`. |
| `image-spec/specs-go/v1/layout.go`                       | `crates/storage/src/lib.rs`              | `ImageLayoutFile`/`ImageBlobsDir`/`ImageLayoutVersion` constants; `BlobStore` CAS is literally an OCI blobs directory; temp+rename atomic writes. |

## Milestone C (implemented)

| Upstream (Go/spec)                                       | Rust                                     | Notes |
|----------------------------------------------------------|------------------------------------------|-------|
| `distribution-spec/spec.md` §Authentication               | `crates/distribution/src/auth.rs`, `client.rs` | RFC 7235 `WWW-Authenticate` parser (quoted commas, multiple challenges); bearer token endpoint with `service`/`scope`, per-scope cache with expiry margin; Basic registries and Basic credentials on the token endpoint; final 401 → `Unauthorized`. |
| `distribution-spec/spec.md` blob upload/pull endpoints    | `crates/distribution/src/client.rs` (`push_blob_from_store`, `push_blob_chunked`, `pull_blob_to_store`, `head_blob`) | Monolithic `POST`→`PUT` and chunked `PATCH`; `Content-Range` `start-end` format; `416` → status `GET` + resume with `Location` rotation; `OCI-Chunk-Min-Length`; digest-verified streaming through `BlobStore`; second push skipped by `HEAD`. |
| `distribution-spec/spec.md` manifest endpoints            | `client.rs` (`get_manifest`, `put_manifest`, `parse_manifest_document`) | Content-negotiated `Accept` list; body digest and `Docker-Content-Digest` verification; schema 1 → `Unsupported`; `application/json` sniffing per the negotiation table. |
| `distribution-spec/spec.md` tag listing                   | `client.rs` (`list_tags`, `list_all_tags`, `parse_link_next`) | `Link: rel="next"` pagination, URL resolved against the registry origin, 100-page cap + repeat detection. |
| `distribution-spec/spec.md` referrers                     | `client.rs` (`referrers`)                | API query with `artifactType`; `404` → `<algo>-<hex>` tag-schema fallback; client-side filtering unless `OCI-Filters-Applied` says the registry did it. |
| `distribution-spec/spec.md` errors                        | `client.rs` (`status_error`, `registry_error_detail`) | HTTP status → `ErrorKind` (400/413/416 `InvalidArgument`, 401, 403, 404, 405 `Unsupported`, 409, 429 `Throttle`, 5xx `Unavailable`); registry `{"errors":[{code,message}]}` appended to messages. |
| `distribution-spec/content-negotiation.md`                | `client.rs` (`DEFAULT_ACCEPT`)           | Preference order: OCI index, OCI manifest, Docker manifest list, Docker schema 2; OCI types preferred when serving. |
| distribution transport rules in `spec.md`                 | `crates/distribution/src/http.rs`, `url.rs` | HTTP/1.1 over `TcpStream`; TLS via `native-tls`; one request per connection; ≤5 redirects; auth/cookie stripped cross-origin; HTTPS→HTTP rejected; size ceilings and header-injection guards. |

## Milestone D (implemented)

| Upstream (Go)                                            | Rust                                     | Notes |
|----------------------------------------------------------|------------------------------------------|-------|
| `moby/daemon/monitor.go` (`handleContainerExit`, `ProcessEvent`) | `crates/daemon/src/engine.rs` (`task_exited`, monitor threads) | One monitor thread per started task blocks in `RuntimeTask::wait`, deletes the task, then posts `TaskExited`. The actor computes `execDuration`, applies the restart decision, emits `die`, and sets `restarting` or `exited`. |
| `moby/restartmanager/restartmanager.go`                  | `crates/daemon/src/restart.rs`            | Policy semantics, 100 ms initial delay, ×2 backoff, 60 s cap, reset at ≥10 s, restart-count cap for `on-failure`, cancellation, and single-active-evaluation guard. |
| `moby/daemon/start.go` (`ContainerStart`, `containerStart`, `validateState`) | `engine.rs` (`start`, `start_task`) | Running/restarting → no-op (Moby keeps `Running` true during backoff and returns 304); paused → Conflict (unpause required); created/exited → create task, start it, spawn monitor, set `running`, emit `start`. Manual starts reset the manager and count. |
| `moby/daemon/stop.go` (`ContainerStop`, `containerStop`) | `engine.rs` (`stop`, `begin_stop`, `send_stop_signal`, `handle_stop_timer`) | Sends `Config.StopSignal`, emits `kill`, waits for `TaskExited`, escalates to SIGKILL after the grace period, and emits `die` → `stop` before resolving the caller. A stop received during restart backoff cancels the timer and settles `exited`. |
| `moby/daemon/kill.go` (`killWithSignal`, `Kill`)          | `engine.rs` (`kill`)                      | Emits `kill` with the numeric signal. Stop-signal/SIGKILL cancels an `always` restart; arbitrary signals set the manual-stop flag but may still restart under `always`, matching `ShouldRestart`. |
| `moby/daemon/delete.go` (`ContainerRm`, force path)      | `engine.rs` (`remove`, `remove_record`)   | Non-force removal rejects running, paused, and restarting containers; force uses a 3 s stop timeout for live tasks, then `die`/`stop`/`destroy`; force-removing a restarting container cancels the pending timer and destroys it. |
| `moby/daemon/events.go` (`LogContainerEventWithAttributes`) | `engine.rs` (`emit_container`, `container_actor`) | Labels are copied first, then `image` and `name` override; `die` adds `exitCode` and `execDuration`; actor id is the container id. |
| `moby/container/container.go` (`ResetRestartManager`, `SetRunning`, `SetRestarting`, `SetStopped`) | `engine.rs` (`start_task`, `set_running`, `set_exit_state`, `set_dead`) | State writes go through the `docker-container-model` transition validator; manual start resets the restart count. |
| containerd Task client (used by upstream `libcontainerd`) | `crates/daemon/src/runtime.rs` (`Runtime`, `RuntimeTask`) | Synchronous Rust seam equivalent to create/start/wait/kill/delete. Milestone E supplies the OCI implementation; D ships `SyntheticRuntime`. |

Concurrency translation: upstream uses container mutexes plus goroutines
(monitor, restart-manager timeout, API handlers). Docker-rs instead uses one
actor thread for all state and per-task monitor threads only for `wait`.
Deferred replies replace condition-variable waiters, so no caller observes a
half-applied transition. This is a deliberate structural divergence; event
order and policy semantics follow the Go sources.

## Planned mapping (later milestones; verified source anchors)

| Upstream (Go) | To be replaced by | Milestone |
|---------------|-------------------|-----------|
| `moby/daemon/container.go`, `daemon/container_operations.go` (networking/storage setup) | `docker-daemon` extensions | F/G |
| `containerd/core/runtime` + `plugins/services` | gRPC service clients / server shims | E |
| `runc/libcontainer/container.go` | `docker-runtime` (own OCI implementation; bundles of `runtime-spec/specs-go`) | E |
| `runtime-spec/schema/*.json` | bundle validation tests | E |
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
