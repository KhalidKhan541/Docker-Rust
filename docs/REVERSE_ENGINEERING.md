# Reverse-engineering record

This document ties each implemented behavior in docker-rs to the original
source it was derived from. It is the traceability start point:

```
original Go implementation -> observed behavior -> Rust design
    -> Rust implementation -> tests -> compatibility status
```

## Pinned source revisions

Cloned shallow (`--depth 1 --branch <tag>`), preserved under `upstream/`
and excluded from this repo's git history by `.gitignore`:

| Path in `upstream/` | Repo | Tag | Pin (commit dereferenced by the tag) |
|---|---|---|---|
| `moby/` | github.com/moby/moby | v28.5.2 | `89c5e8fd66634b6128fc4c0e6f1236e2540e46e0` |
| `cli/` | github.com/docker/cli | v29.9.1 | `dfc520dcc28994c4c699b9b219fd2f5a6bcec0ff` |
| `containerd/` | github.com/containerd/containerd | v2.4.2 | `b7a25261e029d68978bbf290df0a62fc819b3c51` |
| `runc/` | github.com/opencontainers/runc | v1.5.2 | `29dd3dc2b13b4123162e5fe132504bb4b15569f1` |
| `buildkit/` | github.com/moby/buildkit | v0.34.0 | `f5b59a1f2fb25a0cb7072c398cab9aed7835da4e` |
| `runtime-spec/` | github.com/opencontainers/runtime-spec | v1.3.0 | `92249139eea7161e13745abd4cb6d0ea02a3227a` |
| `image-spec/` | github.com/opencontainers/image-spec | v1.1.1 | `147f9c13cedb47a0c4d9a11a222961073d585877` |
| `distribution-spec/` | github.com/opencontainers/distribution-spec | v1.1.1 | `a139cc423184af6078077b9b7ee336eddbd03f8f` |

Note: the Docker CLI (`docker/cli` v29.9.1) is newer than the pinned engine
(moby v28.5.2 — CLI 29.x talks to Engine 29.x). CLI v29 was pinned because
it is the current stable CLI line; where CLI behavior depends on a newer
Engine API than v28.5.2 exposes, the Engine side governs and the mismatch
is recorded in `docs/API_COMPATIBILITY.md`.

## Method

1. Locate the Go implementation of the behavior (package + file).
2. Read the exact control flow, error classes, data shapes, and default
   values; prefer tests in the original repo over prose.
3. Encode the behavior in a Rust module at the matching responsibility
   layer, keeping the upstream behavior comments ("mirrors <file>") next
   to the code.
4. Add unit tests that exercise the *observed behavior*, not the code
   shape.
5. Record compatibility deltas in `docs/API_COMPATIBILITY.md` /
   `docs/OCI_COMPATIBILITY.md`.

## Subsystem traces (Milestone A scope)

### 1. Error taxonomy (`crates/errors`)

* Upstream: moby `errdefs/errors.go` — sentinel error classes
  (`ErrNotFound`, `ErrInvalidArgument`, `ErrConflict`, `ErrBusy`,
  `ErrNotImplemented`, `ErrThrottle`, `ErrUnavailable`, `ErrCancelled`,
  `ErrDeadlineExceeded`, `ErrUnauthorized`, `ErrForbidden`,
  `ErrPermissionDenied`) plus HTTP mapping via
  `GetHTTPErrorStatusCode`.
* Observed behavior verified from source: `Conflict` and `Busy` both map
  to 409; `Cancelled` maps to the nonstandard 499 internally but must
  never be emitted as a status by the API layer (mapped to 500 here);
  messages are returned verbatim to clients without a status prefix.
* Rust: `docker_errors::ErrorKind` + `Error` with `http_status()` table.
* Tests: `crates/errors/src/lib.rs` (status mapping, message-only Display,
  cause chaining, serde round trip).

### 2. Identifier generation (`crates/common::rng`, `ids`)

* Upstream: moby `pkg/stringid/generate.go` (`GenerateRandomID` =
  32 random bytes from crypto/rand, lowercase hex → 64 chars), and
  prefix resolution used thoughout `daemon/container.go`.
* Rust: `docker_common::rng::generate_id`, `ContainerId` (64 hex with
  case normalization), `parse_id_prefix` (1..=64 hex).
* Tests: length/alphabet/aliasing behaviors in
  `crates/common/src/ids.rs`.

### 3. Image reference grammar (`crates/common::reference`)

* Upstream: `github.com/distribution/reference` (vendored by moby at
  `moby/vendor/github.com/distribution/reference/normalize.go`,
  `regexp.go`) — domain detection (must contain `.`, `:`, or be
  `localhost`), tag `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`, repository
  path components lowercased alphanumeric with separator rules, digest
  suffix.
* Rust: `docker_common::ImageRef` with the same decision rules for the
  supported subset; single-component repos report the implicit
  `library/` Docker Hub path in `canonical_repo()`.
