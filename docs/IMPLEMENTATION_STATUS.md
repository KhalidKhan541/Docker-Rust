# Implementation Status

Snapshot: Milestones A—E complete. Milestone E's Linux backend is verified on
`ubuntu-latest` CI: fmt/clippy/build/tests are green, the root smoke test
executes a real process in a materialized rootfs, and the Engine API smoke
test runs a container end-to-end through `docker-rsd --runtime oci` and serves
its output from `/containers/{id}/logs`. Milestone F/G has a runnable
container-lifecycle, image-management, image-archive, and logs HTTP API plus a
daemon binary and an opt-in same-origin static console (`--web-root`). Registry
pulls and Docker save/load are real and persistent; container records, captured
logs, and restart policies now persist under the daemon data root. Output-only
attach and non-interactive exec are implemented, including the Docker CLI
foreground `docker run` and `docker exec` paths; wait supports `not-running`,
`next-exit`, and `removed`. TTY/stdin, bridge networking, and cgroups remain
pending. Honest state of every layer.

## Milestone status

| # | Milestone | Status | Evidence |
|---|-----------|--------|----------|
| A | Foundation (errors, primitives, config, logging, events, container model) | **done** | 83 tests green; fmt/clippy clean; crates listed below |
| B | Image & content addressable store | **done** | 114 tests green (31 new); fmt/clippy `-D warnings` clean; crates listed below |
| C | Registry (distribution client) | **done** | 168 tests green (42 distribution unit + 8 registry integration added); fmt/clippy `-D warnings` clean |
| D | Daemon core (containerd-equivalent orchestration) | **done** | 191 tests green (9 daemon unit + 14 daemon integration added); fmt/clippy `-D warnings` clean; actor + synthetic runtime |
| E | OCI runtime (container execution) | **done — verified on Linux CI** | 25 runtime unit + 4 runtime integration tests; fmt/clippy clean on Windows and `x86_64-unknown-linux-gnu`; deterministic Windows `Unsupported`; the `ubuntu-latest` root smoke test executes a real static process through namespace unshare, chroot, `/proc` mount, and identity drop in a materialized image rootfs; `docker-rsd --runtime oci` runs the same probe over the Engine API; both init stages arm `PR_SET_PDEATHSIG(SIGKILL)` (stage 2 re-arms it after the container credential change) so a daemon crash tears down the container PID namespace |
| F/G | Engine HTTP API | **in progress - container + image + archive + logs + attach + exec + wait + persistence + console slices runnable** | 322 workspace tests green (36 `docker-api` unit + 15 loopback HTTP integration + 5 `docker-rsd` unit + 28 `docker-image-store` unit + 21 `docker-daemon` unit + 4 daemon persistence integration); fmt/clippy `-D warnings` clean; real Docker CLI v29.9.1 pull/save/load and Docker Hub smoke tests over TCP; `/containers/{id}/logs` served from the in-memory ring and persisted to `logs.jsonl`; output-only attach streams stdcopy frames with upgrade and chunked responses; exec create/start/inspect supports numeric users and propagates exit codes; all three Moby wait conditions are actor-side and flush response headers before blocking; container records and restart policies restored on daemon start; `--web-root` serves a same-origin console with API-precedence routing and traversal/symlink/device-name protections; a CI static-console smoke test asserts the same behavior; `docker-rsd --runtime oci` executes a loaded image end-to-end, including foreground `docker run` attach and `docker exec`, on `ubuntu-latest` |
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
| `docker-daemon` | `engine.rs`: public `Daemon` handle plus one actor thread that exclusively owns the `ContainerStore`, active tasks, waiters, exec instances, restart deadlines, and pending replies. Commands cover create/start/stop/kill/restart/remove/inspect/list/wait/exec-create/exec-start/exec-inspect/shutdown; create validates config and host config, enforces name uniqueness, and emits `create`; start creates and starts a runtime task, spawns a monitor thread, transitions to `running`, and emits `start`; stop sends the configured stop signal, emits `kill`, starts a grace timer, escalates to SIGKILL, and resolves the caller only when `TaskExited` arrives; kill honors stop-signal/SIGKILL restart cancellation while arbitrary signals may still restart under `always`; restart reuses the stop path and emits `stop` → `start` → `restart`; remove refuses running containers unless forced, in which case it force-stops with a 3 s timeout before emitting `destroy`; auto-remove runs only when no restart is scheduled and not during a manual restart's intermediate stop; waiters are registered in the actor for `not-running`, `next-exit`, and `removed`, and are woken by exits (including every restart), removal, or shutdown rather than by polling; exec starts a runtime task inside the active container, captures its stdio into a private log store, publishes `ExecExited` through an acknowledged command, and removes exec records with the container; stop timers and restart deadlines are driven by `recv_timeout` in the actor loop. `restart.rs`: Moby `restartmanager` port (policy `no`/`always`/`unless-stopped`/`on-failure[:max]`, 100 ms initial delay, doubling, 60 s cap, reset after a 10 s run, explicit cancellation, single active evaluation) with 9 unit tests. `runtime.rs`: `Runtime`/`RuntimeTask` boundary and `RuntimeConfig`/`ExitStatus` (the containerd Task-equivalent seam). `logs.rs`: per-container stdio capture with a 4 MiB ring, writer-lease close semantics, follow wakeups, and an append-only base64 JSONL file (`logs.jsonl`) that replays the newest records on daemon start. `persist.rs`: versioned container records under `<data-root>/containers/<id>/container.json`, temp-file-plus-rename writes, directory removal that also deletes the native rootfs, and recovery that marks a lost running process exited (code 255). `synthetic.rs`: deterministic in-process runtime with scripted natural exit, ignored SIGTERM, create/start/wait failures, opt-in exec behavior, and recorded create/start/kill/delete calls. |
| `docker-container-model` | `ContainerRecord` now carries `HostConfig` so restart policy, stop timeout, and auto-remove remain available on every exit; `ContainerStore::get_mut` lets the single actor update state without removing/reinserting (stable ID index); `Signal::number()` supplies the POSIX integer used by kill events and `128+signal` exits. |
| `tests/integration` | `daemon_vertical_slice.rs`: 14 tests covering create→start→stop event order and attributes, SIGTERM→SIGKILL escalation, `on-failure` retry cap, stop during restart backoff, non-forced removal during restart backoff, manual restart event ordering and count reset, `always` restart after an arbitrary signal but cancellation on stop, force remove, auto-remove after natural exit and after manual stop (without removing during restart), create validation/name conflicts, runtime start/create failures with retry, and an 8-thread × 10-container lifecycle stress test. |

