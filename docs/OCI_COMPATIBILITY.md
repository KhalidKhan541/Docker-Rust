# OCI Compatibility

docker-rs targets the three OCI specifications, matching the pinned upstream
revisions (root `README.md` pins table).

## runtime-spec v1.3.0 — pinned `92249139ee…`

Sources of truth harvested from `upstream/runtime-spec/`:

- `spec.md`, `runtime.md`, `config.md` (+ per-OS `config-linux.md`,
  `config-windows.md`, …) — normative text.
- `schema/config-schema.json` — the JSON-schema we will validate generated
  bundles against (also `state-schema.json`, `features-schema.json`).
- `specs-go/{config.go, state.go}` — the canonical Go type shapes; any Rust
  OCI types crate (`docker-oci-types`, Milestone E) must serialize
  field-for-field identically (`ociVersion`, `process.env`, `root.path`,
  `hooks.startContainer`, …).

Current status: primitive types handle spec vocabulary (`Platform` with
`OS/architecture/variant`, digest model) — full `Spec` struct generation lands
with the runtime milestone, where **bundle creation from our container model**
must produce a schema-validating `config.json`; test will pin schemas directly
from this pinned tree, not re-typed copies.

## image-spec v1.1.1 — pinned `147f9c13ce…`

Types at `upstream/image-spec/specs-go/v1/`:
`descriptor.go`, `config.go`, `manifest.go`, `index.go`, `layout.go`,
`mediatype.go`, `annotations.go`.

Implemented today (milestones A–B):

- `Digest` (`algo:hex`, sha256-only validation window) — used by and
  producing `Descriptor.digest`-style strings.
- `ImageRef` parsing aligns with the descriptor's reference grammar needs.
- `Descriptor`/`Platform` (`docker-image`): camelCase keys verified verbatim
  against the pinned Go tags; `data` embedded as base64 exactly like Go
  `encoding/json` handles `[]byte`; upstream `DescriptorEmptyJSON` ported
  (digest `sha256:44136fa3…caaff8a`, size 2, contents `{}`).
- `Manifest`/`Index` with the embedded `Versioned` schema version:
  skip-if-empty optional fields matching Go `omitempty`, and `validate()`
  conformance checks (schemaVersion 2, config media type pinned,
  indexed per-entry errors).
- Image `Config` structs: both casing regimes pinned by tests — camelCase
  manifest/index container fields vs PascalCase execution parameters
  (`Env`, `Entrypoint`, `Cmd`, `ExposedPorts`/`Volumes` as
  `map<K, {}>`, `WorkingDir`, `Labels`, `StopSignal`, `ArgsEscaped`);
  `architecture`/`os` at top level (Go's embedding of `Platform`);
  `config` always serialized (Go `omitempty` has no effect on structs).
- `oci-layout` bundle layout (`docker-storage`): `oci-layout` marker bytes
  `{"imageLayoutVersion":"1.0.0"}` verbatim; `blobs/sha256/<hex>`
  content addressing where the store root IS an OCI blobs directory;
  unsupported marker versions reject as `Unsupported`, never silently
  reinterpret.

Implemented in milestone C (`docker-distribution`):

- Manifest/index push and pull under the content-negotiation matrix
  (`DEFAULT_ACCEPT` prefers OCI index → OCI manifest → Docker manifest
  list → Docker schema 2); manifest bodies are parsed into these types only
  after the body digest and `Docker-Content-Digest` agree.
- Referrers responses are parsed as `Index`; the tag-schema fallback reads
  the `<algo>-<hex>` tag as an index and treats absence as "no referrers".
- Schema 1 manifests are rejected as `Unsupported`, never reinterpreted.

## distribution-spec v1.1.1 — pinned `a139cc4231…`

Implemented by `docker-distribution` against `upstream/distribution-spec/
spec.md` and `content-negotiation.md`:

- Authentication: `WWW-Authenticate` challenge parsing (Bearer + Basic,
  multiple header values), bearer token acquisition with service/scope and
  a per-scope cache, Basic fallback for registries without a token service.
- Blobs: `HEAD`/`GET /v2/<name>/blobs/<digest>` (pull streamed into the
  CAS with digest + length verification), monolithic `POST`→`PUT`, chunked
  `PATCH` with inclusive `Content-Range`, `416` resync through the upload
  status endpoint, `OCI-Chunk-Min-Length`, and closing `PUT?digest=`.
- Manifests: `GET`/`PUT /v2/<name>/manifests/<ref>` with the Accept
  preference list, digest verification, and schema 1 rejection.
- Discovery: tags `Link: rel="next"` pagination; referrers API plus the
  spec's tag-schema fallback; `artifactType` filtering.
- Management: manifest/blob `DELETE` (405 → `Unsupported`).
- Errors: HTTP status plus the `{"errors":[...]}` envelope mapped onto
  `docker-errors`.
- Transport: one request per connection, at most five redirects,
  cross-origin `Authorization`/`Cookie` stripping, HTTPS→HTTP downgrade
  rejection.

Known gaps: cross-repository blob mount, single-`POST` monolithic upload
(optional in the spec), schema 1, and trust-root pinning (the OS trust
store is used).

## Conformance harness plan

1. JSON-schema round trips: any `config.json` we emit must pass
   `runtime-spec/schema/config-schema.json`; any manifest we emit must
   validate against the image-spec schema equivalents.
2. Digest robustness: uppercase input normalized; non-sha256 → `Unsupported`
   (not unverified acceptance).
3. Distribution client: in-process loopback registry fixture implementing
   the pinned spec text; a real registry binary differential is deferred to
   milestone L.
4. rea (binary RE MCP) checks the *shipped binaries* of upstream tools at
   behavioral level when our implementation reaches parity testing (see
   TESTING.md) — the spec text decides intent; REA is used for compiled
   artifacts where text sources stop.
