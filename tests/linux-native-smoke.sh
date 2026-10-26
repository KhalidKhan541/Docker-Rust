#!/usr/bin/env bash
# End-to-end Linux smoke test: load a locally built OCI image, create a
# container over the Engine API, start it with the native runtime, and verify
# that the real process ran and exited successfully.
#
# This must run as root because the native runtime uses mount namespaces,
# chroot, mknod, and sethostname.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "linux-native-smoke: must run as root" >&2
    exit 1
fi

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DAEMON_BIN="$REPO_ROOT/target/debug/docker-rsd"
INIT_BIN="$REPO_ROOT/target/debug/docker-runtime-init"
if [ ! -x "$DAEMON_BIN" ] || [ ! -x "$INIT_BIN" ]; then
    echo "linux-native-smoke: build the workspace first (cargo build --workspace)" >&2
    exit 1
fi

WORK=$(mktemp -d)
PORT=$((20000 + RANDOM % 20000))
DAEMON_PID=
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

cat >"$WORK/probe.S" <<'EOF'
.global _start
.section .text
_start:
    mov $1, %rax
    mov $1, %rdi
    lea message(%rip), %rsi
    mov $message_len, %rdx
    syscall
    mov $60, %rax
    xor %rdi, %rdi
    syscall
.section .rodata
message:
    .ascii "native-runtime-ok\n"
.set message_len, . - message
EOF
cc -nostdlib -static -no-pie -o "$WORK/probe" "$WORK/probe.S"

mkdir -p "$WORK/layer"
cp "$WORK/probe" "$WORK/layer/probe"
chmod 0755 "$WORK/layer/probe"
tar -C "$WORK/layer" -cf "$WORK/layer.tar" probe
gzip -n -c "$WORK/layer.tar" >"$WORK/layer.tar.gz"

layer_digest="sha256:$(sha256sum "$WORK/layer.tar.gz" | cut -d' ' -f1)"
diff_id="sha256:$(sha256sum "$WORK/layer.tar" | cut -d' ' -f1)"
layer_size=$(stat -c%s "$WORK/layer.tar.gz")

cat >"$WORK/config.json" <<EOF
{"architecture":"amd64","os":"linux","config":{"Cmd":["/probe"]},"rootfs":{"type":"layers","diff_ids":["$diff_id"]}}
EOF
config_digest="sha256:$(sha256sum "$WORK/config.json" | cut -d' ' -f1)"
config_size=$(stat -c%s "$WORK/config.json")

cat >"$WORK/manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"$config_digest","size":$config_size},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"$layer_digest","size":$layer_size}]}
EOF
manifest_digest="sha256:$(sha256sum "$WORK/manifest.json" | cut -d' ' -f1)"
manifest_size=$(stat -c%s "$WORK/manifest.json")

mkdir -p "$WORK/layout/blobs/sha256"
cp "$WORK/layer.tar.gz" "$WORK/layout/blobs/sha256/${layer_digest#sha256:}"
cp "$WORK/config.json" "$WORK/layout/blobs/sha256/${config_digest#sha256:}"
cp "$WORK/manifest.json" "$WORK/layout/blobs/sha256/${manifest_digest#sha256:}"
printf '{"imageLayoutVersion":"1.0.0"}' >"$WORK/layout/oci-layout"
cat >"$WORK/layout/index.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"$manifest_digest","size":$manifest_size,"annotations":{"org.opencontainers.image.ref.name":"probe:latest"}}]}
EOF
tar -C "$WORK/layout" -cf "$WORK/image.tar" .

"$DAEMON_BIN" --runtime oci --host "127.0.0.1:$PORT" --data-root "$WORK/data" \
    >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 100); do
    if curl -fsS "http://127.0.0.1:$PORT/_ping" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        cat "$WORK/daemon.log" >&2
        echo "linux-native-smoke: docker-rsd exited during startup" >&2
        exit 1
    fi
    sleep 0.1
done
curl -fsS "http://127.0.0.1:$PORT/_ping" >/dev/null

curl -fsS -X POST -H 'Content-Type: application/x-tar' --data-binary @"$WORK/image.tar" \
    "http://127.0.0.1:$PORT/images/load" >/dev/null

curl -fsS -X POST -H 'Content-Type: application/json' \
    -d '{"Image":"probe:latest","HostConfig":{"NetworkMode":"none"}}' \
    "http://127.0.0.1:$PORT/containers/create?name=probe" >/dev/null

curl -fsS -X POST -H 'Content-Type: application/json' -d '{}' \
    "http://127.0.0.1:$PORT/containers/probe/start" >/dev/null

state=created
for _ in $(seq 1 100); do
    state=$(curl -fsS "http://127.0.0.1:$PORT/containers/probe/json" | jq -r '.State.Status')
    if [ "$state" = "exited" ] || [ "$state" = "dead" ]; then
        break
    fi
    sleep 0.1
done

inspect=$(curl -fsS "http://127.0.0.1:$PORT/containers/probe/json")
echo "$inspect" | jq '{Name, State, Image}'
exit_code=$(echo "$inspect" | jq -r '.State.ExitCode')
if [ "$exit_code" != "0" ]; then
    cat "$WORK/daemon.log" >&2
    echo "linux-native-smoke: container did not exit successfully (state=$state exit=$exit_code)" >&2
    exit 1
fi

# The engine always captures container stdio into its log store (pipelines
# feed `docker logs`), so the probe output is served by the logs endpoint
# rather than appearing in the daemon's own stdout. Retry briefly because the
# state flips to exited before the last capture pipe necessarily drains.
seen=0
for _ in $(seq 1 50); do
    curl -fsS \
        "http://127.0.0.1:$PORT/containers/probe/logs?stdout=1&stderr=1&follow=1" \
        -o "$WORK/probe.logs" || true
    if [ -f "$WORK/probe.logs" ] && grep -aq "native-runtime-ok" "$WORK/probe.logs"; then
        seen=1
        break
    fi
    sleep 0.1
done
if [ "$seen" -ne 1 ]; then
    cat "$WORK/daemon.log" >&2
    echo "linux-native-smoke: probe output was not served by the logs endpoint" >&2
    cat "$WORK/probe.logs" >&2 || true
    exit 1
fi

echo "linux-native-smoke: native runtime executed the probe successfully"
