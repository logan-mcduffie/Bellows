# Manifold integration

Bellows is integrated at Manifold's CI setup seam. Cargo commands, test
partitions, toolchain pins, and hardware boundaries stay unchanged.

## Current WarpBuild topology

CPU jobs run on ephemeral WarpBuild runners. Each job restores a Bellows binary
pinned by full commit SHA, restores a lane-specific content store with
`actions/cache`, and starts an authenticated loopback-only `bellowsd`. The job
quiesces the daemon and bounds the store before the cache post-step archives it.
This topology needs no public Bellows service or long-lived runner.

The two GPU execution jobs remain on the local `[self-hosted, gpu]` runner.
Bellows may restore portable compiler outputs there, but browser pixels and GPU
tests still execute on the RTX 5080.

The production actions live in Manifold as `setup-bellows-warp`,
`setup-bellows`, and `teardown-bellows-warp`. The Bellows source repository
remains independent; Manifold consumes an immutable commit rather than a
mutable branch or local checkout.

## Workflow migration

For another trusted GitHub Actions deployment, copy `action.yml` into a local
composite action and invoke it after starting or connecting to an authenticated
service:

```yaml
- uses: ./.github/actions/setup-bellows
  with:
    server-url: ${{ steps.bellows-service.outputs.server }}
    auth-token: ${{ steps.bellows-service.outputs.token }}
    target-namespace: clippy
```

The namespace map remains unchanged:

| Workload | Namespace |
|---|---|
| Stable native/WASM checks, builds, and CPU tests | `stable` |
| Clippy | `clippy` |
| Stable browser parity | `browser-stable` |
| Pinned-nightly threaded WASM | `threaded` |

The action retains `CARGO_INCREMENTAL=0` and target-root separation. Manifold's
workflow owns any project-specific profile overrides. Local `bellows run`
leaves Cargo's incremental setting alone.

## Expected first-phase result

- A fresh runner can restore Rust library metadata and rlibs produced by an
  equivalent runner.
- Simultaneous unit/integration/compile-contract cold misses converge through
  Bellows leases instead of compiling the same library independently.
- Stable and pinned-nightly, native and WASM, features, flags, and target triples
  remain distinct because their actual rustc invocations differ.
- Test harnesses, final links and build-script runs are restored when every
  input matches. Tests that bake `env!("CARGO_MANIFEST_DIR")` (golden images,
  fixtures) are pinned to their checkout; reading the variable at run time
  makes them shareable across worktrees.
- Crates loading procedural macros are cached. The macro artifacts' digests
  and their tracked expansion inputs are part of the key.
- GPU and browser pixel gates still execute on the RTX 5080; only eligible
  compiler products are restored there.

Use `bellows stats` after cold and warm runs and compare end-to-end workflow wall
time, bytes restored, compiler hits, single-flight waits, and bypass reasons.
`bellows explain --json` is suitable for attaching diagnostics to CI artifacts.

## Later-phase pilot lanes

The five-phase demonstrator supports opt-in pilots without changing Manifold's
canonical lanes:

- A producer can publish a nextest archive directory with `bellows archive
  publish <content-version> <archive-dir>`; unit and integration executors
  restore the same publish-once tree independently.
- `scripts/build-test-mod-cached.sh` can migrate to `bellows action run` once its
  selected manifest, reachable crates, toolchain files, WIT, and deterministic
  output paths are declared. The action sandbox has no ambient Cargo
  credentials and requires locked/offline or vendored dependencies.
- `flagship-game-bundle`'s nested WASM build is restored as a build-script run;
  its nested Cargo target directory is scratch and is not stored.
- The self-hosted GPU job runs its own `bellowsd`. It can consume the
  developer service on the same machine with `BELLOWS_READ_ONLY=1`, so results
  from pull-request code are never published into it.
- A trusted canary worker may enable `bellowsd --enable-execution --auth-token
  ...`; the server rejects clients whose exact rustc/Cargo/host identity differs
  and single-flights identical requests. This demonstrator is not suitable for
  hostile multi-tenant execution.
- `bellows analyze snapshot/compare` can attach advisory affected-package JSON
  to CI. Its syntactic surface result is diagnostic only and must never skip a
  required Manifold lane.

# Exact-diagnostics tests

Compiler messages name real paths: Bellows remaps paths in objects and
debuginfo for portable results, and rewrites rustc's diagnostics back to the
receiving checkout's paths, live and on replay. Compiler-output snapshot tools
such as `trybuild` therefore pass through the wrapper, including notes that
quote a restored dependency's source: every virtual path names one directory
for the whole session (`/bellows/checkout/crates/x`, `/bellows/cargo-home/…`),
so rustc maps it back to the local file.

GPU/browser execution boundaries remain unchanged throughout: compilation and
archives may move, but RTX 5080 golden/pixel tests execute on the GPU runner.
