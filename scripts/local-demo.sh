#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
sentinel_pid=""
cleanup() {
  if [ -n "$sentinel_pid" ]; then
    kill "$sentinel_pid" 2>/dev/null || true
    wait "$sentinel_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

cargo build --manifest-path "$root/Cargo.toml" --workspace --bins
bellows="$root/target/debug/bellows"
cp -R "$root/demo" "$scratch/workspace"
workspace="$scratch/workspace"
cache="$scratch/cache"
events="$cache/events.jsonl"

# A live listener makes the offline guarantee observable: any accidental HTTP
# connection writes to sentinel.log and fails the acceptance check below.
port="$((30000 + ($$ % 20000)))"
socat TCP-LISTEN:"$port",bind=127.0.0.1,reuseaddr,fork \
  SYSTEM:'printf "connected\n" >&2' 2>"$scratch/sentinel.log" &
sentinel_pid="$!"

(
  cd "$workspace"
  BELLOWS_SERVER="http://127.0.0.1:$port" \
    CARGO_TARGET_DIR="$scratch/target-cold" \
    "$bellows" local --cache-dir "$cache" -- \
    cargo build --release
)
rm -rf "$scratch/target-cold"
(
  cd "$workspace"
  BELLOWS_SERVER="http://127.0.0.1:$port" \
    CARGO_TARGET_DIR="$scratch/target-warm" \
    "$bellows" local --cache-dir "$cache" -- \
    cargo build --release
)

if grep -q connected "$scratch/sentinel.log"; then
  echo "bellows local attempted a network connection" >&2
  exit 1
fi
grep -q '"kind":"l1_hit"' "$events"
"$scratch/target-warm/release/forge-cli" | grep -q '42'

# A transitive source edit must miss and produce the changed behavior.
misses_before="$(grep -c '"kind":"miss"' "$events")"
sed -i 's/42/43/' "$workspace/crates/forge-core/src/temperature.rs"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/target-edited" \
    "$bellows" local --cache-dir "$cache" -- \
    cargo build --release
)
misses_after="$(grep -c '"kind":"miss"' "$events")"
test "$misses_after" -gt "$misses_before"
"$scratch/target-edited/release/forge-cli" | grep -q '43'

# Independent Cargo processes may safely share the durable local store.
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/target-concurrent-a" \
    "$bellows" local --cache-dir "$cache" -- \
    cargo build --release
) &
first_pid="$!"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/target-concurrent-b" \
    "$bellows" local --cache-dir "$cache" -- \
    cargo build --release
) &
second_pid="$!"
wait "$first_pid"
wait "$second_pid"

# Declared local actions restore whole outputs without executing again.
(
  cd "$workspace"
  "$bellows" action run --local --cache-dir "$cache" \
    --name local-rustc-v1 \
    --input crates/forge-core/src/temperature.rs \
    --output libfixture.rlib \
    -- rustc --crate-name fixture --crate-type rlib \
      crates/forge-core/src/temperature.rs -o libfixture.rlib
)
rm "$workspace/libfixture.rlib"
second_action="$(
  cd "$workspace"
  "$bellows" action run --local --cache-dir "$cache" \
    --name local-rustc-v1 \
    --input crates/forge-core/src/temperature.rs \
    --output libfixture.rlib \
    -- rustc --crate-name fixture --crate-type rlib \
      crates/forge-core/src/temperature.rs -o libfixture.rlib
)"
grep -q 'HIT' <<<"$second_action"
test -f "$workspace/libfixture.rlib"

# A surviving action record with a missing blob is treated as a safe miss.
record="$(find "$cache/store-v5/declared" -name '*.json' -type f | head -n 1)"
digest="$(jq -r '.outputs[0].digest' "$record")"
rm "$cache/store-v5/blobs/${digest:0:2}/$digest"
rm "$workspace/libfixture.rlib"
recovered_action="$(
  cd "$workspace"
  "$bellows" action run --local --cache-dir "$cache" \
    --name local-rustc-v1 \
    --input crates/forge-core/src/temperature.rs \
    --output libfixture.rlib \
    -- rustc --crate-name fixture --crate-type rlib \
      crates/forge-core/src/temperature.rs -o libfixture.rlib 2>&1
)"
grep -q 'stale cached result will be rebuilt' <<<"$recovered_action"
grep -q 'CACHE MISS' <<<"$recovered_action"
test -f "$workspace/libfixture.rlib"

# Collection may remove records and blobs, but the next invocation safely
# rebuilds instead of treating cache loss as a build failure.
"$bellows" gc --local --cache-dir "$cache" --max-mb 0
rm "$workspace/libfixture.rlib"
(
  cd "$workspace"
  "$bellows" action run --local --cache-dir "$cache" \
    --name local-rustc-v1 \
    --input crates/forge-core/src/temperature.rs \
    --output libfixture.rlib \
    -- rustc --crate-name fixture --crate-type rlib \
      crates/forge-core/src/temperature.rs -o libfixture.rlib
)
test -f "$workspace/libfixture.rlib"
"$bellows" stats --local --cache-dir "$cache" --json >/dev/null

echo "bellows local demo passed"
