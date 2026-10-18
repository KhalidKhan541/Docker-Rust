# Security Analysis

Threat-model classes distilled from the Docker ecosystem's attack surface,
and how each is addressed at the current milestone.

## Threat classes (from upstream structure)

1. **Container escape** — kernel boundary between container and host.
   Addressed by the runtime layer (Milestone E; runc's
   `libcontainer/container.go` surface: namespaces, caps, seccomp, rootfs
   setup). Our runtime must produce `Unsupported` on anything it cannot
   guarantee rather than a weaker fallback.
2. **Privilege escalation via configuration** — `privileged`,
   `--user`, caps, mount sockets. Upstream gates these in HostConfig
   validation + daemon policy. Our `HostConfig::validate(allow_privileged)`
   encodes the gate: privileged execution requires an explicit daemon opt-in
   (`Forbidden` otherwise).
3. **API/administration surface** — HTTP API on a TCP host/port is the
   single most-attacked Docker interface. Our API milestone (F/G) inherits
   upstream rules, verified at the source level:
   - default bind stays loopback-only unless `0.0.0.0` is explicitly
     configured (tls/no-tls warnings upstream) — enforced in
     `daemon-config` validation (`api_host` port range/shape checks live).
   - version gating (unknown/newer client version → structured error, never
     silent behavior change) — mirrored from `moby/api/server/middleware/
     version.go` behavior.
4. **DoS via resource exhaustion** — event streams, log files, config-file
   parsing. Our bounded event broker (drop-oldest, capacity-pinned) exists
   precisely for subscriber-backpressure DoS; config parsing is
   `deny_unknown_fields` so unexpected trusted heterogeneous fallbacks
   cannot appear in later layers unnoticed.
5. **Supply chain** — all upstream sources are pinned by commit (root
   `README.md` table); Rust deps are resolved through one lockfile;
   `upstream/` trees are gitignored so third-party code never enters our
   history.
6. **Path traversal / unsafe IDs** — `SafePath` rejects absolute, `..`,
   drive-letter, and embedded-NUL paths against a data root; ids are
   random, 64-hex, never accepted from users beyond exact-prefix lookup
   (ambiguous prefix resolution returns an error, not first-match).

## Validations implemented so far (Milestone A)

| Surface | Guard |
|---------|-------|
| container names | charset `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, ≤ 255 |
| image references | registry detection, tag format, digest format (64 hex) |
| env entries | `KEY=value` shape, non-empty key, no control chars |
| ports | 1..=65535, explicit protocol |
| labels | deny `"` `\` and non-ASCII control bytes in keys |
| restart policy | `Always`/`UnlessStopped` do not pair with `max_retry_count` |
| cpu-shares / memory | ranges 2..=262144 / reasonable minimums |
| data roots | relative paths, drive-letter escapes, symlink-free joins under root |
| unknown config | daemon.json rejects unknown keys (no typo-silence) |
| privileged | `Forbidden` unless daemon explicitly allows |

## What we deliberately do NOT do yet

- No user/kernel checks are faked. `ChildProcess::spawn` reports
  `Unsupported` if a Windows host lacks what a milestone needs; nothing
  silently degrades.
- No third-party process executes docker-rs code with elevated rights in
  these milestones; privilege work is Milestone K only.
- Digest algorithms: only `sha256` is implemented (spec-minimum for
  image-spec v1.1.1 usage in Docker); `md5`, `sha1` and future algorithms
  return `Unsupported` rather than accepting-but-not-verifying.

## Review hooks for later milestones

- Runtime (E): capability bounding sets, seccomp/no-seccomp honesty, userns.
- Network (H, after runtime): port-publish allowlists, no default
  `0.0.0.0` binding without explicit config.
- Registry (C): TLS-only by default, referrers/tag-schema CC validation.
- API (F/G): authn/authz middleware slots (upstream's authz plugins), 499
  for client-side cancellation only.
