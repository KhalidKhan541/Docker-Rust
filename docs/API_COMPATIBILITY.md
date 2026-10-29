# API Compatibility

Compatibility contract with the Docker Engine HTTP API, grounded in the
pinned `moby` sources.

## Version gates (verified in source)

`moby/api/common.go`:

```go
DefaultVersion         = "1.51"
MinSupportedAPIVersion = "1.24"
```

- `moby/api/server/server.go`: requests with a version below the configured
  minimum are rejected (`versions.LessThan(v, "1.24")` path).
- `moby/api/server/middleware/version.go`: response header `Api-Version` is
  set from the default version; a client requesting a **newer** version than
  the server supports receives a `versionUnsupportedError`, not newer
  behavior.

docker-rs implements the identical gate: it accepts `v1.24` through `v1.51`,
rejects lower/unknown versions with a structured error, and sets
`Api-Version` on every response.

## Wire shapes already implemented and pinned by tests

| Surface | Upstream anchor | docker-rs |
|---------|-----------------|-----------|
| Event objects | `moby/daemon/events/events.go` | `docker-events::Event` — brownfield casing `type`, `action`, `scope`, `time`, `timeNano`, actor `ID` uppercase |
| json-file logs | `moby/daemon/logger/jsonfilelog/jsonfilelog.go` | `docker-logging::JsonLinesDriver` — `{"log","stream","time"}` per line |
| daemon.json | daemon `config.go` | `docker-configuration::FileConfig` kebab-case, deny_unknown_fields |
| error payloads | `moby/errdefs/http_helpers.go` | `docker-errors::http_status()`; message text returned verbatim (no `status: ` prefix) |

## Implemented HTTP surface (Milestone F/G, container + image slice)

Every response (success or error) carries `Api-Version: 1.51`. Requests may
use an unversioned path or a `v1.24`–`v1.51` prefix; the prefix is validated
before routing. Errors use Moby's `{"message":"..."}` envelope with the
`errdefs` status mapping.

| Method | Path | Status |
|--------|------|--------|
| `GET`, `HEAD` | `/_ping` | implemented; `OK`, `Server`, `OSType`, `Swarm: inactive` |
| `GET` | `/version` | implemented; advertises API `1.51`, minimum `1.24` |
| `GET` | `/info` | implemented from live actor/image state; image count and connectivity are real, unsupported counters are zero, and the synthetic-runtime warning is explicit |
| `GET` | `/events` | implemented; history replay + live stream, `since`/`until`, type/container/event/label/scope filters |
| `GET` | `/containers/json` | implemented; `all`, `limit`, status/id/name/label filters |
| `POST` | `/containers/create` | implemented for the supported Config/HostConfig subset; optional `name`; generated name when absent |
| `GET` | `/containers/{id}/json` | implemented; full id, unique prefix, or name |
| `POST` | `/containers/{id}/start` | implemented; `204`, or Moby's `304` no-op |
| `POST` | `/containers/{id}/stop` | implemented; `t` timeout, `204`, or `304` when already stopped |
| `POST` | `/containers/{id}/restart` | implemented; `t` timeout |
| `POST` | `/containers/{id}/kill` | implemented; signal name or number |
| `POST`, `GET` | `/containers/{id}/wait` | implemented for `not-running` (default), `next-exit`, and `removed`; returns `StatusCode`; actor-side waiters, no polling |
| `GET` | `/containers/{id}/logs` | implemented; stdcopy frames, `follow`, `timestamps`, `tail`, `since`/`until`, stdout/stderr filters; `details` returns 501 |
| `POST` | `/containers/{id}/attach` | implemented output-only; stdcopy frames over `101` upgrade or chunked `200`; `stream`, `logs`, stdout/stderr handled; `stdin=1` returns 501 |
| `POST` | `/containers/{id}/exec` | implemented; validates numeric `UID[:GID]`; TTY, stdin, and privileged mode return 501 |
| `POST` | `/exec/{id}/start` | implemented; non-TTY/non-interactive; `101` multiplexed upgrade or chunked `200`; detached start returns 200 |
| `GET` | `/exec/{id}/json` | implemented; `Running`, `ExitCode`, `Pid`, and process config |
| `POST` | `/exec/{id}/resize` | returns 501; resize requires TTY support |
| `DELETE` | `/containers/{id}` | implemented; `force` and `v` query parameters |
| `GET` | `/images/json` | implemented; `all`; empty `filters` only (non-empty filters return 501) |
| `POST` | `/images/create` | implemented for registry pulls (`fromImage`, `tag`, `platform`, `X-Registry-Auth` username/password); `fromSrc` and `repo`/`message` return 501 |
| `GET` | `/images/{name}/json` | implemented; tag, repository digest, full/unique ID prefix; `manifests` and non-empty `platform` return 501 |
| `GET` | `/images/{name}/history` | implemented from image history metadata; non-empty `platform` returns 501 |
| `POST` | `/images/{name}/tag` | implemented; `repo` and optional `tag` |
| `DELETE` | `/images/{name}` | implemented; `force`; untag + conservative blob GC |
| `GET` | `/images/get?names=...` | implemented; OCI layout + legacy `manifest.json`, optional `platform` |
| `GET` | `/images/{name}/get` | implemented; same archive writer as `/images/get` |
| `POST` | `/images/load` | implemented; OCI or legacy Docker archive, plain/gzip tar, optional `platform`, newline-delimited JSON status stream |

