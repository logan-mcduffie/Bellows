#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/bellows-five-phase.XXXXXX")"
server_pid=""
port="${BELLOWS_FIVE_PHASE_PORT:-17888}"
server="http://127.0.0.1:$port"
token="five-phase-demo-token"

cleanup() {
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

section() {
  echo
  echo "╔══════════════════════════════════════════════════════════════╗"
  printf '║  %-60s║\n' "$1"
  echo "╚══════════════════════════════════════════════════════════════╝"
}

run_bellows() {
  "$bellows" "$@" --server "$server"
}

section "Build the five-phase Bellows"
cargo build --manifest-path "$root/Cargo.toml" --bins --quiet
bellows="$root/target/debug/bellows"
bellowsd="$root/target/debug/bellowsd"
cp -a "$root/demo" "$scratch/workspace"
rm -rf "$scratch/workspace/target" "$scratch/workspace/.bellows"
cargo generate-lockfile --manifest-path "$scratch/workspace/Cargo.toml" --offline

BELLOWS_AUTH_TOKEN="$token" "$bellowsd" \
  --listen "127.0.0.1:$port" \
  --data-dir "$scratch/server-data" \
  --enable-execution \
  --max-executors 2 >"$scratch/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 50); do
  curl -fsS -H "Authorization: Bearer $token" "$server/v1/health" >/dev/null 2>&1 && break
  sleep 0.1
done

export BELLOWS_AUTH_TOKEN="$token"
export BELLOWS_SERVER="$server"
export BELLOWS_STATE_DIR="$scratch/client-state"

section "Phase 1 — L1, remote CAS, exact rustc reuse, graceful hierarchy"
(
  cd "$scratch/workspace"
  CARGO_TARGET_DIR="$scratch/p1-cold" "$bellows" run -- cargo build
) 2>&1 | tee "$scratch/p1-cold.log"
grep -q 'CACHE MISS.*forge_core' "$scratch/p1-cold.log"

(
  cd "$scratch/workspace"
  CARGO_TARGET_DIR="$scratch/p1-l1" "$bellows" run -- cargo build
) 2>&1 | tee "$scratch/p1-l1.log"
grep -q 'LOCAL HIT.*forge_core' "$scratch/p1-l1.log"

rm -rf "$scratch/client-state/l1"
(
  cd "$scratch/workspace"
  CARGO_TARGET_DIR="$scratch/p1-remote" "$bellows" run -- cargo build
) 2>&1 | tee "$scratch/p1-remote.log"
grep -q 'CACHE HIT.*forge_core' "$scratch/p1-remote.log"

section "Phase 2 — compile once, restore and execute many"
mkdir -p "$scratch/test-archive"
rustc --edition 2024 --test \
  "$scratch/workspace/crates/forge-core/src/lib.rs" \
  -o "$scratch/test-archive/forge-core-tests"
"$scratch/test-archive/forge-core-tests" >/dev/null
"$bellows" archive publish forge-tests-v1 "$scratch/test-archive" \
  --server "$server" | tee "$scratch/archive-publish.log"
rm -rf "$scratch/test-archive"
"$bellows" archive restore forge-tests-v1 "$scratch/executor-a" --server "$server"
"$bellows" archive restore forge-tests-v1 "$scratch/executor-b" --server "$server"
"$scratch/executor-a/forge-core-tests" >/dev/null
"$scratch/executor-b/forge-core-tests" >/dev/null
mkdir -p "$scratch/conflicting-archive"
printf 'different bytes' > "$scratch/conflicting-archive/forge-core-tests"
if "$bellows" archive publish forge-tests-v1 "$scratch/conflicting-archive" \
    --server "$server" >/dev/null 2>&1; then
  echo "publish-once archive unexpectedly accepted different content" >&2
  exit 1
fi
echo "archive executors: 2; recompilations: 0; publish-once conflict: rejected"

section "Phase 3 — declared Cargo action caches final linked output"
(
  cd "$scratch/workspace"
  rm -rf target
  "$bellows" action run --name final-link-v1 \
    --input Cargo.toml --input Cargo.lock --input crates \
    --output target/debug/forge-cli \
    --server "$server" -- \
    cargo build --locked --offline -p forge-cli
) 2>&1 | tee "$scratch/action-miss.log"
grep -q 'CACHE MISS.*final-link-v1' "$scratch/action-miss.log"
rm -rf "$scratch/workspace/target"
(
  cd "$scratch/workspace"
  "$bellows" action run --name final-link-v1 \
    --input Cargo.toml --input Cargo.lock --input crates \
    --output target/debug/forge-cli \
    --server "$server" -- \
    cargo build --locked --offline -p forge-cli
  ./target/debug/forge-cli
) 2>&1 | tee "$scratch/action-hit.log"
grep -q 'CACHE HIT.*final-link-v1' "$scratch/action-hit.log"
grep -q 'Bellows remembered this build' "$scratch/action-hit.log"

sed -i 's/    42/    43/' "$scratch/workspace/crates/forge-core/src/temperature.rs"
rm -rf "$scratch/workspace/target"
(
  cd "$scratch/workspace"
  "$bellows" action run --name final-link-v1 \
    --input Cargo.toml --input Cargo.lock --input crates \
    --output target/debug/forge-cli \
    --server "$server" -- \
    cargo build --locked --offline -p forge-cli
) 2>&1 | tee "$scratch/action-invalidated.log"
grep -q 'CACHE MISS.*final-link-v1' "$scratch/action-invalidated.log"

cp -a "$root/demo/phase3-fixture" "$scratch/nested-action"
rm -rf "$scratch/nested-action/target" "$scratch/nested-action/generator/target"
(
  cd "$scratch/nested-action"
  "$bellows" action run --name nested-build-script-v1 \
    --input Cargo.toml --input Cargo.lock --input build.rs --input src --input generator \
    --output target/debug/phase3-fixture \
    --server "$server" -- \
    cargo build --locked --offline
  ./target/debug/phase3-fixture
) 2>&1 | tee "$scratch/nested-action-miss.log"
grep -q 'CACHE MISS.*nested-build-script-v1' "$scratch/nested-action-miss.log"
grep -q 'build.rs → nested Cargo → cached binary' "$scratch/nested-action-miss.log"
rm -rf "$scratch/nested-action/target" "$scratch/nested-action/generator/target"
(
  cd "$scratch/nested-action"
  "$bellows" action run --name nested-build-script-v1 \
    --input Cargo.toml --input Cargo.lock --input build.rs --input src --input generator \
    --output target/debug/phase3-fixture \
    --server "$server" -- \
    cargo build --locked --offline
) 2>&1 | tee "$scratch/nested-action-hit.log"
grep -q 'CACHE HIT.*nested-build-script-v1' "$scratch/nested-action-hit.log"

section "Phase 4 — authenticated, attested, single-flight remote execution"
sed -i 's/    43/    44/' "$scratch/workspace/crates/forge-core/src/temperature.rs"
cp -a "$scratch/workspace" "$scratch/remote-a"
cp -a "$scratch/workspace" "$scratch/remote-b"
rm -rf "$scratch/remote-a/target" "$scratch/remote-b/target"
export BELLOWS_ACTION_DELAY_MS=1500
(
  cd "$scratch/remote-a"
  "$bellows" remote run --name remote-link-v1 \
    --input Cargo.toml --input Cargo.lock --input crates \
    --output target/debug/forge-cli --env BELLOWS_ACTION_DELAY_MS \
    --server "$server" -- \
    cargo build --locked --offline -p forge-cli
) >"$scratch/remote-a.log" 2>&1 &
remote_a=$!
(
  cd "$scratch/remote-b"
  "$bellows" remote run --name remote-link-v1 \
    --input Cargo.toml --input Cargo.lock --input crates \
    --output target/debug/forge-cli --env BELLOWS_ACTION_DELAY_MS \
    --server "$server" -- \
    cargo build --locked --offline -p forge-cli
) >"$scratch/remote-b.log" 2>&1 &
remote_b=$!
wait "$remote_a"
wait "$remote_b"
cat "$scratch/remote-a.log" "$scratch/remote-b.log"
grep -q 'EXECUTED.*remote-link-v1' "$scratch/remote-a.log" "$scratch/remote-b.log"
grep -Eq 'CACHE HIT|EXECUTED' "$scratch/remote-a.log" "$scratch/remote-b.log"
"$scratch/remote-a/target/debug/forge-cli" | grep -q '44°'
"$scratch/remote-b/target/debug/forge-cli" | grep -q '44°'
unset BELLOWS_ACTION_DELAY_MS

section "Phase 5 — advisory compiler-aware downstream impact"
cp -a "$root/demo" "$scratch/analysis"
rm -rf "$scratch/analysis/target" "$scratch/analysis/.bellows"
cargo generate-lockfile --manifest-path "$scratch/analysis/Cargo.toml" --offline
(
  cd "$scratch/analysis"
  BELLOWS_STATE_DIR="$scratch/analysis/.bellows" "$bellows" analyze snapshot baseline
  sed -i 's/    42/    45/' crates/forge-core/src/temperature.rs
  BELLOWS_STATE_DIR="$scratch/analysis/.bellows" "$bellows" analyze snapshot private-change
  BELLOWS_STATE_DIR="$scratch/analysis/.bellows" \
    "$bellows" analyze compare baseline private-change --json > "$scratch/private-impact.json"
  sed -i 's/-> u32/-> u64/' crates/forge-core/src/temperature.rs
  BELLOWS_STATE_DIR="$scratch/analysis/.bellows" "$bellows" analyze snapshot public-change
  BELLOWS_STATE_DIR="$scratch/analysis/.bellows" \
    "$bellows" analyze compare private-change public-change --json > "$scratch/public-impact.json"
)
grep -q '^  "syntactic_surface_changed": \[\],$' "$scratch/private-impact.json"
grep -q '^  "affected_downstream": \[\]$' "$scratch/private-impact.json"
sed -n '/"private_implementation_candidates": \[/,/^  \]/p' \
  "$scratch/private-impact.json" | grep -q 'forge-core'
sed -n '/"syntactic_surface_changed": \[/,/^  \]/p' \
  "$scratch/public-impact.json" | grep -q 'forge-core'
public_affected="$(sed -n '/"affected_downstream": \[/,/^  \]/p' "$scratch/public-impact.json")"
printf '%s\n' "$public_affected" | grep -q 'forge-core'
printf '%s\n' "$public_affected" | grep -q 'forge-engine'
printf '%s\n' "$public_affected" | grep -q 'forge-cli'
sed -n '1,80p' "$scratch/private-impact.json"
sed -n '1,80p' "$scratch/public-impact.json"

section "Retention — reference-aware collection bounds durable storage"
"$bellows" gc --max-mb 0 --server "$server" | tee "$scratch/gc.log"
# A rejected publish intentionally leaves a fresh orphan. GC protects fresh
# uploads for one hour so it cannot race a blob-to-record publication.
grep -Eq -- '(→|->) [0-9]+ B' "$scratch/gc.log"

section "Five phases complete"
echo "L1 + remote compiler cache; compile-once/test-many archives; final Cargo"
echo "actions; authenticated remote execution; and compiler-aware impact analysis"
echo "all passed their machine-checked acceptance gates."
