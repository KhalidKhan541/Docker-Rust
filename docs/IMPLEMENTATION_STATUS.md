# Implementation Status

Snapshot: Milestones A–D complete. Honest state of every layer.

## Milestone status

| # | Milestone | Status | Evidence |
|---|-----------|--------|----------|
| A | Foundation (errors, primitives, config, logging, events, container model) | **done** | 83 tests green; fmt/clippy clean; crates listed below |
| B | Image & content addressable store | **done** | 114 tests green (31 new); fmt/clippy `-D warnings` clean; crates listed below |
| C | Registry (distribution client) | **done** | 168 tests green (42 distribution unit + 8 registry integration added); fmt/clippy `-D warnings` clean |
| D | Daemon core (containerd-equivalent orchestration) | **done** | 191 tests green (9 daemon unit + 14 daemon integration added); fmt/clippy `-D warnings` clean; actor + synthetic runtime |
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

## Milestone B — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-image` | `mediatype.rs`: upstream media-type constant set. `descriptor.rs`: `Descriptor` (`mediaType`/`digest`/`size` required; `urls`/`annotations`/`platform`/`artifactType` skip-if-empty; `data` base64 via a Go `encoding/json` `[]byte` codec emulation) + `Platform` (`os.version`/`os.features` renames) + `Descriptor::empty_json()` (upstream `DescriptorEmptyJSON`: `{}` blob, digest `sha256:44136fa3…caaff8a`, size 2). `manifest.rs`/`index.rs`: camelCase structs with embedded `specs.Versioned` schemaVersion + `validate()` conformance checks (schema 2, config media type pinned to the image config type, indexed per-entry error messages). `config.rs`: `ImageConfig` PascalCase keys incl. `ExposedPorts`/`Volumes` as `map<String, EmptyObject>` with wire `{}` values, `ArgsEscaped` skip-if-false, `RootFS`, `History`, `Image` with top-level `architecture`/`os` (Go embedding shape) and always-serialized `config` key |
| `docker-storage` | `BlobStore` CAS at `<root>/sha256/<hex>` (absolute-root check; temp-file+rename writes; `put_verified` rejects digest mismatch before touching disk; duplicate puts dedupe with cross-process corruption detection; `get`/`get_verified`/`stat`/`exists`/`delete`; strict `list` tolerating only dot-prefixed temp leftovers); `init_image_layout`/`check_image_layout` writing/validating the exact `oci-layout` marker bytes `{"imageLayoutVersion":"1.0.0"}` with version mismatch → `Unsupported` |
| `tests/integration` | `image_content_vertical_slice.rs`: config compose → rejected wrong-digest put (nothing written) → verified CAS put/get/stat → manifest composition → `init_image_layout` export with `index.json` → reload and re-verify all blobs |

Wire shapes are pinned by tests against `serde_json::Value` goldens (Go JSON
tags verified verbatim in the pinned upstream tree), covering both casing
regimes: camelCase descriptor/manifest/index fields and PascalCase image
execution parameters.

## Milestone C — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-distribution` | `url.rs`: absolute URL parser (scheme/host/port/IPv6, default-port normalization, dot-segment removal, query preservation), `join` resolution and query encoding. `http.rs`: synchronous HTTP/1.1 over `std::net::TcpStream`; `native-tls` for HTTPS (SChannel on this host); one request per connection (`Connection: close`); connect/read/write timeouts; Content-Length/chunked/EOF response bodies; head and body size ceilings; up to 5 redirects with cross-origin `Authorization`/`Cookie` stripping and HTTPS→HTTP downgrade rejection; request-header injection guards. `auth.rs`: RFC 7235 challenge parser (Bearer realm/service/scope, Basic, multiple challenges per header or across headers, quoted commas and escapes) + RFC 7617 Basic header. `client.rs`: `/v2/` ping; bearer challenge → token → retry with a per-scope token cache (30 s expiry margin, `token`/`access_token`); Basic-only registries and Basic credentials on the token endpoint; monolithic `POST`→`PUT` and chunked `PATCH` uploads (`Content-Range start-end`, `416` status query + resume capped at 2 restarts, `OCI-Chunk-Min-Length`, upload-`Location` rotation); manifest GET/PUT with content negotiation, body-digest and `Docker-Content-Digest` verification, schema 1 → `Unsupported`; blob `HEAD` presence/size; digest-verified pull streamed into `BlobStore`; tag listing with `Link: rel="next"` pagination (100-page cap, cycle detection); referrers API with tag-schema fallback and client-side `artifactType` filtering when `OCI-Filters-Applied` is absent; manifest/blob DELETE; registry error envelopes surfaced in classified messages; redirects to offloaded hosts carry no registry credentials. |
| `docker-storage` | `BlobStore::put_stream`: hashes while writing to a temp file, verifies expected digest and size before atomic rename, removes the temp file on mismatch/read error, dedupes an existing blob, and detects store corruption; used by registry pulls so layers are never buffered whole in memory. |
| `tests/integration` | `registry_vertical_slice.rs`: 8 tests against an in-process loopback HTTP registry — bearer token flow with caching, monolithic push (including second-push dedupe) + manifest PUT/GET + tags + byte-identical digest-verified pull + classified 404/405 failures + schema 1 rejection; cross-origin `307` redirect strips the bearer token; corrupted blob pull fails with `InvalidArgument` and leaves the store empty; chunked upload resumes after `416` and follows the status `Location`; `OCI-Chunk-Min-Length` raises the effective chunk size; tag pagination; referrers API filtering + tag-schema fallback; `403 DENIED` and repeated-`401` classification. |