Short-id and name resolution happen in the single-owner container store, so
ambiguous prefixes produce the upstream-style conflict instead of selecting
one arbitrarily.

### Create compatibility

The wire decoder accepts the field set emitted by the pinned Docker CLI
(v29.9.1), including explicit zero values such as `MemorySwappiness: -1`,
`PidsLimit: -1`, `ConsoleSize: [0,0]`, an empty `NetworkingConfig`, and empty
log/port/mount maps. Non-zero values for features the runtime cannot honor
are rejected with `Unsupported` (HTTP 501) rather than silently ignored.
Examples: bind mounts, port bindings, privileged mode, extra hosts, TTY,
non-default logging drivers, seccomp/AppArmor options, and resource limits.

When an image store is attached (the normal `docker-rsd` path), create
resolves the image before touching the container store. A missing image
returns Moby's `No such image` 404 instead of creating a container that
cannot ever run. The resolved image's `Cmd`/`Entrypoint` (when the request
left them empty), environment, labels, working directory, user, exposed
ports, and stop signal become the container defaults; explicit container
values win. `Env` replacement follows `docker run -e` key semantics rather
than appending a duplicate key.

### Attach, exec, and wait

- **Attach is output-only.** `stdout`/`stderr` select the streams, `logs=1`
  replays the retained backlog before following, and `stream=0` returns the
  available backlog and closes. A request carrying `Upgrade` receives `101`
  and a raw multiplexed body; other clients receive a chunked `200` body with
  `Content-Type: application/vnd.docker.multiplexed-stream`. A streaming
  attach to a stopped container stays open until the next start/exit or
  removal, which is what `docker start -a` relies on. The Docker CLI's
  `detachKeys` value is accepted but unused. `stdin=1` is rejected explicitly
  because stdin transport is not implemented; TTY containers cannot be
  created.
