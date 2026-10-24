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

docker-rs will implement the identical gate (milestone F): accept `v1.24`
through `v1.51`, reject lower/unknown with a structured error, and set
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
| `POST`, `GET` | `/containers/{id}/wait` | implemented for `not-running`; returns `StatusCode`; `next-exit`/`removed` return 501 |
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

Smoke-tested against the real static Docker CLI: `docker version`, `docker
info`, `docker pull`, `docker images`, `docker image inspect`, `docker tag`,
`docker rmi`, `docker create`, `docker run -d`, `docker ps`, `docker
inspect`, `docker start`, `docker stop`, `docker kill`, `docker rm`, `docker
wait`, and `docker events` all drive the daemon over TCP. `docker logs`,
`attach`, `exec`, and foreground `docker run` correctly fail with an
explicit `Unsupported` response until the runtime captures stdio.

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

## Honest gaps in this slice

- **Image management is not complete.** Push, prune, build, manifest
  inspection, non-empty image filters, cross-platform inspect/history, and
  multi-tag deletion without `force` are not implemented and return
  `Unsupported`/`Conflict` rather than pretending to work. Save/load are
  implemented but spooled to temporary files instead of streaming directly.
  Pull progress is buffered, not streamed per layer.
- **Images own no extracted rootfs yet.** Pull verifies and stores the
  manifest/config/layer blobs, but layers are not unpacked into a container
  rootfs. Container create therefore cannot run the image even if the
  native runtime were selected; `docker-rsd` still uses the synthetic
  runtime.
- **Image deletion does not check container use.** Container records do not
  yet track image references for GC, and the JSON index write is not atomic
  on Windows. Blob GC is deliberately conservative.
- **No process execution in the daemon binary.** `docker-rsd` wires the
  synthetic runtime: lifecycle transitions and events are real, host PIDs are
  zero, and no container process runs. The OCI backend exists and is
  compile-checked but has no image-rootfs provider connected.
- **No network plumbing.** `bridge`/`default` is accepted as the default
  metadata value, `host` and `none` are accepted, but no bridge, veth, DNS,
  port publishing, or named/container network setup occurs. Named and
  `container:` network modes are rejected at create.
- **No logs/attach/exec/stats/top/changes/export/archive/pause/update/rename/
  resize.** Container subresources not listed above return HTTP 501 with a
  `not implemented` message.
- **No persistence.** Container records live only in the daemon actor's
  memory; restarting `docker-rsd` loses them.
- **Transport is TCP only.** `npipe://` and `unix://` listeners are rejected
  explicitly by `docker-rsd`. Clients connect with
  `DOCKER_HOST=tcp://127.0.0.1:2375`.
- **No authentication or TLS.** The default listener binds loopback; do not
  expose it to an untrusted network.
- `/info` values that need image, network, cgroup, or kernel integrations are
  zero/empty placeholders rather than invented data.
- Hijacked upgrade streams
  (`application/vnd.docker.multiplexed-stream`) are unimplemented.
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
- Event wire shape: `moby/daemon/events/events.go`.
- Filter vocabulary: `moby/api/types/filters` and daemon container list
  filters.
