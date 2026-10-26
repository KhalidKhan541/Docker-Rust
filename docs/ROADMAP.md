# Roadmap

Milestone plan with concrete exit criteria. Material for each milestone is
taken only from pinned upstream sources (root `README.md` pins table).

## A — Foundation ✅ (complete)
JSON wire shapes for events, logs, daemon config; error taxonomy;
primitives (id, digest, name, ref, safe path, platform, child-process
scaffolding); container state machine + store; validation suite.
Exit: workspace green (`cargo test` 83/83, fmt, clippy). **Met.**

## B — Image & content store (`docker-image`, `docker-storage`) ✅ (complete)
Port `image-spec/specs-go/v1` types (descriptor, config, manifest, index,
layout); content-addressed blob store keyed by verified digest; layer
extraction/apply on a `SafePath` root.
Exit: full load/push-to-dir cycle with schema-validated manifests; digest
mismatch = error; image inspection API types defined and unit-tested.
**Met** (114 tests at the B snapshot).

## C — Distribution client (`docker-distribution`) ✅ (complete)
Implement distribution-spec v1.1.1 client: auth challenge/token, blob
upload (single + chunked), manifest PUT/GET, tag listing under
`content-negotiation.md` rules; TLS-first.
Exit: push/pull against a pinned local registry fixture round-trips
byte-identical content; failure paths produce classified errors.
**Met** — in-process loopback registry fixture implementing the pinned spec
(8 integration tests); real-binary differential deferred to L. 168 workspace
tests green.

Implementation notes: Bearer challenge → token → retry with per-scope token
caching, Basic registries and Basic credentials on the token endpoint;
monolithic `POST`→`PUT` and chunked `PATCH` uploads with `416` range resync,
status-`Location` rotation and `OCI-Chunk-Min-Length`; manifests verified
against their computed digest (and `Docker-Content-Digest`); tags follow
`Link: rel="next"`; referrers API plus the tag-schema fallback; schema 1 is
an explicit `Unsupported` error. TLS uses the OS trust store via
`native-tls` (SChannel here); redirect-following strips `Authorization`
cross-origin and refuses HTTPS→HTTP downgrades.

## D — Daemon orchestration core (`docker-daemon`) ✅ (complete)
Single-owner container store behind a command/reply actor; state machine
drives create→start→die transitions with events emitted at each step;
restart policy engine; task supervision equivalent to
`moby/daemon/monitor.go` + `moby/restartmanager/restartmanager.go`.
Exit: container lifecycle runs headlessly with the synthetic runtime;
event stream reflects transitions; store stress tests pass.
**Met** — 191 workspace tests green (9 daemon unit + 14 daemon integration
added); fmt/clippy `-D warnings` clean.

Implementation notes: one actor thread owns the `ContainerStore`, active
tasks, restart timers, and pending API replies; command senders are the only
public handle, so lifecycle transitions cannot interleave. Every started
task gets a monitor thread blocked in `RuntimeTask::wait`; the monitor
deletes the task and posts the exit back to the actor. Stop/restart/force-
remove replies are deferred until `TaskExited`; stop escalates to SIGKILL
after the configured grace period and fails honestly if the task still does
not exit after 10 s. Restart scheduling ports Moby's manager: 100 ms initial
delay, doubling to a 60 s cap, reset after a 10 s run, `no`/`always`/
`unless-stopped`/`on-failure[:max]` semantics, and cancellation on manual
stop/kill. Auto-remove fires only when no restart is scheduled, but not
during a manual restart's intermediate stop. Events follow upstream ordering
and attributes: `create`, `start`, `kill`, `die`, `stop`, `restart`,
`destroy`; labels are merged first and `image`/`name` override, while `die`
adds `exitCode`/`execDuration`.

Milestone D scope limits, documented rather than hidden: no pause/unpause
command, no HTTP surface, no persistence across daemon restart, no random
name generation for unnamed creates, no negative (wait-forever) stop
timeouts, and no real process execution — `SyntheticRuntime` is the only
runtime implementation until E.

## E — OCI runtime (`docker-runtime`) — **complete; Linux execution verified on CI**
Own OCI-bundle creation (runtime-spec 1.3.0 structures + schema
validation of emitted JSON); execution via platform-appropriate process
spawn; honest `Unsupported` for missing kernel features; attach/stdio
plumbing.

Implemented:

- Runtime-spec 1.3.0 Linux `Spec`/`State` types with strict unknown-field
  handling and semantic validation.
- `config.json` generation from `ContainerConfig`/`HostConfig`, including
  Moby effective-argv and PATH/HOSTNAME/TERM merge behavior, namespace
  selection, rootfs path/read-only, numeric user, and resources.
- Bundles validated against the pinned schemas loaded directly from
  `upstream/runtime-spec/schema/`.
- `Runtime`/`RuntimeTask`/`RuntimeConfig` moved to `docker-runtime`; stdio
  handle plumbing and cached exit status.
