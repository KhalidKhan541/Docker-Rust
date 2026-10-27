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

Current status (Milestones A—E complete; F/G container + image-management +
image-archive + logs + persistence + console slices runnable): **316 tests,
0 failed** across the workspace on Windows; `cargo fmt --check` and clippy with
`-D warnings` are clean, and the Linux-only runtime backend is clean under
`cargo check`/`cargo clippy --target x86_64-unknown-linux-gnu`. The Linux
backend is executed by CI (`.github/workflows/linux-native.yml`):
`native_runtime` runs a static probe inside a materialized rootfs,
`tests/linux-native-smoke.sh` drives the Engine API with
`docker-rsd --runtime oci`, and `tests/static-console-smoke.sh` verifies the
same-origin console over a synthetic daemon. The Linux smoke test then starts
a long-running container as uid/gid 1000, SIGKILLs the daemon, asserts the
container process is gone (parent-death signal re-armed after the credential
change), restarts on the same data root, and verifies the exited record and
replayed logs. Manual smoke tests with Docker CLI v29.9.1 and Docker Hub (not
part of the automated suite) exercised pulls, images, inspect/tag/delete,
container lifecycle, and image save/load.

Daemon persistence is covered by unit and integration tests: container
records round-trip through `container.json` and survive a spawn/shutdown/spawn
cycle; lost running containers recover as exited (255); restart-policy
containers start again; captured log chunks round-trip base64 JSONL
(including non-UTF-8 bytes), replay into a bounded ring, ignore a torn
trailing record, and are deleted with the container directory. The Linux
smoke test additionally proves that a daemon SIGKILL tears down the running
container's PID namespace (parent-death signal) before recovery.

## Test inventory

| Crate | Tests | What they pin |
|-------|-------|---------------|
| docker-errors | 5 | classification → HTTP mapping, verbatim messages |
| docker-common | 35 | ids, digest grammar (incl. serde to `algo:hex`), names, references (registry vs path detection, uppercase rules, tag/digest), safe paths, platform probes (Unsupported on this host is expected, asserted) |
| docker-configuration | 7 | defaults, daemon.json kebab parse + unknown-key rejection, env layering, validation (ports, data roots, log levels) |
| docker-logging | 3 | level mapping, json-file record shape, file rotate ceilings |
| docker-events | 10 | wire casing (incl. lowercase action), filters, bounded-drop behavior, bounded replay history, atomic replay-plus-subscribe |
| docker-container-model | 23 | state machine + lifecycle table, config validation (env, ports, labels, hosts, restart policy, cpu/mem, privileged gating), id/prefix resolution semantics (incl. ambiguity errors) |
| docker-image | 17 | descriptor/manifest/index/config wire casing against `image-spec`, validation rules, empty-JSON descriptor hash |
| docker-storage | 17 | CAS round-trip, streaming writes, dedupe, corruption detection, digest mismatch leaves nothing, OCI layout marker |
| docker-distribution | 42 | URL parse/join/encoding, HTTP framing + limits + header/request-target injection guards, redirect credential stripping and downgrade refusal, RFC 7235 challenges, media-type negotiation, status→kind mapping |
| docker-daemon | 25 | 21 unit: restart policy/backoff/cap/reset/count/cancellation semantics and single-active-evaluation guard (9), versioned record round-trip/atomic replace/corrupt and ID-mismatch rejection/lost-running recovery (6), log file round-trip incl. non-UTF-8, bounded replay, torn-line tolerance, close/reopen (6); 4 persistence integration tests: record and log survival across spawn/shutdown/spawn, directory removal, exited-255 recovery, and restart-policy restart |
| docker-image-store | 28 | persistent index/tag/ID/prefix/digest resolution, tag moves, conservative delete + blob GC, reference canonicalization/familiarization, platform selection, loopback detection, registry auth parsing, pull descriptor verification, OCI/legacy save-archive export, legacy/gzip load, GNU-tar root-entry tolerance, tag restoration, diffID verification, and path-traversal rejection |
| docker-api | 45 | 35 unit: API version parse/gate/messages and path prefixes; query booleans/timestamps/filters; DTO casing, image-default inheritance, and rejection of unsupported runtime fields; signal parsing; event replay-cutoff rule; static-file traversal/encoded-traversal/dotfile/backslash/device-name/symlink-escape rejection and MIME mapping. 10 real-loopback HTTP tests: system endpoints, CRUD/lifecycle/304/wait, generated names/short ids/filters/501s, event replay/live streaming, image CRUD/history/tag/delete, image create validation/defaults, image save/load round-trip (OCI + legacy, gzip, platform filters), a real registry pull against a loopback OCI fixture, and static console serving with API precedence, clean URLs, MIME, 404/405/HEAD |
| docker-rsd | 5 | CLI option parsing (inline/separate values, help/version, `--data-root`, `--web-root`), unknown/incomplete flag rejection, TCP-only listen-address handling including explicit named-pipe `Unsupported` |
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
| E (runtime) | **done** — runtime-spec schema validation of emitted bundles/states loads the pinned schemas; bundle/model mapping and `Unsupported` honesty tests; two `ubuntu-latest` smoke steps execute a real process in a materialized rootfs and the same probe through the Engine API. runc-equivalence matrix remains for L. |
| F/G (HTTP API) | **container + image-management + image-archive + logs + console slices done** — real loopback HTTP tests for the version-gate matrix (`v1.24`–`v1.51`, lower → structured rejection), headers, CRUD/lifecycle/304/wait, filters, image list/inspect/history/tag/delete, create-time image resolution/defaults, save/load round-trip (`/images/get`, `/images/{name}/get`, `/images/load`), container logs (stdcopy framing, stream filters, follow/tail), unsupported endpoints, event replay/live streaming, and a verified pull from an in-process OCI registry fixture; static-console unit tests plus loopback HTTP coverage of API precedence, reserved namespaces, version prefixes, clean URLs, MIME, 404/405/HEAD, traversal, and symlink escapes; `tests/static-console-smoke.sh` on Linux; manual Docker CLI v29.9.1 save/load and Docker Hub smoke; `ubuntu-latest` native smoke through `docker-rsd --runtime oci`. **Pending:** attach/exec, push/prune, and upstream router fixtures. |
| H/I (build) | dockerfile2llb conversion ports with buildkit's own test vectors |
| J (CLI) | CLI golden-output tests |
| L (conformance) | Differential runs: docker-rs vs upstream binaries on identical inputs; diffed via event/log outputs. **rea MCP** (reverse-engineering toolchain; now connected and verified callable end-to-end) is used here on the compiled upstream artifacts (e.g. `dockerd.exe`, `runc.exe`) for error-path and behavior probing that text sources cannot show; any directives found *inside* analyzed binaries are treated as data, never followed. |

## Known environment constraints (documented, not hidden)

- This Windows host has no WSL/Docker backend, so the Linux runtime cannot run
  locally; CI's `ubuntu-latest` job executes it (root runtime smoke test plus
  the Engine API smoke), and failures are published as check-run annotations.
  Differential runs against *running upstream daemons* remain deferred;
  binary-artifact analysis via rea does not require a container host.
- `docker-runtime/tests/schema.rs` requires the pinned
  `upstream/runtime-spec/schema/` checkout and fails with an actionable
  message if `upstream/` is not populated (CI fetches it at the pinned
  revision).
- PowerShell 5.1 wrapper noise: `-2>&1` redirections may report exit code 1
  on success; the suite is judged by `test result:` lines.
