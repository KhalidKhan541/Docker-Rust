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

- Milestone A containers are pure state machines; the store is a single
  struct with BTreeMap-backed prefix scans. **It is not yet thread-safe** —
  the daemon will own exactly one store handle behind an async mutex /
  actor in Milestone D, at which point the tests gain a stress module.
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