## Milestone E — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-runtime` | `spec.rs`: OCI runtime-spec 1.3.0 `Spec`/`Process`/`Root`/`Mount`/`Linux`/resource/state types with camelCase tags matching `specs-go`, omit-empty behavior, strict unknown-field rejection, and semantic checks (absolute container `cwd`, absolute mount destinations, unique namespaces, valid env entries, rlimit ordering; execution additionally requires mount, PID, IPC, and UTS namespaces and rejects `domainname`). `bundle.rs`: `spec_from_container` maps the Milestone D model to a schema-valid `config.json` — effective argv, Moby PATH/HOSTNAME/TERM environment merge, numeric UID[:GID], working dir, hostname (short ID default), mount/pid/ipc/uts (+ network unless host mode) namespaces, canonical rootfs path/read-only flag, memory/CPU/pids resources, image/network annotations; named/container networking, `extra_hosts`, and non-numeric users return `Unsupported`. `Bundle::create/open/write_config/remove` materializes and round-trips a bundle without overwriting an existing directory. `task.rs`: `Runtime`/`RuntimeTask`/`RuntimeConfig` (now with optional rootfs), `ExitStatus`, and optional stdio handles moved out of `docker-daemon` so the OCI and synthetic runtimes share one boundary. `oci/mod.rs`: `OciRuntime`, feature policy (`ensure_supported`) that rejects every unimplemented security-relevant field explicitly, and init-helper discovery (`DOCKER_RS_RUNTIME_INIT` override). `oci/unsupported.rs`: non-Linux hosts return an actionable `Unsupported`. |
| `docker-runtime` (Linux) | `oci/linux.rs`: bundle creation plus task lifecycle (`start`/`wait`/`kill`/`delete`/`take_stdio`/`exec`); helper process group, SIGKILL to the whole group, SIGTERM forwarding, cached exit status, one-waiter guard, bundle cleanup, stage-2 PID discovery from stage 1's children, parentage re-validation around namespace-FD acquisition so a recycled host PID cannot be exec'd into, and bounded-JSON spec transport to the exec helper over stdin. `exec.rs` + `docker-runtime-exec`: open every target namespace descriptor before the first `setns` (the container mount namespace can hide host `/proc`), join mount/UTS/IPC/network/cgroup/PID namespaces, chroot through a retained rootfs descriptor, resolve the executable inside the rootfs, resolve numeric users through `/etc/passwd`/`/etc/group` for primary and supplementary groups, match the container's `no_new_privs: false` default instead of forcing NNP, re-arm `PDEATHSIG`, and propagate the child's exit code. `init.rs` + `docker-runtime-init`: two-stage init. Stage 1 creates mount/UTS/IPC/PID (+ optional network) namespaces, private mount propagation, recursive rootfs bind, hostname, then blocks its forwarded signals plus `SIGCHLD`, forks stage 2, and stays single-threaded: a `signalfd` forwards signals to stage 2 and `SIGCHLD` drives the `waitpid` reap. A helper thread is impossible here because the kernel rejects `clone(CLONE_THREAD)` with `EINVAL` once `unshare(CLONE_NEWPID)` has made `pid_ns_for_children` differ from the active PID namespace, and `unshare` itself requires a single-threaded caller. Both stages arm `PR_SET_PDEATHSIG(SIGKILL)` (stage 2 re-arms after `setuid`/`setgid`, which clear it), so an abruptly killed daemon takes stage 1 down, then stage 2 (PID 1), and the kernel kills the rest of the container PID namespace instead of orphaning it. Stage 2 is PID 1 in the new namespace; it restores the default signal mask (exec preserves it), chroots, mounts `/proc` in the correct PID namespace, remounts a read-only root, applies supplementary groups/GID/UID/`no_new_privs`, and `exec`s the container process. `nix` 0.30 safe APIs only (`unsafe_code = "forbid"`). |
| `docker-daemon` | `runtime.rs` re-exports the shared boundary; `engine.rs` wires the record and log stores into the actor, applies disk-first create/start/remove transitions and memory-authoritative exit transitions (a failed exit write is logged; the next start repairs a stale running record to exited/255), and keeps the Milestone D supervisor semantics. |
| `tests` | `crates/runtime/tests/schema.rs`: a draft-04 subset validator loads `config-schema.json`, `state-schema.json`, `defs*.json`, and `config-linux.json` directly from `upstream/runtime-spec/schema/`; generated bundle and state documents validate, and a negative test proves required-field detection. `runtime_windows.rs`: create on this host returns `Unsupported` naming Linux and the current OS. Bundle/model unit tests cover env merge order, default hostname, namespace mapping, resources, bridge annotation, numeric users, rootfs checks, round-trip, and conflict handling. |

