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

## HTTP endpoint plan (F/G) — endpoints, in dependency order

1. `/_ping`, `GET /version`, `GET /info` (self-describe; version gate first)
2. Container CRUD: `/containers/create`, `/containers/{id}/json` (inspect),
   `?filters=` for `/containers/json`, `/start`, `/stop`, `/restart`, `/kill`
   — all must accept **both** long ids and short prefixes resolved against
   the store (`resolve()` semantics).
3. `/events` (streaming; broker backpressure behavior documented in
   RUST_DESIGN.md) and `/logs` with the `follow`/`since`/`tail` matrix.
4. `/images/*` (distribution + content store hooks, milestone C).
5. `/build` (buildkit frontend, milestone H/I).
6. Exec + attach (hijacked streams) last — they need the runtime (E).

## Non-goals / honest gaps at this stage

- No HTTP is served yet; everything above is contract, not claim.
- Hijacked upgrade (`application/vnd.docker.multiplexed-stream`) semantics
  are specified but unimplemented.
- Swarm endpoints (`/swarm/*`) are explicitly out of scope long-term —
  docker-rs is a single-node engine. This will be stated in `/version`
  responses once served (`("" swarm)` feature keys omitted), so clients see
  a capability-honest v1.51 subset rather than broken 404s.
