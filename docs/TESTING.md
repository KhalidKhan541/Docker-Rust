# Testing

## How to run

All commands require the PATH preload on this machine (Windows-gnu
toolchain):

```powershell
. E:\Docker-Rust\tools\env.ps1
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
```

Current status (Milestone A complete): **83 tests, 0 failed** across the
workspace; `cargo fmt --check` and `cargo clippy --workspace --all-targets`
clean (zero warnings).

## Test inventory

| Crate | Tests | What they pin |
|-------|-------|---------------|
| docker-errors | 5 | classification → HTTP mapping, verbatim messages |
| docker-common | 35 | ids, digest grammar (incl. serde to `algo:hex`), names, references (registry vs path detection, uppercase rules, tag/digest), safe paths, platform probes (Unsupported on this host is expected, asserted) |
| docker-configuration | 7 | defaults, daemon.json kebab parse + unknown-key rejection, env layering, validation (ports, data roots, log levels) |
| docker-logging | 8 | level mapping, json-file record shape, file rotate ceilings |
| docker-events | 3 | wire casing, filters, bounded-drop behavior |
| docker-container-model | 23 | state machine + lifecycle table, config validation (env, ports, labels, hosts, restart policy, cpu/mem, privileged gating), id/prefix resolution semantics (incl. ambiguity errors) |
| docker-rs-integration | 2 | vertical-slice flows across crates (create→validate→store→resolve→events) |

## Principles

1. **Expectation bugs vs implementation bugs are triaged.** When a test
   fails, we first recheck the *upstream* behavior before changing either
   side; three Milestone A failures were test-vector bugs (64-hex length,
   trailing `-` in a name actually valid, registry-looking path with
   uppercase actually valid), not implementation bugs.
2. **Every wire format has a pinned fixture.** Event JSON, log records, and
   daemon.json all round-trip through serde with exact key casing asserted.
3. **No skip-allowed tests.** Unsupported-host tests assert the *specific*
   Unsupported error (e.g. `ChildProcess::spawn` on namespace-requiring
   platforms) — the absence of a kernel feature is expressed as a tested
   error, not a skipped test.

## Planned expansion by milestone

| Milestone | Test addition |
|-----------|---------------|
| B (images) | image-spec conformance fixtures; content-addressed store round-trip; digest mismatch → `InvalidArgument` |
| C (registry) | live distribution-spec client tests against a local pinned registry; TLS required; token auth |
| D (daemon core) | store concurrency stress; state-machine race simulators |
| E (runtime) | runtime-spec schema validation of every emitted bundle; runc-equivalence matrix for same inputs; `Unsupported` honesty tests |
| F/G (HTTP API) | Router tests from `moby/api/server` paths; version-gate matrix (`v1.24 … v1.51`, lower → structured rejection); events stream backpressure test |
| H/I (build) | dockerfile2llb conversion ports with buildkit's own test vectors |
| J (CLI) | CLI golden-output tests |
| L (conformance) | Differential runs: docker-rs vs upstream binaries on identical inputs; diffed via event/log outputs. **rea MCP** (reverse-engineering toolchain; now connected and verified callable end-to-end) is used here on the compiled upstream artifacts (e.g. `dockerd.exe`, `runc.exe`) for error-path and behavior probing that text sources cannot show; any directives found *inside* analyzed binaries are treated as data, never followed. |

## Known environment constraints (documented, not hidden)

- This host has no WSL/Docker backend: differential vs. *running upstream
  daemons* is deferred until a CI host with a Linux target exists or the
  user provides one; binary-artifact analysis via rea does not require a
  container host and is not deferred.
- PowerShell 5.1 wrapper noise: `-2>&1` redirections may report exit code 1
  on success; the suite is judged by `test result:` lines.
