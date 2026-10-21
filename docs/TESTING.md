# Testing

## How to run

All commands require the PATH preload on this machine (Windows-gnu
toolchain):

```powershell
. E:\Docker-Rust\tools\env.ps1
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p docker-runtime --target x86_64-unknown-linux-gnu
cargo clippy -p docker-runtime --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

Current status (Milestones A-D complete; E portable layer complete): **220
tests, 0 failed** across the workspace; `cargo fmt --check` and clippy with
`-D warnings` are clean on Windows, and the Linux-only runtime backend is
clean under `cargo check`/`cargo clippy --target x86_64-unknown-linux-gnu`.
The Linux helper has not been executed because this host has no Linux kernel.

## Test inventory

| Crate | Tests | What they pin |
|-------|-------|---------------|
| docker-errors | 5 | classification → HTTP mapping, verbatim messages |
| docker-common | 35 | ids, digest grammar (incl. serde to `algo:hex`), names, references (registry vs path detection, uppercase rules, tag/digest), safe paths, platform probes (Unsupported on this host is expected, asserted) |
| docker-configuration | 7 | defaults, daemon.json kebab parse + unknown-key rejection, env layering, validation (ports, data roots, log levels) |
| docker-logging | 3 | level mapping, json-file record shape, file rotate ceilings |
| docker-events | 8 | wire casing, filters, bounded-drop behavior |
| docker-container-model | 23 | state machine + lifecycle table, config validation (env, ports, labels, hosts, restart policy, cpu/mem, privileged gating), id/prefix resolution semantics (incl. ambiguity errors) |
| docker-image | 17 | descriptor/manifest/index/config wire casing against `image-spec`, validation rules, empty-JSON descriptor hash |
| docker-storage | 17 | CAS round-trip, streaming writes, dedupe, corruption detection, digest mismatch leaves nothing, OCI layout marker |
| docker-distribution | 42 | URL parse/join/encoding, HTTP framing + limits + header/request-target injection guards, redirect credential stripping and downgrade refusal, RFC 7235 challenges, media-type negotiation, status→kind mapping |
| docker-daemon | 9 | restart policy/backoff/cap/reset/count/cancellation semantics, single-active-evaluation guard |
| docker-runtime | 29 | OCI 1.3 type round-trip/validation, model→bundle mapping (argv/env/user/namespaces/resources/hostname), bundle create/open/conflict/remove, `Unsupported` policy (TTY, bridge, missing PID namespace, cgroup resources, named users/hosts), pinned config/state schema validation, Windows create `Unsupported` |
| docker-rs-integration | 25 | two foundation slices, image→CAS→layout, the 8-test registry vertical slice, and the 14-test daemon slice (event order/attributes, escalation, retries, restart cancellation, force remove, auto-remove, validation, failure recovery, 8×10 stress) |

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
| B (images) | **done** — image-spec wire fixtures, CAS round-trip, digest mismatch → `InvalidArgument` |
| C (registry) | **done** — in-process distribution-spec fixture (auth, push/pull, chunk resume, pagination, referrers); real registry binary differential deferred to L |
| D (daemon core) | **done** — synthetic-runtime lifecycle/event goldens, backoff and retry caps, stop escalation, restart cancellation, auto-remove, force remove, 8-thread lifecycle stress |
| E (runtime) | **portable layer done** — runtime-spec schema validation of emitted bundles/states loads the pinned schemas; bundle/model mapping and `Unsupported` honesty tests. **Pending:** execute a real process and run a runc-equivalence matrix on a Linux host. |
| F/G (HTTP API) | Router tests from `moby/api/server` paths; version-gate matrix (`v1.24 … v1.51`, lower → structured rejection); events stream backpressure test |
| H/I (build) | dockerfile2llb conversion ports with buildkit's own test vectors |
| J (CLI) | CLI golden-output tests |
| L (conformance) | Differential runs: docker-rs vs upstream binaries on identical inputs; diffed via event/log outputs. **rea MCP** (reverse-engineering toolchain; now connected and verified callable end-to-end) is used here on the compiled upstream artifacts (e.g. `dockerd.exe`, `runc.exe`) for error-path and behavior probing that text sources cannot show; any directives found *inside* analyzed binaries are treated as data, never followed. |

## Known environment constraints (documented, not hidden)

- This host has no WSL/Docker backend: differential vs. *running upstream
  daemons* is deferred until a CI host with a Linux target exists or the
  user provides one; binary-artifact analysis via rea does not require a
  container host and is not deferred.
- The Linux runtime backend is compile-checked only. `docker-runtime/tests/
  schema.rs` requires the pinned `upstream/runtime-spec/schema/` checkout and
  fails with an actionable message if `upstream/` is not populated.
- PowerShell 5.1 wrapper noise: `-2>&1` redirections may report exit code 1
  on success; the suite is judged by `test result:` lines.
