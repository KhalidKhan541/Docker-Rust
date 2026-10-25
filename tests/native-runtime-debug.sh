#!/usr/bin/env bash
# Debug harness for the Linux-only native runtime checks on a bare host; a
# dual-boot Ubuntu is perfect. Safe to re-run: it refreshes the clone,
# reinstalls whatever is missing, rebuilds, and executes both root smoke
# targets with backtraces enabled. A full log is kept for reports.
#
# Usage (from this repository, or any directory):
#   bash tests/native-runtime-debug.sh
#
# Environment:
#   REPO_URL    default https://github.com/KhalidKhan541/Docker-Rust.git
#   CLONE_ROOT  default $HOME/docker-rs-debug (fresh clone kept on ext4)
#   REPORT      default $HOME/native-runtime-debug.log
set -euo pipefail

REPO_URL="${REPO_URL:-https://github.com/KhalidKhan541/Docker-Rust.git}"
CLONE_ROOT="${CLONE_ROOT:-$HOME/docker-rs-debug}"
WORK="${WORK:-$CLONE_ROOT/Docker-Rust}"
REPORT="${REPORT:-$HOME/native-runtime-debug.log}"

if [ "$(id -u)" -eq 0 ]; then
    echo "native-runtime-debug: run as a normal sudo user, not root (it escalates itself)" >&2
    exit 1
fi

echo "== host =="
uname -a || true
sed -n '1,4p' /etc/os-release 2>/dev/null || true
aa-status 2>/dev/null | sed -n '1,6p' || true

echo "== toolchain =="
if ! command -v cargo >/dev/null 2>&1; then
    echo "rustup: installing for $USER"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cargo --version
rustc --version

echo "== packages (requires sudo) =="
sudo env DEBIAN_FRONTEND=noninteractive apt-get update -qq
sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc jq libssl-dev pkg-config

echo "== clone =="
if [ ! -d "$WORK" ]; then
    git clone --depth 1 "$REPO_URL" "$WORK"
else
    git -C "$WORK" fetch --depth 1 origin main
    git -C "$WORK" reset --hard origin/main
fi

cd "$WORK"
export RUST_BACKTRACE=full

echo "== build =="
cargo build --workspace 2>&1 | tee -a "$REPORT"

echo "== native runtime test (root) =="
cargo test -p docker-rs-integration --test native_runtime --no-run --message-format=json 2>/dev/null \
    | jq -r 'select(.reason == "compiler-artifact" and .target.name == "native_runtime" and .executable != null) | .executable' \
    | tail -n 1 >/tmp/drc-test-bin
TEST_BIN=$(cat /tmp/drc-test-bin)
test -n "$TEST_BIN"
sudo PATH="$PATH" "$TEST_BIN" --ignored --nocapture 2>&1 | tee -a "$REPORT"
status=${PIPESTATUS[0]}

echo "== engine API native smoke (root) =="
sudo PATH="$PATH" bash tests/linux-native-smoke.sh 2>&1 | tee -a "$REPORT" || true

echo
echo "== done: native test exit $status; full report: $REPORT =="
exit "$status"
