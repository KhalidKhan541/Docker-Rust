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

Implemented today:

- `Digest` (`algo:hex`, sha256-only validation window) — used by and
  producing `Descriptor.digest`-style strings.
- `ImageRef` parsing aligns with the descriptor's reference grammar needs.

 Coming in milestone B/C (`docker-image`):

- `Descriptor`, `(Image|Index)Manifest`, image `Config` structs, and the
  `oci-layout` bundle layout (`layout.go`'s `ImageIndex` + layout markers),
  stored content-addressed exactly like containerd's content store model.

## distribution-spec v1.1.1 — pinned `a139cc4231…`

`upstream/distribution-spec/spec.md` (endpoints) and
`content-negotiation.md` (accept-header rules) govern the registry client
(milestone C): `HEAD/GET /v2/<name>/blobs|manifests`, `POST
/v2/<name>/blobs/uploads/`, chunked + single-shot PUT semantics, challenge /
token auth, and the manifest `Content-Type` matrix..digest tagging rules
(`referrers` and tag-schema images) are part of the v1.1 contract the client
will speak.

## Conformance harness plan

1. JSON-schema round trips: any `config.json` we emit must pass
   `runtime-spec/schema/config-schema.json`; any manifest we emit must
   validate against the image-spec schema equivalents.
2. Digest robustness: uppercase input normalized; non-sha256 → `Unsupported`
   (not unverified acceptance).
3. Distribution client: live protocol tests against a pinned local registry
   fixture in milestone C; no `latest` tag synthesis beyond spec.
4. rea (binary RE MCP) checks the *shipped binaries* of upstream tools at
   behavioral level when our implementation reaches parity testing (see
   TESTING.md) — the spec text decides intent; REA is used for compiled
   artifacts where text sources stop.
