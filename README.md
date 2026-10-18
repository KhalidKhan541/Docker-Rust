# docker-rs

A Rust-native reimplementation of the Docker container ecosystem, built by
reverse-engineering the original Go/C implementations from their official
repositories. The long-term goal is to replace Moby (Docker Engine), the
Docker CLI, containerd, runc, and BuildKit with a modular Rust workspace
that preserves compatibility with Docker Engine API semantics and the OCI
specifications.

**Project status: Milestone A (core foundation) — see
[docs/IMPLEMENTATION_STATUS.md](docs/IMPLEMENTATION_STATUS.md).**

## Motivation and scope

This is not an API wrapper and not an orchestration layer around the
existing Docker Engine. Each behavioral clone is implemented from the
pinned upstream source (see the revisions table below) and validated against
that source's behavior.

Upstream revisions used for reverse engineering (pinned, preserved in
`upstream/`, not executed):

| Upstream        | Tag     | Commit |
|-----------------|---------|--------|
| moby/moby       | v28.5.2 | `89c5e8fd66634b6128fc4c0e6f1236e2540e46e0` |
| docker/cli      | v29.9.1 | `dfc520dcc28994c4c699b9b219fd2f5a6bcec0ff` |
| containerd/containerd | v2.4.2 | `b7a25261e029d68978bbf290df0a62fc819b3c51` |
| opencontainers/runc | v1.5.2 | `29dd3dc2b13b4123162e5fe132504bb4b15569f1` |
| moby/buildkit   | v0.34.0 | `f5b59a1f2fb25a0cb7072c398cab9aed7835da4e` (also tagged dockerfile/1.28.0) |
| opencontainers/runtime-spec | v1.3.0 | `92249139eea7161e13745abd4cb6d0ea02a3227a` |
| opencontainers/image-spec | v1.1.1 | `147f9c13cedb47a0c4d9a11a222961073d585877` |
| opencontainers/distribution-spec | v1.1.1 | `a139cc423184af6078077b9b7ee336eddbd03f8f` |

Current milestone map:

* **A — Core foundation** *(implemented)*: workspace, shared error
  taxonomy (moby `errdefs` equivalent), typed IDs/references/digests,
  platform capability probing, configuration layering, structured logging,
  event model + broker, container model + lifecycle state machine,
  integration tests.
* **B — Container engine** *(next)*: Engine-API-shaped daemon endpoints,
  container manager, image manager.
* **C — Native runtime**: OCI runtime in Rust (libcontainer equivalent).
* **D — Lifecycle & resources**, **E — image/content**, **F — storage &
  snapshots**, **G — networking**, **H — CLI**, **I — build**, **J — Engine
  API conformance**, **K — security/isolation**, **L — differential
  testing**. See `docs/ROADMAP.md`.

## Supported operating systems

Development and tests currently run on **Windows 11 x64** using the
`x86_64-pc-windows-gnu` Rust toolchain with MSYS2/MinGW binutils fetched
into `tools/mingw64` (see below). The *runtime* functionality
(namespaces/cgroups/overlayfs) is Linux-only by kernel design; on hosts
without those facilities the capability probe reports precise `Unsupported`
errors instead of silently degrading (mission rule: never silently fall
back to a semantically weaker implementation).

## Prerequisites

* Rust stable (`rustup`, toolchain `stable-x86_64-pc-windows-gnu` on this
  host; any modern stable with Edition 2021 support works on other hosts)
* Cargo (bundled with the toolchain)
* Git
* On Windows-gnu hosts only: binutils on `PATH`. The bundled binutils in
  the rustup GNU toolchain need `as.exe`; this workspace fetches MSYS2's
  `mingw-w64-x86_64-binutils` (v2.47-4) + runtime DLL deps into
  `tools/mingw64` and prepends it to `PATH`.

## Build instructions

```bash
cargo build --workspace          # build all crates
cargo test --workspace           # unit + integration tests
cargo fmt --all -- --check       # formatting check
cargo clippy --workspace --all-targets -- -D warnings   # lints
```

## Repository layout

```
Cargo.toml            workspace manifest (resolver = "2", edition 2021)
crates/
  errors/             shared error taxonomy (moby errdefs equivalent)
  common/             IDs, names, image refs, digests, paths, platform, process
  configuration/      daemon config model + layered merge (daemon.json equiv)
  logging/            daemon tracing init + container log entry json-file driver
  events/             engine event model + pub/sub broker (events.Message equiv)
  container-model/    create-request config, lifecycle state machine, store
tests/
  integration/        cross-crate integration tests (foundation slice)
docs/                 reverse-engineering + design + status documents
upstream/             pinned original repositories (not tracked in git)
```

## Runtime requirements

* accomplishing Milestone A needs none beyond the toolchain;
* Milestone C+ (native runtime) requires Linux kernel ≥ 5.8 facilities
  (namespaces, cgroup v2, overlayfs, veth/bridge), or an explicitly selected
  VM/WSL backend once implemented.

## Security limitations

Containers are not a complete security boundary; see
`docs/SECURITY_ANALYSIS.md` for the threat model inherited from the OCI
stack and the explicit gaps (privileged mode refused, seccomp profiles,
socket permissioning — tracked per milestone).

## License

MIT. Upstream sources remain under their own licenses (Apache-2.0 / BSD /
CC-BY depending on the project) and are preserved unmodified in
`upstream/`.