## Milestone D — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-daemon` | `engine.rs`: public `Daemon` handle plus one actor thread that exclusively owns the `ContainerStore`, active tasks, restart deadlines, and pending replies. Commands cover create/start/stop/kill/restart/remove/inspect/list/shutdown; create validates config and host config, enforces name uniqueness, and emits `create`; start creates and starts a runtime task, spawns a monitor thread, transitions to `running`, and emits `start`; stop sends the configured stop signal, emits `kill`, starts a grace timer, escalates to SIGKILL, and resolves the caller only when `TaskExited` arrives; kill honors stop-signal/SIGKILL restart cancellation while arbitrary signals may still restart under `always`; restart reuses the stop path and emits `stop` → `start` → `restart`; remove refuses running containers unless forced, in which case it force-stops with a 3 s timeout before emitting `destroy`; auto-remove runs only when no restart is scheduled and not during a manual restart's intermediate stop; stop timers and restart deadlines are driven by `recv_timeout` in the actor loop. `restart.rs`: Moby `restartmanager` port (policy `no`/`always`/`unless-stopped`/`on-failure[:max]`, 100 ms initial delay, doubling, 60 s cap, reset after a 10 s run, explicit cancellation, single active evaluation) with 9 unit tests. `runtime.rs`: `Runtime`/`RuntimeTask` boundary and `RuntimeConfig`/`ExitStatus` (the containerd Task-equivalent seam). `synthetic.rs`: deterministic in-process runtime with scripted natural exit, ignored SIGTERM, create/start/wait failures, and recorded create/start/kill/delete calls. |
| `docker-container-model` | `ContainerRecord` now carries `HostConfig` so restart policy, stop timeout, and auto-remove remain available on every exit; `ContainerStore::get_mut` lets the single actor update state without removing/reinserting (stable ID index); `Signal::number()` supplies the POSIX integer used by kill events and `128+signal` exits. |
| `tests/integration` | `daemon_vertical_slice.rs`: 14 tests covering create→start→stop event order and attributes, SIGTERM→SIGKILL escalation, `on-failure` retry cap, stop during restart backoff, non-forced removal during restart backoff, manual restart event ordering and count reset, `always` restart after an arbitrary signal but cancellation on stop, force remove, auto-remove after natural exit and after manual stop (without removing during restart), create validation/name conflicts, runtime start/create failures with retry, and an 8-thread × 10-container lifecycle stress test. |

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

- Milestone D is a headless daemon core: no HTTP server, no container
  execution, and no build yet. E–L remain source work beyond the A–D layers.
- Daemon limitations kept explicit: `SyntheticRuntime` is the only runtime;
  there is no pause/unpause, no persistence/reconnect across daemon restart,
  no auto-generated name for an unnamed create, and no negative
  (wait-forever) stop timeout. Start/stop on an already-running/stopped
  container return success without an event; the HTTP layer must translate
  those into upstream's 304 Not Modified response in F/G.
- Registry client: no cross-repository blob mount (every push does `HEAD`
  and then a full upload), no single-`POST` monolithic upload (optional in
  the spec), schema 1 manifests rejected rather than parsed, no Docker
  config / credential-helper loading (callers provide Basic credentials), no
  client certificates or pinned CA bundle (OS trust store via `native-tls`;
  on Linux that pulls in OpenSSL and needs a C toolchain), and token caches
  are in-memory only.
- Windows host has no container backend for differential-at-runtime tests
  (see TESTING.md constraints).
- `ContainerStore` is intentionally not internally synchronized. Milestone D
  gives it a single owner (the daemon actor); standalone users must provide
  their own synchronization.
- `Descriptor::validate` checks required fields, non-negative size, and
  embedded-data consistency; the broader §descriptor conformance matrix is
  still planned, while the distribution client enforces protocol-level
  media-type and digest rules at its own boundaries.