- **Exec is non-interactive.** `POST /containers/{id}/exec` accepts
  `User`, `Env` (including Go's `null`), `WorkingDir`, `Cmd`, `AttachStdout`,
  `AttachStderr`, and configured `DetachKeys`; `Tty`, `AttachStdin`, and
  `Privileged` return explicit `501` errors. Numeric `UID[:GID]` values are
  accepted and validated at create and start. A missing GID is resolved from
  the container's `/etc/passwd`, and supplementary groups from `/etc/group`;
  named users still return `501` because name lookup is not implemented. A
  non-detached start streams output with stdcopy framing; a detached start
  returns `200` immediately. Exec records are in-memory and are deleted with
  their container.
- **Exec exit ordering.** The exec monitor publishes the exit status to the
  actor and waits for acknowledgement before releasing its log-writer lease.
  Consequently, the attach stream reaches EOF only after `GET /exec/{id}/json`
  can report the final `ExitCode`; the Docker CLI's attach-then-inspect flow
  therefore works without a race.
- **Wait conditions.** `not-running` returns immediately when the state is not
  alive (`created`, `exited`, `dead`, or `removing`); `restarting` counts as
  alive, so the waiter stays blocked across a restart, matching Moby.
  `next-exit` never replays a previous exit: it blocks on `created`, `exited`,
  `dead`, and every running/restarting state until the next process exit
  (including exits followed by restart-policy replacements). A restart or
  forced-stop failure that can no longer produce an exit wakes the waiter
  with the last status. `removed` waits for container removal and returns the
  last known exit status. Waiters live in the actor and are woken by state
  transitions, not polling. Registration completes before the HTTP response
  starts, so the response headers are sent immediately (Moby's post-1.30
  behavior) while the JSON body remains open. This is required by the Docker
  CLI, whose `ContainerWait` call waits for headers before it starts the
  container. The foreground `docker run` and `docker start -a` flows use
  `next-exit` and receive the new process's exit status.

Smoke-tested against the real static Docker CLI: `docker version`, `docker
info`, `docker pull`, `docker images`, `docker image inspect`, `docker tag`,
`docker rmi`, `docker create`, `docker run -d`, `docker ps`, `docker
inspect`, `docker start`, `docker stop`, `docker kill`, `docker rm`, `docker
wait`, and `docker events` all drive the daemon over TCP. On Linux CI, the
same CLI additionally exercises foreground `docker run` attach and
`docker exec`, including `--user 1000:1000`, multiplexed output, and `/exit7`
status 7. Interactive stdin, TTY, resize, and privileged exec remain
explicitly unsupported.

### Image pulls

`POST /images/create` uses the distribution client, not a shell-out to
Docker. It resolves multi-platform indexes, downloads the selected manifest,
config, and every layer into the verified CAS, checks descriptor sizes and
digests, and records the image by config digest. The default platform is the
daemon host (`windows/amd64` here), so pulling Linux images on this host
requires an explicit `platform=linux/amd64`; the failure names the available
platforms instead of silently picking one. Loopback registries are allowed
over plain HTTP; every other registry requires TLS. Anonymous pulls and
Basic username/password pulls are supported. `identitytoken`/`registrytoken`
credentials and tag-plus-digest references return `Unsupported`.

The image index is `<data-root>/images/images.json`; blobs live in the
`docker-storage` CAS below `<data-root>/images/blobs`. Records survive a
`docker-rsd` restart. Pull progress is currently buffered into a small final
status stream instead of being relayed live layer by layer.

### Image save and load

`GET /images/get` and `GET /images/{name}/get` write the archive format
produced by current Moby: an OCI layout (`oci-layout`, `index.json`,
`blobs/sha256/...`) plus the legacy `manifest.json` compatibility file.
Layer blobs are copied byte-for-byte from the verified CAS, so compressed
layers keep their original digests and Moby's loader can decompress them.
`index.json` descriptors are annotated with the canonical image name and tag;
`manifest.json` carries familiar `RepoTags`, the config path, and layer paths.
Repeat the `names` query parameter to export multiple images. A tag, image ID,
or digest selects one image; a bare repository exports every matching tag.
No names returns HTTP 400.

`POST /images/load` accepts either the OCI-layout archive or a legacy Docker
archive, as an uncompressed tar or a gzip-compressed tar. It validates every
manifest/config/layer digest and rootfs diffID before committing blobs, then
restores tags, IDs, sizes, labels, and platform metadata. Success responses are
newline-delimited `{"stream":"Loaded image: ..."}` messages (or `Loaded image
ID:` for an untagged image). Archives are spooled to a temporary file; request
bodies are capped at 64 GiB. Extraction rejects absolute paths, `..`, symlinks,
hardlinks, and device nodes, and is bounded to 100,000 entries and 64 GiB.
Zstd-compressed archives return an explicit `Unsupported` error because no zstd
decoder is linked yet.

`platform` is an OCI platform JSON object (for example
`{"architecture":"amd64","os":"linux"}`), not `os/arch`. Save rejects a
platform that does not match the selected image; load skips manifests that do
not match and fails if none remain. Repeating `platform` returns HTTP 400.
Save is spooled before the response and is not a true live stream yet. Neither
path extracts a rootfs; layers remain verified archive blobs.

Manual smoke testing with Docker CLI v29.9.1 exercised `docker save` (single
and multiple names), `docker load`, and gzip-compressed archives against
`docker-rsd`.

### Events

`EventBroker` keeps a bounded replay window (4096 events by default). The
history snapshot and subscriber registration are performed under one lock,
so an event published during `subscribe_since` is either replayed or
delivered live, never lost. `/events` writes newline-delimited JSON with
Moby's lowercase `type`/`action`/`scope`, camelCase `timeNano`, and uppercase
actor `ID`. A stream with neither `since` nor `until` starts at the current
position; `until` alone replays the buffered window up to that time, matching
Moby's zero-time rule. A slow subscriber is disconnected/dropped by the
broker; the stream uses chunked transfer encoding.

### Static console serving (docker-rs extension)

`docker-rsd --web-root <dir>` serves a local console or static site from the
same TCP listener as the Engine API. This is deliberately **not** a Docker
Engine endpoint and clients should not depend on it.

**Route precedence.** The static handler runs only after the API router has
declined a path. All `/vX.Y/...` paths and these top-level namespaces are
reserved for the API, even when the endpoint is unimplemented:

`/_ping`, `/auth`, `/build`, `/commit`, `/configs`, `/containers`,
`/distribution`, `/events`, `/exec`, `/images`, `/info`, `/networks`,
`/nodes`, `/plugins`, `/secrets`, `/services`, `/session`, `/swarm`,
`/system`, `/tasks`, `/version`, `/volumes`.

A file at `web/containers/json` can therefore never shadow
`GET /containers/json`; an unimplemented namespace such as `/networks` stays
an API 404 rather than serving a same-named page. Use a non-reserved path
(such as `/assets/...`) for static assets.

**Resolution.** Only `GET` and `HEAD` are served. `/` maps to `index.html`,
an extensionless path such as `/console` maps to `console.html`, and a
directory maps to its `index.html`. There is no directory listing or SPA
fallback. An existing page requested with another method returns 405 with
`Allow: GET, HEAD`; an unknown path returns the ordinary JSON 404.

**Safety.** The root is canonicalized at startup and must be a directory.
The URL path is percent-decoded exactly once; NUL/control characters,
backslashes, colons, `..`, dot-prefixed segments, trailing dots/spaces, and
Windows device names (`CON`, `NUL`, `COM1`, ...) are rejected. The resolved
path must canonicalize to a regular file under the canonical root, so
symlinks/junctions cannot escape it. Files larger than 64 MiB are rejected.
Responses get `Cache-Control: no-cache` and
`X-Content-Type-Options: nosniff`, plus a small extension-to-MIME map.

There is no CORS support and no authentication/TLS. The console is intended
to be opened from the daemon's own origin (for example
`http://127.0.0.1:2375/console.html`). For remote access, put an
authenticated TLS proxy in front of the daemon.