* Tests: `crates/common/src/reference.rs` (registry-with-port,
  localhost registry, digest pinning, rejection matrix).

### 4. Digests (`crates/common::digest`)

* Upstream: OCI image-spec v1.1.1 (`specs-go/v1/descriptor.go`) digest
  grammar; moby layer/content store uses `sha256:<64hex>`.
* Rust: `Digest::parse` strict validation; `Digest::sha256(bytes)`
  hashing (test vector: empty-input SHA-256 reference vector).
* Tests: `crates/common/src/digest.rs`.

### 5. Container naming (`crates/common::names`)

* Upstream: moby container name regex (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`) used
  by the daemon when validating user-supplied names;
  `docker_common::Name` implements the same character class with an
  explicit 255-byte docker-rs bound (upstream enforces none; documented
  difference in API_COMPATIBILITY.md).
* Tests: `crates/common/src/names.rs`.

### 6. Configuration layering (`crates/configuration`)

* Upstream: moby `daemon/config_unix.go`/`config_windows.go` merged over
  *defaults → daemon.json → flags → env*; per-key merge semantics.
* Rust: `DaemonConfig` (validated subset) + `FileConfig` (deny-unknown
  fields — upstream rejects unknown `daemon.json` keys) + env layer
  (`DRC_*`). Validation rules (log-level vocabulary, absolute data-root,
  host:port API host) follow upstream checks.
* Tests: `crates/configuration/src/lib.rs`.

### 7. Container lifecycle state machine (`crates/container-model::state`)

* Upstream: moby `container/state.go` — statuses `created`,
  `running`, `paused`, `restarting`, `removing`, `exited`, `dead`; the
  daemon refuses contradictory transitions (e.g. `created → paused`
  without a start; `dead` is terminal for run chains).
* Rust: `LifecycleState` + explicit transition table +
  `validate_transition` returning `Conflict` with the illegal pair. The
  Engine-API status strings are produced by `as_api_status()`.
* Tests: `crates/container-model/src/state.rs`.

### 8. Create-request validation (`crates/container-model::config`)

* Upstream: moby `api/types/container/config.go`, `hostconfig.go`,
  `resources.go`, plus daemon-side checks (`daemon` package).
* Observed rules carried over: env requires `KEY=value` with the key
  rejecting control/whitespace characters; `WorkingDir` must be absolute;
  `cpu-shares ∈ [2, 262144]`; memory limits ≥ 1 KiB when set; stop
  defaults: `SIGTERM` + 10 s grace; `privileged` is rejected unless the
  engine explicitly allows it; `restartPolicy` retry cap only valid for
  `on-failure`; extra-hosts `host:ip` syntax; port protocol triad
  tcp/udp/sctp.
* Rust: `ContainerConfig`, `HostConfig`, `Signal`, `PortKey`,
  `NetworkMode`, `RestartPolicy`.
* Tests: `crates/container-model/src/config.rs`.

### 9. Events (`crates/events`)

* Upstream: moby `api/types/events/events.go` (`Message`, `Actor` with
  uppercase `ID` json tag) and `daemon/events/events.go` (per-subscriber
  channels; slow listeners terminate their stream).
* Rust: `Event`/`Actor` serde shapes matching the Engine API wire format
  exactly (`type`, `action`, `scope`, `time`, `timeNano`, all lowercase
  keys, `actor.ID` uppercase), `EventBroker` with bounded per-subscriber
  queues and drop-on-full (documented difference: docker-rs drops for the
  slow subscriber, upstream closes that subscriber's stream).
* Tests: `crates/events/src/lib.rs`.

### 10. Container log entries (`crates/logging`)

* Upstream: moby `daemon/logger/jsonfilelog/jsonfilelog.go` — per-line
  JSON objects `{"log":<payload>,"stream":"stdout|stderr","time":<RFC3339>}`.
* Rust: `JsonLinesDriver` producing the identical schema + bounded
  in-memory tail for `docker logs` semantics; `LogDriver` trait for
  future drivers.
* Tests: `crates/logging/src/lib.rs` (wire-format byte equality with the
  upstream shape, ring tail behavior, append persistence).

## Subsystems pending source trace (documented in later milestones)

* Engine HTTP router surface (moby `api/server/router/...`).
* Volumes/networks managers (moby `daemon/volume.go`, `daemon/network.go`,
  libnetwork drivers).
* Runtime execution path (moby `daemon/exec_s3c273.go` equivalent glue →
  runc; containerd shim architecture `runtime/v2`).
* Layer store/snapshot graphs (moby `layer/`, `daemon/graphdriver/`;
  containerd `snapshots/`).
* Image pull/push paths (moby `distribution/`;
  distribution-spec `api.md`).
* BuildKit solver graph (`buildkit/solver/`, frontend `dockerfile/`).
