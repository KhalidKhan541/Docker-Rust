# docker-rs

A Rust-native reimplementation of the Docker container ecosystem, built by
reverse-engineering the original Go/C implementations from their official
repositories. The long-term goal is to replace Moby (Docker Engine), the
Docker CLI, containerd, runc, and BuildKit with a modular Rust workspace
that preserves compatibility with Docker Engine API semantics and the OCI
specifications.

**Project status: Milestones A—E complete; the native Linux runtime executes
real containers in materialized image rootfs on `ubuntu-latest` CI (this
Windows host cannot run it locally); Milestone F/G has a runnable
Docker-compatible container-lifecycle, image-management, image-save/load, and
logs HTTP API with real registry pulls, persistent image metadata, and
container records/logs that survive daemon restarts. Attach, exec, and process
re-attachment across a daemon restart are still pending — see
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

* **A — Foundation** *(implemented)*: workspace, shared error taxonomy
  (moby `errdefs` equivalent), typed IDs/references/digests, platform
  capability probing, configuration layering, structured logging, event
  model + broker, container model + lifecycle state machine.
* **B — Image & content store** *(implemented)*: OCI image-spec v1.1.1
  types (descriptor/config/manifest/index/layout), content-addressed blob
  store with verified writes, OCI layout export.
* **C — Distribution client** *(implemented)*: distribution-spec v1.1.1
  pull/push (monolithic + chunked with resume), bearer/basic auth, manifest
  content negotiation, tag pagination, referrers with tag-schema fallback,
  classified registry errors.
* **D — Daemon orchestration core** *(implemented)*: single-owner command
  actor; create/start/stop/kill/restart/remove lifecycle; per-task monitor
  threads; Moby restart-manager backoff and policy semantics; deferred
  replies; auto-remove; `create`/`start`/`kill`/`die`/`stop`/`restart`/
  `destroy` events; deterministic `SyntheticRuntime` for headless tests;
  versioned container records and append-only log files with restart recovery.