## Honest gaps in this slice

- **Image management is not complete.** Push, prune, build, manifest
  inspection, non-empty image filters, cross-platform inspect/history, and
  multi-tag deletion without `force` are not implemented and return
  `Unsupported`/`Conflict` rather than pretending to work. Save/load are
  implemented but spooled to temporary files instead of streaming directly.
  Pull progress is buffered, not streamed per layer.
- **Image rootfs materialization lives in `docker-rsd`.** Pull verifies and
  stores the manifest/config/layer blobs; `docker-rsd --runtime oci`
  materializes them into `<data-root>/containers/<id>/rootfs` through
  `ImageRootfsProvider` and executes the container. Library-level callers must
  supply a rootfs or provider, and the default runtime remains synthetic.
- **Image deletion does not check container use.** Container records do not
  yet track image references for GC, and the JSON index write is not atomic
  on Windows. Blob GC is deliberately conservative.
- **The daemon defaults to the synthetic runtime.** `docker-rsd` serves the
  synthetic runtime unless `--runtime oci` (alias `native`) is passed; that
  mode materializes image rootfs and runs real Linux containers. Synthetic
  lifecycle transitions and events are real, but host PIDs are zero and no
  container process runs.
- **No network plumbing.** `bridge`/`default` is accepted as the default
  metadata value, `host` and `none` are accepted, but no bridge, veth, DNS,
  port publishing, or named/container network setup occurs. Named and
  `container:` network modes are rejected at create.