- Windows/non-Linux `OciRuntime::create` returns an actionable
  `Unsupported`.
- Linux-only two-stage init helper: stage 1 creates namespaces/rootfs bind;
  stage 2 is PID 1, mounts `/proc` in the correct PID namespace, applies
  identity/`no_new_privs`, and `exec`s the process. Stage 1 stays
  single-threaded (a thread cannot join an unshared PID namespace): a
  blocked signal mask plus `signalfd` forwards signals, and `SIGCHLD`
  drives the reap. Safe `nix` APIs only (`unsafe_code = "forbid"`).

Exit criterion met on `ubuntu-latest` CI: the runtime smoke test executes a
real static process inside a materialized image rootfs, and the Engine API
smoke test loads an OCI archive and runs a container through
`docker-rsd --runtime oci`. This Windows host still cannot execute the helper
(no Linux kernel/WSL/VM); local iteration relies on CI and its published
check-run annotations.

Known E limits: no cgroups, capability dropping, seccomp, LSM labels, user
namespaces, TTY, additional mounts, hooks, devices, bridge networking, or
custom rlimits. Each returns `Unsupported` rather than being ignored.

## F/G — Engine HTTP API (`docker-api`) — **container + image slices runnable**
Router + version middleware per `moby/api/common.go` gates; endpoints in
API_COMPATIBILITY.md order; streaming (`/events`, `/logs`) with
backpressure; hijack for exec/attach after E.

Implemented: API 1.24–1.51 gate and `/vX.Y` prefixes; `/_ping`, `/version`,
`/info`; container create/list/inspect/start/stop/restart/kill/wait/remove
with names and short ids; 304 start/stop semantics; list filters; image
list/pull/inspect/history/tag/delete backed by a persistent store and real
distribution client, including multi-platform selection and verified layer
downloads; Docker/OCI save/load (`/images/get`, `/images/{name}/get`,
`/images/load`) with legacy `manifest.json`, gzip input, and platform
filtering; create-time image resolution and image default inheritance;
`/events` history replay + live stream; `/containers/{id}/logs` over captured
stdio (stdcopy framing, `follow`/`timestamps`/`tail`/`since`/`until` with
Moby's error messages, relative Go durations, and lease-based follow EOF);
Docker-cased DTOs with explicit
rejection of unsupported fields; synchronous `tiny_http` transport; runnable
`docker-rsd` binary over TCP with `--data-root` and a native
`--runtime oci` backed by image-rootfs materialization. Verified with
curl-level integration tests, an in-process registry fixture, a live Docker
Hub pull, a manual Docker CLI v29.9.1 smoke test including save/load, and an
`ubuntu-latest` smoke run that executes a real container through the Engine
API and reads its output back from `/containers/{id}/logs`.

Still pending: push/prune, image filters, attach/exec hijack, stats/top, and
network and volume endpoints. Container state is in-memory only; image
metadata and blobs persist; captured logs stay in a bounded in-memory ring
and are lost on daemon restart.

Exit criterion partially met: curl/Docker-CLI container lifecycle and image
pull/list/inspect/tag/delete/save/load work over HTTP, the version-gate
matrix is green, and error payloads use upstream message conventions. Full
endpoint and error-payload conformance is milestone L.

## H/I — Builder (`docker-buildkit`)
Dockerfile frontend: instruction parsing → DAG (LLB-equivalent) →
execution against the image/blob store; dockerfile2llb conversion rules
ported with buildkit's test vectors; context upload (including
`.dockerignore` semantics from `frontend/dockerfile/` sources).
Exit: builds an image into the B store; provenance metadata emitted;
dockerfile2llb vector tests pass.

## J — CLI (`dockerrs`)
Command surface mirroring `cli/cmd/docker/docker.go` tree: run, ps, logs,
images, pull/push, build (buildkit path), version; config file handling;
completion scaffold.
Exit: golden-output tests; end-to-end `run` via HTTP API against our
daemon in D/E; reference grammar shared with `docker-common`.

## K — Security hardening
Privileged/caps policy layering; rootless-mode support on Linux targets;
`Unsupported` transparency for missing isolation (SECURITY_ANALYSIS.md
hooks); audit-log hook on privileged actions.
Exit: security checklist module passes; no endpoint allows privileged
execution without explicit opt-in; docs updated.

## L — Conformance & differential
Live differential vs upstream where a backend exists; binary-level
behavior probing via rea on pinned compiled artifacts (`dockerd`,
`runc` builds FROM the pinned revisions); API conformance suite from
`moby/api/server` sources; Harness failures documented as divergences or
bugs, never softened silently.
Exit: divergence ledger complete; every planned API endpoint verified
against upstream fixtures; REA-derived binary notes archived under
`docs/re-notes/`.

## Cadence

Each milestone: sources read → crates added with tests → docs updated
(`IMPLEMENTATION_STATUS.md`, mapping tables) → workspace green → commit.
