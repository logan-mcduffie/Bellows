# Validation record

Validated locally on 2026-07-11 with `rustc 1.92.0`, the same stable compiler
version used by Manifold CI.

## Repository gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/demo.sh
scripts/five-phase-demo.sh
```

All passed. The demo machine-checks cold publication, fresh-runner remote hits,
server-restart durability, transitive source invalidation, fleet single-flight,
corruption rejection and repair, executable behavior, and offline fallback.

The five-phase showcase additionally machine-checks L1/remote hierarchy,
publish-once test archives with two executors, cached final binaries, a
`build.rs` that launches a nested offline Cargo generator, authenticated and
toolchain-attested remote execution with concurrent single-flight, syntactic
private/public surface analysis, downstream dependency closure, and
reference-aware collection to a zero-byte budget.

Authentication was separately checked by starting `bellowsd --auth-token
secret`: unauthenticated and incorrect-token health requests returned 401,
while `BELLOWS_AUTH_TOKEN=secret bellows doctor` passed.

## Manifold smoke target

The real `/home/logan/dev/manifold` checkout was built twice with isolated empty
target directories:

```bash
CARGO_TARGET_DIR=/tmp/runner-a GITHUB_RUN_ID=101 GITHUB_SHA=aaaaaaaa \
  bellows run -- cargo check -p manifold-mod-types

CARGO_TARGET_DIR=/tmp/runner-b GITHUB_RUN_ID=202 GITHUB_SHA=bbbbbbbb \
  bellows run -- cargo check -p manifold-mod-types
```

Observed on this machine:

| Run | Wall time | Remote compiler hits |
|---|---:|---:|
| Cold runner A | 2.64s | 0 |
| Fresh runner B | 1.51s | 5 |

The fresh-runner smoke check was approximately 43% faster. It restored
`unicode-ident`, `serde-core`, `proc-macro2`, `quote`, and `syn`. Build scripts,
`serde-derive`, the proc-macro-consuming `serde` action, and downstream products
whose extern identity changed bypassed or missed conservatively.

This is a small integration smoke measurement, not a forecast for Manifold's
full workflow. Full-pipeline acceptance still requires two representative CI
runs and end-to-end wall-time, transfer, storage, and hit-rate comparison.