Exit status: complete and verified. The portable bundle/schema/validation half
is covered by unit and schema tests. On `ubuntu-latest`, the root smoke test
executes a static probe inside a materialized image rootfs, and the Engine API
smoke test loads an OCI archive, creates and starts a container through
`docker-rsd --runtime oci`, and reads the probe output back from
`/containers/{id}/logs`. Two real defects were fixed to get there: stage 1
spawned its signal-forwarding thread *after* `unshare(CLONE_NEWPID)`, and the
kernel rejects `clone(CLONE_THREAD)` with `EINVAL` once `pid_ns_for_children`
differs from the active PID namespace (stage 1 now uses a blocked mask plus
`signalfd`, with `SIGCHLD` reaping stage 2), and the archive importer rejected
GNU tar's `./` root directory entry as an unsafe path.

## Milestone F/G — what exists, file by file

| Crate | Contents |
|-------|----------|
| `docker-api` | `version.rs`: API `1.24`–`1.51` gate with upstream-compatible too-old/too-new messages and `/vX.Y` path-prefix parsing. `dto.rs`: explicit Moby-cased wire DTOs for version/info/create/list/inspect, conversion to the internal model, image-default inheritance, and rejection of non-empty unsupported fields. `image_dto.rs`: Docker image summary/inspect/history/pull/load/delete wire shapes. `query.rs`: query decoding with Docker boolean semantics, timestamp parsing, list/event filters, and explicit rejection of unimplemented filters. `server.rs`: synchronous `tiny_http` transport over the daemon actor and image store; `/_ping`, `/version`, `/info`, container CRUD/lifecycle/wait/logs/attach, exec create/start/inspect, image list/pull/inspect/history/tag/delete, `/images/get`, `/images/{name}/get`, `/images/load`, and `/events`; output-only attach and exec start use stdcopy-framed `101` upgrade streams or chunked `200` responses; exact `Content-Length` reads on upgrade requests prevent body bytes from being mistaken for the hijacked stream; 64 GiB archive spooling, OCI-platform query decoding, newline-delimited load status; shared `Api-Version` headers and `{"message":...}` errors; Moby `304` start/stop semantics; chunked live event streaming and actor-side wait conditions (`not-running`, `next-exit`, `removed`). `static_files.rs`: opt-in `--web-root` GET/HEAD serving, single percent-decode, canonical-root containment (symlink/junction escape rejection), dotfile/backslash/colon/Windows-device rejection, 64 MiB cap, `/` → `index.html`, extensionless → `.html`, directory index, MIME map, and API-namespace precedence. |
| `docker-image-store` | Persistent `<data-root>/images/images.json` reference index plus verified-CAS blobs. Canonical/familiar Docker reference normalization, tag/repository-digest canonical keys, ID/prefix/tag/digest resolution, tag moves, conservative untag/delete + blob GC, direct manifest pulls through `docker-distribution`, multi-platform index selection, config/layer descriptor size and digest verification, host-platform default, loopback plain-HTTP registries, and Basic/anonymous `X-Registry-Auth` parsing. `archive.rs` writes OCI layout plus legacy `manifest.json`, copies verified layer blobs, and imports OCI or legacy plain/gzip tar archives with path-traversal guards, entry/byte limits, digest/diffID verification, gzip support, and tag restoration. |
| `docker-rsd` | Runnable daemon binary. Resolves `daemon.json`-style config and `DRC_*` environment overrides, initializes tracing, opens the image store, constructs the broker/runtime/actor, restores persisted container records/logs, and serves the API on `--host` (default loopback `127.0.0.1:2375`). `--data-root`/`DRC_DATA_ROOT` selects the persistent image and per-container root. `--runtime synthetic` (default) serves the in-process runtime; `--runtime oci` (alias `native`) wires `OciRuntime` to `ImageRootfsProvider`, which materializes image layers into `<data-root>/containers/<id>/rootfs` atomically, and preflights root and the init helper. `--web-root <dir>` opts into same-origin static console serving after startup validation (the directory is canonicalized; a missing path or file fails with InvalidArgument). `npipe://`/`unix://` are rejected instead of pretending they work. |
| `docker-events` | Bounded replay history (4096 events by default), atomic history-snapshot + subscribe (`subscribe_since`), `history_len`, `recv_timeout`, and lowercase action serialization matching Moby's wire JSON. |
| `docker-container-model` / `docker-daemon` | `created_at` on records and `started_at`/`finished_at` on state snapshots, populated by actor lifecycle transitions and exposed through inspect/list. |
| `docker-runtime` / `docker-daemon` (logs) | `runtime` gains a per-task `RuntimeConfig::capture_stdio` switch: runtimes create owned pipes when set, claimed with `RuntimeTask::take_stdio`; the Linux runtime honors it on its init helper without changing the parent-side protocol. `docker-daemon` gains `logs.rs`: a bounded per-container ring store (`LogEntry { seq, stream, timestamp, data }`, 4 MiB default) with an atomic `entries_and_state` snapshot, `wait_for_change`, lease-based writer accounting so follow streams end exactly when the last capture pipe reaches EOF, plus close on exit/removal/shutdown and reopen on restart. `engine.rs` spawns one drain thread per claimed pipe, drops the stdin writer end (detached semantics), and exposes `Daemon::log_store(reference)` to the API. The same `EntryStream` serves logs, output-only attach, and non-detached exec; an attach to a stopped container reopens its store and stays open for the next start/exit (the next exit, removal, or shutdown closes it again), attach accepts the Docker CLI's configured `detachKeys` but rejects stdin explicitly, and exec holds its writer lease until the actor acknowledges the exit code so an inspect immediately after attach EOF cannot observe a stale status. |
| Tests | `crates/api/tests/http_api.rs`: a real loopback TCP client (Content-Length and chunked decoding) exercises version gates/headers, create/list/inspect/start/stop/restart/kill/wait/delete, generated names, short ids, filters, image defaults/create validation, image list/inspect/history/tag/delete, image save/load round-trip (OCI + legacy, gzip, platform filters), unsupported fields/endpoints, container logs (stream-less 400, empty 200 raw stream), output-only attach (upgrade `101`, stdcopy replay/live bytes, stream and `logs` selection, stdin rejection), exec create/start/inspect (upgrade stream, output, status 7, numeric-user enforcement, named-user 501, unsupported TTY/stdin/privileged), wait conditions (`next-exit` before start and again after a previous exit, with `removed`), event replay/live streaming, and a real pull against an in-process loopback OCI registry fixture. It also covers `--web-root` index/clean URL/MIME/API precedence/reserved-namespace/versioned-path/405/404/HEAD behavior. `docker-image-store` unit tests cover persistence, resolution, delete/GC, reference canonicalization, platform selection, auth parsing, pull verification, archive export, legacy/gzip import, tag restoration, and traversal rejection. `docker-rsd` unit tests cover option parsing and explicit named-pipe rejection. `docker-daemon`/`docker-api` unit tests cover ring eviction, stream filters, follow/lease liveness, stdcopy framing, rfc3339NanoFixed stamps, line-accurate tail slicing, and relative Go duration parsing; `static_files` unit tests cover traversal/encoded traversal/dotfiles/backslashes/device names/symlink escapes and MIME mapping. `tests/static-console-smoke.sh` is the Linux CI same-origin end-to-end check. `tests/linux-native-smoke.sh` drives the real Docker CLI against `docker-rsd --runtime oci` for foreground `docker run` attach, `docker exec --user 1000:1000` output, and `/exit7` status 7. Manual differential smoke tests used the pinned Docker CLI v29.9.1 against `docker-rsd`, pulled `alpine:3.20` from Docker Hub, and exercised `docker save`/`docker load`. |