* **E — OCI runtime** *(complete; execution verified on Linux CI)*:
  runtime-spec 1.3.0 types, model-to-bundle generation, pinned-schema
  validation, shared runtime/task/stdio boundary, explicit Windows
  `Unsupported`, and a Linux two-stage init helper (both stages tied to the
  daemon's lifetime with a parent-death signal). CI runs a real static
  process through unshare/chroot/`/proc`/identity drop in a materialized
  image rootfs.
* **F/G — Engine HTTP API** *(container + image-management + archive + logs +
  persistence slices implemented)*: Docker API `1.24`–`1.51` gate,
  `/_ping`, `/version`, `/info`, container create/list/inspect/start/stop/
  restart/kill/wait/delete, image list/pull/inspect/history/tag/delete,
  image save/load (`/images/get`, `/images/{name}/get`, `/images/load`),
  streaming `/events` with replay, `/containers/{id}/logs`, and a runnable
  `docker-rsd` binary with a persistent image and container-record/log store.
  Real registry pulls (multi-platform manifests, verified layers), Docker Hub,
  Docker save/load archives, and the real Docker CLI v29.9.1 have been
  exercised. It defaults
  to the synthetic runtime; `--runtime oci` materializes image rootfs and
  runs real containers (verified on `ubuntu-latest` CI). Container records,
  logs, and restart policies survive a daemon restart; a lost process is
  reported exited (255) and only restarted when its policy says so.
* **H/I — Builder**, **J — CLI**, **K — Security hardening**,
  **L — Conformance & differential**: planned. See `docs/ROADMAP.md`.

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
cargo check -p docker-runtime --target x86_64-unknown-linux-gnu  # Linux backend compile
```

## Run the Engine API

```bash
cargo run -p docker-rsd -- --host 127.0.0.1:2375 --data-root /var/lib/docker-rs
```

On Linux, add `--runtime oci` (alias `native`) to execute real containers: the
daemon materializes each image into `<data-root>/containers/<id>/rootfs` and
runs the container through the built-in init helper. The native runtime
requires root (rootless/user namespaces are not implemented yet) and a
`docker-runtime-init` binary next to `docker-rsd`; `cargo build --workspace`
produces both.

`--data-root` (or `DRC_DATA_ROOT`) selects the persistent image and container
root: image metadata/blobs under `images/`, and one directory per container
under `containers/<id>/` holding `container.json`, `logs.jsonl`, and (for the
native runtime) `rootfs/`. On this Windows host the built-in default is
`C:\ProgramData\docker-rs`, which normally needs administrator rights, so pass
a user-writable path for development.

In another terminal, point the official Docker CLI at the daemon:

```bash
export DOCKER_HOST=tcp://127.0.0.1:2375    # PowerShell: $env:DOCKER_HOST=...
docker version
docker pull --platform linux/amd64 alpine:3.20
docker images
docker save -o alpine.tar alpine:3.20
docker load -i alpine.tar
docker run -d --name demo alpine:3.20 sleep 100
docker ps
docker inspect demo
docker stop demo
docker rm demo
docker rmi alpine:3.20
```

Pulls default to the daemon host platform (`windows/amd64` on this host), so
Linux images need an explicit `--platform linux/amd64`; a missing platform is
reported honestly instead of silently selecting another architecture.

The daemon defaults to the **synthetic runtime**: container lifecycle state,
the API, and events are real, but no host process is executed. On Linux,
`--runtime oci` executes real containers and captures their stdout/stderr for
`docker logs`; attach/exec, TTY, and bridge networking remain explicit
`Unsupported`, so foreground `docker run` (which needs attach) is not
available yet. Container records and logs survive a daemon restart. A process
that was running when the daemon stopped cannot be adopted, so it is reported
exited with code 255; the helper dies with the daemon (taking its PID namespace
with it), and containers with `--restart always`, `unless-stopped`, or
`on-failure` are started again with a fresh process. Do not expose the
unauthenticated listener to an untrusted network. The runnable surface and
remaining gaps are listed in
[docs/API_COMPATIBILITY.md](docs/API_COMPATIBILITY.md).

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
  image/              OCI image-spec types (descriptor, config, manifest, index)
  storage/            content-addressed blob store + OCI layout marker
  distribution/       distribution-spec v1.1.1 registry client (pull/push/auth)
  runtime/            OCI 1.3 spec types, bundle builder, runtime/task boundary,
                      Linux two-stage init, non-Linux Unsupported backend
  daemon/             single-owner lifecycle actor, restart manager, runtime
                      boundary, persisted records/logs, synthetic runtime
  image-store/        persistent image index + verified-CAS blobs + registry pulls
                      + Docker/OCI save-load archives
  api/                Docker Engine HTTP API (version gate, DTOs, routes, events)
  daemon-bin/         `docker-rsd` runnable Engine API daemon
tests/
  integration/        cross-crate integration tests (A–D vertical slices)
docs/                 reverse-engineering + design + status documents
upstream/             pinned original repositories (not tracked in git)
```

## Runtime requirements

* accomplishing Milestones A–D needs none beyond the toolchain (the registry
  client talks HTTP/1.1 + TLS over `std::net`; Milestone D is exercised with
  the in-process synthetic runtime and does not execute containers);
* the F/G container API (`docker-rsd`) needs only the toolchain and a free
  TCP port; it defaults to the synthetic runtime and `--runtime oci` runs
  real Linux containers;
* Milestone E native execution requires a Linux host with namespaces, mount
  privileges, and root (rootless/user namespaces are not implemented yet).
  It is verified on `ubuntu-latest` CI; this Windows host has no Linux
  kernel/WSL/VM to run it locally.

## Security limitations

Containers are not a complete security boundary; see
`docs/SECURITY_ANALYSIS.md` for the threat model inherited from the OCI
stack and the explicit gaps. In particular, the current Linux runtime does
not yet drop capabilities or apply seccomp/LSM policy, and bridge networking,
cgroups, TTY, and user namespaces return `Unsupported`. Do not treat
Milestone E as hardened isolation (Milestone K). The F/G API listener is
currently plain TCP with no authentication or TLS; keep it on loopback.

## License

MIT. Upstream sources remain under their own licenses (Apache-2.0 / BSD /
CC-BY depending on the project) and are preserved unmodified in
`upstream/`.
