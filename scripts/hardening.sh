#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/bellows-hardening.XXXXXX")"
server_pid=""
cleanup() {
  if [ -n "$server_pid" ]; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

cargo build --manifest-path "$repo/Cargo.toml" --workspace --bins >/dev/null
bellows="$repo/target/debug/bellows"
bellowsd="$repo/target/debug/bellowsd"
cp -R "$repo/demo" "$tmp/workspace"
rm -rf "$tmp/workspace/target"

run_fixture() {
  local target="$1"
  shift
  (
    cd "$tmp/workspace"
    env \
      CARGO_TARGET_DIR="$target" \
      CARGO_INCREMENTAL=0 \
      BELLOWS_WORKSPACE="$tmp/workspace" \
      BELLOWS_STATE_DIR="$tmp/state" \
      BELLOWS_EVENT_LOG="$tmp/events.jsonl" \
      RUSTC_WRAPPER="$bellows" \
      "$@" cargo check --workspace
  )
}

wait_for_live() {
  local url="$1"
  for _ in $(seq 1 100); do
    curl -fsS "$url/live" >/dev/null 2>&1 && return 0
    sleep 0.05
  done
  return 1
}

echo "hardening: unsafe server configurations are rejected"
if timeout 3 "$bellowsd" --listen 0.0.0.0:0 --data-dir "$tmp/unsafe-server" >"$tmp/unsafe.log" 2>&1; then
  echo "unauthenticated non-loopback server unexpectedly started" >&2
  exit 1
fi
grep -q 'refusing unauthenticated non-loopback listener' "$tmp/unsafe.log"
if "$bellowsd" --max-requests 0 --data-dir "$tmp/invalid-server" >"$tmp/invalid.log" 2>&1; then
  echo "server unexpectedly accepted a zero request limit" >&2
  exit 1
fi
grep -q 'max-requests must be greater than zero' "$tmp/invalid.log"

echo "hardening: authentication, single ownership, liveness, and SIGTERM"
auth_port=$((18000 + RANDOM % 5000))
BELLOWS_AUTH_TOKEN=hardening-secret "$bellowsd" \
  --listen "127.0.0.1:$auth_port" \
  --data-dir "$tmp/auth-server" \
  >"$tmp/auth-server.log" 2>&1 &
server_pid=$!
wait_for_live "http://127.0.0.1:$auth_port"
test "$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$auth_port/v1/health")" = 401
test "$(curl -sS -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' "http://127.0.0.1:$auth_port/v1/health")" = 401
test "$(curl -sS -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer hardening-secret' "http://127.0.0.1:$auth_port/v1/health")" = 200
if "$bellowsd" --data-dir "$tmp/auth-server" >"$tmp/second-server.log" 2>&1; then
  echo "two servers unexpectedly acquired one data directory" >&2
  exit 1
fi
grep -q 'already owned by another bellowsd process' "$tmp/second-server.log"
kill -TERM "$server_pid"
wait "$server_pid"
server_pid=""
BELLOWS_AUTH_TOKEN=hardening-secret "$bellowsd" \
  --listen "127.0.0.1:$auth_port" \
  --data-dir "$tmp/auth-server" \
  >"$tmp/auth-server-restart.log" 2>&1 &
server_pid=$!
wait_for_live "http://127.0.0.1:$auth_port"
test "$(curl -sS -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer hardening-secret' "http://127.0.0.1:$auth_port/v1/health")" = 200
kill -TERM "$server_pid"
wait "$server_pid"
server_pid=""

echo "hardening: malformed server configuration fails open"
: >"$tmp/events.jsonl"
run_fixture "$tmp/target-malformed" BELLOWS_L1=0 BELLOWS_SERVER='://not-a-url'
grep -q '"kind":"fallback"' "$tmp/events.jsonl"

echo "hardening: corrupt L1 index and offline server fail open"
port=$((28000 + RANDOM % 10000))
"$bellowsd" \
  --listen "127.0.0.1:$port" \
  --data-dir "$tmp/server" \
  >"$tmp/bellowsd.log" 2>&1 &
server_pid=$!
wait_for_live "http://127.0.0.1:$port"
run_fixture "$tmp/target-prime" BELLOWS_L1=1 BELLOWS_SERVER="http://127.0.0.1:$port"
index="$(find "$tmp/state/l1/actions" -name '*.json' -print -quit)"
test -n "$index"
printf '{not-json' >"$index"
kill -TERM "$server_pid"
wait "$server_pid" || true
server_pid=""
: >"$tmp/events.jsonl"
run_fixture "$tmp/target-corrupt-l1" BELLOWS_L1=1 BELLOWS_SERVER="http://127.0.0.1:$port"
grep -q 'L1 index is unavailable or corrupt' "$tmp/events.jsonl"
grep -q 'remote unavailable' "$tmp/events.jsonl"

echo "Bellows hardening fail-open checks passed"
