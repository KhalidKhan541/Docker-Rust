# Roadmap

Milestone plan with concrete exit criteria. Material for each milestone is
taken only from pinned upstream sources (root `README.md` pins table).

## A — Foundation ✅ (complete)
JSON wire shapes for events, logs, daemon config; error taxonomy;
primitives (id, digest, name, ref, safe path, platform, child-process
scaffolding); container state machine + store; validation suite.
Exit: workspace green (`cargo test` 83/83, fmt, clippy). **Met.**

## B — Image & content store (`docker-image`)
Port `image-spec/specs-go/v1` types (descriptor, config, manifest, index,
layout); content-addressed blob store keyed by verified digest; layer
extraction/apply on a `SafePath` root.
Exit: full load/push-to-dir cycle with schema-validated manifests; digest
mismatch = error; image inspection API types defined and unit-tested.

## C — Distribution client (`docker-distribution`)
Implement distribution-spec v1.1.1 client: auth challenge/token, blob
upload (single + chunked), manifest PUT/GET, tag listing under
`content-negotiation.md` rules; TLS-first with pinned roots.
Exit: push/pull against a pinned local registry fixture round-trips
byte-identical content; failure paths produce classified errors.

## D — Daemon orchestration core
Single-owner container store behind an actor; state machine drives
create→start→die transitions with events emitted at each step; restart
policy engine; task supervision scaffolding equivalent to
`moby/daemon/container.go` + `moby/container/monitor.go`.
Exit: container lifecycle runs headlessly with synthetic runtime stub;
event stream reflects transitions; store stress tests pass.

## E — OCI runtime (`docker-runtime`)
Own OCI-bundle creation (runtime-spec 1.3.0 structures + schema
validation of emitted JSON); execution via platform-appropriate process
spawn; honest `Unsupported` for missing kernel features; attach/stdio
plumbing.
Exit: a container created from our model produces a schema-valid bundle
and executes a real process on a supported host; on this Windows host the
tests assert explicit `Unsupported` (not silent paths).

## F/G — Engine HTTP API (`docker-api`)
Router + version middleware per `moby/api/common.go` gates; endpoints in
API_COMPATIBILITY.md order; streaming (`/events`, `/logs`) with
backpressure; hijack for exec/attach after E.
Exit: CLI-equivalent clients (curl) operate containers through HTTP;
version-gate matrix tests green; error payloads byte-match upstream
message conventions.

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
