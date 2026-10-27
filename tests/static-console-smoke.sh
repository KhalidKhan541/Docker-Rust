#!/usr/bin/env bash
# Same-origin static console smoke test. Runs with the synthetic runtime, so it
# needs no root or Linux container facilities. It proves that:
#   * the landing page, clean URLs, assets, MIME types, 404s and 405s work;
#   * API namespaces and version-prefixed paths can not be shadowed by files;
#   * traversal and encoded traversal are rejected.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DAEMON_BIN=${DAEMON_BIN:-"$REPO_ROOT/target/debug/docker-rsd"}
if [ ! -x "$DAEMON_BIN" ]; then
    echo "static-console-smoke: build the workspace first (cargo build --workspace)" >&2
    exit 1
fi

WORK=$(mktemp -d)
PORT=$((20000 + RANDOM % 20000))
BASE="http://127.0.0.1:$PORT"
DAEMON_PID=
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'status=$?; echo "static-console-smoke: failed at line $LINENO (status $status)" >&2; if [ -f "$WORK/daemon.log" ]; then tail -n 80 "$WORK/daemon.log" >&2; fi' ERR

WEB="$WORK/web"
mkdir -p "$WEB/assets" "$WEB/containers" "$WEB/v1.51"
printf '<h1>docker-rs</h1>\n' >"$WEB/index.html"
printf '<h1>console</h1>\n' >"$WEB/console.html"
printf 'console.log("ok");\n' >"$WEB/assets/app.js"
printf 'must not shadow\n' >"$WEB/containers/json"
printf 'must not shadow\n' >"$WEB/networks"
printf 'must not shadow\n' >"$WEB/v1.51/console.html"

"$DAEMON_BIN" --host "127.0.0.1:$PORT" --data-root "$WORK/data" \
    --web-root "$WEB" >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 100); do
    if curl -fsS "$BASE/_ping" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        cat "$WORK/daemon.log" >&2
        echo "static-console-smoke: docker-rsd exited during startup" >&2
        exit 1
    fi
    sleep 0.1
done
curl -fsS "$BASE/_ping" >/dev/null

# expect_status STATUS URL [curl args...]
expect_status() {
    local expected=$1
    shift
    local actual
    actual=$(curl -sS --path-as-is -D "$WORK/headers" -o "$WORK/body" -w '%{http_code}' "$@")
    if [ "$actual" != "$expected" ]; then
        echo "static-console-smoke: expected HTTP $expected, got $actual for $*" >&2
        cat "$WORK/body" >&2
        exit 1
    fi
}

# Landing page, clean URL, and a real asset. Headers are checked for the MIME
# types the console needs and for the static safety headers.
curl -fsS -D "$WORK/headers" -o "$WORK/body" "$BASE/"
grep -qi '^Content-Type: text/html; charset=utf-8' "$WORK/headers"
grep -qi '^X-Content-Type-Options: nosniff' "$WORK/headers"
grep -qi '^Cache-Control: no-cache' "$WORK/headers"
grep -q '<h1>docker-rs</h1>' "$WORK/body"

curl -fsS -D "$WORK/headers" -o "$WORK/body" "$BASE/console"
grep -qi '^Content-Type: text/html; charset=utf-8' "$WORK/headers"
grep -q '<h1>console</h1>' "$WORK/body"

curl -fsS -D "$WORK/headers" -o "$WORK/body" "$BASE/assets/app.js"
grep -qi '^Content-Type: text/javascript; charset=utf-8' "$WORK/headers"
grep -q 'console.log("ok");' "$WORK/body"

# The API wins over same-named files, including under a reserved namespace and
# a version prefix.
curl -fsS -o "$WORK/body" "$BASE/containers/json"
grep -q '^\[' "$WORK/body"
if grep -q 'must not shadow' "$WORK/body"; then
    echo "static-console-smoke: static file shadowed /containers/json" >&2
    exit 1
fi

expect_status 404 "$BASE/networks"
if grep -q 'must not shadow' "$WORK/body"; then
    echo "static-console-smoke: static file shadowed /networks" >&2
    exit 1
fi

expect_status 404 "$BASE/v1.51/console.html"
if grep -q 'must not shadow' "$WORK/body"; then
    echo "static-console-smoke: static file shadowed a versioned API path" >&2
    exit 1
fi

# Missing pages, traversal, and encoded traversal stay 404s.
expect_status 404 "$BASE/missing"
expect_status 404 "$BASE/%2e%2e/Cargo.toml"
expect_status 404 "$BASE/..%2fCargo.toml"
expect_status 404 "$BASE/%2e%2e%2fCargo.toml"

# Methods: an existing page gets 405 + Allow, an unknown page stays 404.
expect_status 405 -X POST "$BASE/console.html"
grep -qi '^Allow: GET, HEAD' "$WORK/headers" || {
    echo "static-console-smoke: 405 response is missing Allow: GET, HEAD" >&2
    exit 1
}
expect_status 404 -X POST "$BASE/missing"

# HEAD is supported for a real page and reports the GET representation size.
curl -fsS -I -D "$WORK/head.headers" -o /dev/null "$BASE/console.html" >/dev/null
grep -qi '^HTTP/.* 200' "$WORK/head.headers"
grep -qi '^Content-Length: 17' "$WORK/head.headers"

echo "static-console-smoke: ok"