- **Attach and exec are deliberately narrow.** Attach is output-only: stdin
  and TTY are rejected, and WebSocket upgrade is not implemented (the Docker
  CLI's HTTP `101` upgrade and chunked fallback are). Exec is non-interactive
  and non-TTY: no stdin, resize, or privileged mode; users must be numeric
  `UID[:GID]` (the GID and supplementary groups may be resolved from the
  container's `/etc/passwd` and `/etc/group`). Exec records live only in memory
  and are not restored after a daemon restart. No stats, top, changes, export,
  archive, pause/unpause, update, rename, or resize endpoints exist; unlisted
  container subresources return HTTP 501 with a `not implemented` message.
- **Wait requests have no per-client cancellation.** A client that disconnects
  while waiting for `next-exit` leaves an actor-side waiter until the next exit
  or daemon shutdown.
- **Persistence is partial.** Container records (config, host config, state),
  restart policies, and captured logs are stored under
  `<data-root>/containers/<id>/` and restored at daemon startup. A container
  that was running when the daemon stopped cannot be adopted, so it is
  reported exited with code 255; `always`, `unless-stopped`, and `on-failure`
  policies start a fresh process, while `no` leaves it exited. Restart counts
  and the manually-stopped flag reset on recovery. The log file is unbounded
  and only the newest records that fit the 4 MiB serving ring are replayed.
  The native init helper arms a parent-death SIGKILL, so an abruptly killed
  daemon also kills the helper and its container PID namespace; recovery does
  not leave a live process behind a record that says `exited`.
- **Transport is TCP only.** `npipe://` and `unix://` listeners are rejected
  explicitly by `docker-rsd`. Clients connect with
  `DOCKER_HOST=tcp://127.0.0.1:2375`.
- **Static serving is intentionally minimal.** `--web-root` supports
  GET/HEAD only, has no directory listing, SPA fallback, range requests,
  compression, cache validators, CORS, or auth. Docker API namespaces and
  `/vX.Y` prefixes are reserved and cannot be served from the web root. The
  web root is trusted content: dotfiles and traversal are blocked, but any
  other readable regular file under the canonical root can be downloaded.
- **No authentication or TLS.** The default listener binds loopback; do not
  expose it to an untrusted network.
- `/info` values that need image, network, cgroup, or kernel integrations are
  zero/empty placeholders rather than invented data.
- Swarm endpoints are intentionally out of scope. `/info` reports
  `Swarm.LocalNodeState=inactive`; `/version` does not advertise swarm
  capability keys.

## Upstream anchors for the implemented behavior

- Version gate: `moby/api/common.go` (`DefaultVersion = "1.51"`,
  `MinSupportedAPIVersion = "1.24"`) and
  `moby/api/server/middleware/version.go`.
- Start/stop `304` semantics: container start/stop handlers in
  `moby/daemon/start.go` / `moby/daemon/stop.go`.
- Wait conditions: `moby/api/types/container` and `moby/daemon/wait.go`.
- Attach upgrade and stdcopy framing: `moby/api/server/router/container/container_routes.go`,
  `moby/api/types/backend/backend.go`, and the pinned CLI's
  `cli/command/container/attach.go` / `run.go`.
- Exec create/start/inspect: `moby/api/types/container/exec.go`,
  `moby/daemon/exec.go`, and the pinned CLI's
  `cli/command/container/exec.go`.
- Event wire shape: `moby/daemon/events/events.go`.
- Filter vocabulary: `moby/api/types/filters` and daemon container list
  filters.
