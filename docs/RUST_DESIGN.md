# Rust Design

Decisions that are specifically Rust-side (not ported from Go), with the
reason each was chosen.

## Workspace & toolchain

- Edition 2021, resolver `2`, one workspace manifest per crate under
  `crates/`; integration tests live in `tests/integration`.
- Toolchain: stable `x86_64-pc-windows-gnu` (host reality: Windows 10 x64,
  2-core i3 Rust-compile box). A `tools/env.ps1` preloader is **required**
  for every cargo invocation (PATH for cargo, rustup self-contained
  binutils, and MSYS2 `binutils 2.47` fetched into `tools/mingw64`).
- dlltool/as chain fixed at sysroot level; do not undo (see
  `tools/mingw64/README.md` note in repo history).

## Error model

- `thiserror 2` for the single `Error` type in `docker-errors`;
  classification is data (`ErrorKind`), not a type hierarchy, so API
  mapping (`http_status()`) stays a pure function.
- `Conflict`/`Busy` → 409; `Cancelled` → 499 but kept internal (upstream's
  http_helpers treat it the same way).
- `serde_json::Error` converts with `From` so config-file parsing errors
  ride the same channel.

## Serde and wire compatibility

- Upstream JSON wire shapes are **schema**, not style: event casing
  (`type/action/scope/time/timeNano`, uppercase `ID`), json-file log keys
  (`log/stream/time`), daemon.json kebab-case (`log-level`, `data-root`)
  are pinned by tests, not by struct naming taste.
- `Digest` serializes as the canonical `algo:hex` string (custom impl) —
  never as a two-field object — matching image-spec descriptors on the wire.
- `FileConfig` uses `deny_unknown_fields` on purpose: the upstream daemon
  rejects unknown configuration options at startup; silently ignoring typos
  like `data_root` vs `data-root` would be a security regression.

## Concurrency posture

- `ContainerStore` is a single struct with BTreeMap-backed prefix scans and
  **no internal locking**. Milestone D gives it exactly one owner: the
  `docker-daemon` actor thread. Public `Daemon` handles hold only a
  `Mutex<Sender<Command>>` (for `Sync`) and every lifecycle call is a
  command/reply round-trip, so there are no self-deadlocks and no torn
  transitions.
- The actor is deliberately synchronous: `std::sync::mpsc` plus
  `recv_timeout` for the nearest restart/stop deadline. No Tokio, no async
  mutex, no timer wheel. A single thread owns all container state; blocking
  work must stay in runtime/monitor threads, not in the actor.
- Each started container gets a monitor thread that blocks in
  `RuntimeTask::wait`, deletes the task, and posts `TaskExited`. The actor
  owns task handles and cancels stale generations by generation number.
- Stop/restart/force-remove use deferred replies: the reply sender lives in a
  pending map and is completed by `TaskExited` (or by an explicit error at
  the 10 s hard timeout). Callers never poll.
- `Runtime` and `RuntimeTask` are `Send + Sync`; Milestone E added a
  process-backed `OciRuntime` without changing the actor. The synthetic
  runtime remains a testable production implementation, not a hidden mock.
- Event broker is bounded (capacity 1024) and drop-oldest: never blocks,
  never panics on full, and surfaces drop counts to the subscriber stream
  (documented divergence from upstream's stream-closing behavior).

## "Honest Unsupported" policy

Any host feature that cannot be provided (Linux namespaces on this Windows
host, cgroups v2, rootless elevation, overlay2 on Windows data roots,
`md5`/`sha1` digest algorithms) must produce `Error::unsupported(...)`
describing exactly what is missing — never a fallback, never a silent
no-op. This is tested (`ChildProcess::spawn`, digest algorithms, platform
capability probes).

## No-panic discipline

Constructors that can fail (`Name::new`, `ContainerId::new`,
`Digest::parse`, `ImageRef::parse`, `DaemonConfig::validate`) return
`Result`; panic paths (`unwrap` in tests only) are gated by `#[cfg(test)]`.

## Logging

`tracing` with levels mapped from upstream daemon levels (`debug`,
`info`, `warn`, `error`, `panic`, `fatal`); subscriber init is a function,
not a global side effect, so tests can capture.

## Binary RE tooling (rea MCP)

Rea (reverse-engineering MCP) will be used later for: differential analysis
of compiled `dockerd.exe` / `runc.exe` artifacts against our Rust builds
(behavior matching, error-path probing via Ghidra), and runtime probing of
running daemons during milestone L conformance work. Source-level
engineering uses the pinned Go trees directly — REA adds its value when the
artifact is a binary, not a text file.