What the real Docker CLI successfully exercised over
`DOCKER_HOST=tcp://127.0.0.1:<port>`: `version`, `info`, `pull --platform
linux/amd64`, `images`, `image inspect`, `tag`, `rmi`, `create`, `run -d`,
`ps`, `inspect`, `start`, `stop`, `kill`, `rm`, `wait`, `events`, `save`, and
`load` (including gzip archives). `logs` streams each container's captured
stdio with stdcopy framing (`application/vnd.docker.multiplexed-stream`);
output-only `attach` and non-interactive `exec` use the same framing over the
Docker CLI's upgrade path, including foreground `docker run`, `--user
1000:1000`, and `/exit7` exit-code propagation. Synthetic-runtime containers
never execute, so their logs are empty, and log options follow Moby semantics
(`follow`, `timestamps`, `tail` incl. `all`, `since`/`until` with unix,
RFC3339, and relative Go durations; `details` returns explicit unsupported). A pull
without `--platform` honestly reports that the `windows/amd64` image is not
present in Alpine's manifest list and lists the available platforms.

## Toolchain (environment)

- stable-x86_64-pc-windows-gnu; `tools/env.ps1` PATH preloader mandatory.
- MSYS2 binutils 2.47 + runtime DLLs in `tools/mingw64` (gitignored) plus
  sysroot `self-contained` shim (as.exe copies) — fixed and reproducible.
- `x86_64-unknown-linux-gnu` rust-std added for `cargo check`/`cargo clippy`
  of the Linux-only runtime backend (no Linux linker or kernel is present).

## Reverse-engineering tooling

- All 8 upstream repos pinned at listed revisions under `upstream/`.
- rea MCP server: **connected and verified callable** (startup-timeout fix
  applied to `~/.config/opencode/opencode.json`, rea now answers tool
  calls; reserved for compiled-artifact RE in milestone L, naked for text
  phases where the Go sources answer directly).

## Honest gaps

- Milestone F/G covers the system, container, image-management, archive,
  logs, and persistence slices, not the full Engine API. No network, volume,
  build, plugin, or swarm endpoints are routed. Container create resolves the
  image record and inherits its execution defaults; `docker-rsd --runtime oci`
  materializes the image rootfs for execution.
- Image management is partial: save/load exist, but there is no push, prune,
  build, manifest inspection, non-empty image filters, cross-platform
  inspect/history, or multi-tag deletion without `force`; those paths return
  explicit `Unsupported`/`Conflict` errors. Save/load spool archives through
  temporary files rather than streaming end to end. Pull progress is buffered
  into a final summary, image deletion does not yet check container use, and
  the JSON index rename is not atomic on Windows.
- `docker-rsd` defaults to the synthetic runtime: HTTP lifecycle behavior and
  events are real, but no host process runs and `State.Pid` is zero, so
  captured logs are empty. `--runtime oci` executes real containers and
  captures their stdio. Attach is output-only: stdin and TTY are rejected
  explicitly, and a configured `detachKeys` is accepted but unused. A
  streaming attach to a stopped container waits for its next run, matching
  `docker start -a`. Exec is non-interactive: no stdin, TTY, resize, or
  privileged mode; users must be numeric `UID[:GID]` (a missing GID and
  supplementary groups are resolved from the container's `/etc/passwd` and
  `/etc/group`; named lookup is rejected), exec records are in-memory and
  disappear on daemon restart. Foreground `docker run`, output-only
  `docker attach`, and non-interactive `docker exec` work through the real
  Docker CLI on Linux.
- The HTTP listener is TCP-only, unauthenticated, and unencrypted. The
  default is loopback; exposing it is unsafe. Unix sockets and Windows named
  pipes are rejected as `Unsupported`.
- `--web-root` is a minimal same-origin static server, not a general web
  server: GET/HEAD only, no directory listing, SPA fallback, range requests,
  compression, cache validators, CORS, or auth. Docker API namespaces and
  `/vX.Y` prefixes are reserved. The web root is trusted content, so any other
  readable regular file under it can be downloaded.
- Container records and captured logs persist under
  `<data-root>/containers/<id>/` (`container.json`, `logs.jsonl`, optional
  native `rootfs/`). A container that was running, paused, or restarting when
  the daemon stopped cannot be adopted: it is restored as exited with code 255
  and its record is rewritten. `--restart always|unless-stopped|on-failure`
  starts it again after recovery; `no` leaves it exited. Restart counts and
  the "manually stopped" flag are not persisted, so recovery starts with a
  fresh restart manager. The native init helper is tied to the daemon with a
  parent-death signal: if the daemon is killed abruptly, stage 1, stage 2
  (PID 1), and the whole container PID namespace die with it, so recovery
  cannot leave an orphaned process behind.
- Log persistence is append-only and unbounded (no rotation, compression, or
  log-driver options); after a restart only the newest records that fit the
  4 MiB in-memory ring are served, while older records remain in the file. The
  format is docker-rs's base64 JSONL, not Moby's json-file schema, so container
  output round-trips losslessly even when it is not UTF-8.
- F/G API gaps, all explicit rather than silent: no stats, top, changes,
  export, archive, pause/unpause, update, rename, or resize; those
  subresources return 501. Logs, output-only attach, and non-interactive exec
  are implemented; log `details` (extra attach attributes) returns explicit
  unsupported. Attach/exec support the Docker CLI's HTTP upgrade
  (`101`) and a chunked `200` fallback; WebSocket attach is not implemented.
  Stdin and TTY are rejected at the boundary rather than simulated. A wait
  client that disconnects can leave its actor-side `next-exit` waiter until
  the container exits or the daemon shuts down because per-request
  cancellation is not implemented. Unrouted paths return 404.
- Milestone E's Linux backend is verified on `ubuntu-latest` CI (both root
  smoke steps). This Windows host still cannot execute it locally because it
  has no Linux kernel, WSL distro, or VM.
- E execution limitations, all explicit errors rather than silent fallbacks:
  `docker-rsd` materializes image rootfs through `ImageRootfsProvider` (crate
  callers must supply a rootfs or provider); no cgroups (memory/CPU/pids
  limits are accepted by the bundle builder but rejected by
  `ensure_supported`); no capability dropping, seccomp, AppArmor/SELinux,
  user namespaces, TTY/console allocation, additional mounts, hooks, devices,
  bridge networking, or custom rlimits. Root in the container retains the
  helper's initial-user-namespace capabilities, so this backend is not yet a
  security boundary (Milestone K). `/proc` is mounted, but `/dev`, `/dev/pts`,
  `/sys`, `/etc/hosts`, and `/etc/resolv.conf` are not; `none` networking
  leaves loopback down; stop/delete uses the helper process group rather than
  a cgroup, so a process that creates a new session can escape it.
- Daemon limitations kept explicit: `docker-rsd` defaults to the synthetic
  runtime; `--runtime oci` (alias `native`) runs real Linux containers.
  There is no pause/unpause, no reattach across a daemon restart (the helper
  is tied to the daemon, and restart policies start a fresh process instead),
  and no negative (wait-forever) stop timeout. Start/stop no-ops are translated
  to upstream's 304 by the HTTP layer, and unnamed API creates receive a
  generated `docker-rs_<12-hex>` name.
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
