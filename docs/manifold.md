# Manifold integration

Bellows fits Manifold at the existing `.github/actions/setup-sccache` seam. No
Cargo command, test partition, toolchain pin, or hardware boundary needs to
change.

## Runner preparation

Build a release binary and install `bellows` on every runner image. Run one
organization-scoped `bellowsd` near the Hetzner runners, preferably behind TLS,
with a persistent data volume and bearer token:

```bash
bellowsd --listen 127.0.0.1:7878 --data-dir /var/lib/bellows
```

Expose its URL and token as `BELLOWS_SERVER_URL` and `BELLOWS_AUTH_TOKEN`
repository or organization secrets. The demonstrator server is single-tenant;
do not expose it directly to the public internet.

## Workflow migration

Copy `action.yml` from this repository into
`.github/actions/setup-bellows/action.yml`, then replace:

```yaml
- uses: ./.github/actions/setup-sccache
  with:
    target-namespace: clippy
```

with:

```yaml
- uses: ./.github/actions/setup-bellows
  with:
    server-url: ${{ secrets.BELLOWS_SERVER_URL }}
    auth-token: ${{ secrets.BELLOWS_AUTH_TOKEN }}
    target-namespace: clippy
```

The namespace map remains unchanged:

| Workload | Namespace |
|---|---|
| Stable native/WASM checks, builds, and CPU tests | `stable` |
| Clippy | `clippy` |
| Stable browser parity | `browser-stable` |
| Pinned-nightly threaded WASM | `threaded` |

The action retains `CARGO_INCREMENTAL=0`, stripped CI debug profiles, persistent
target directories, and the existing target-root separation between cloud and
GPU hosts.

## Expected first-phase result

- A fresh runner can restore Rust library metadata and rlibs produced by an
  equivalent runner.
- Simultaneous unit/integration/compile-contract cold misses converge through
  Bellows leases instead of compiling the same library independently.
- Stable and pinned-nightly, native and WASM, features, flags, and target triples
  remain distinct because their actual rustc invocations differ.
- Final test binaries and links still execute locally in this MVP.
- Crates loading procedural macros bypass until their arbitrary expansion inputs
  can be sandboxed or declared.
- GPU and browser pixel gates still execute on the RTX 5080; only eligible
  compiler products are restored there.

Use `bellows stats` after two identical full runs and compare end-to-end workflow
wall time, bytes restored, compiler hits, single-flight waits, and bypass
reasons. `bellows explain --json` is suitable for attaching diagnostics to CI
artifacts.

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
- Final link or nested component-build experiments can use the same declared
  action boundary while the transparent rustc wrapper continues to bypass
  unsafe link/proc-macro actions.
- A trusted canary worker may enable `bellowsd --enable-execution --auth-token
  ...`; the server rejects clients whose exact rustc/Cargo/host identity differs
  and single-flights identical requests. This demonstrator is not suitable for
  hostile multi-tenant execution.
- `bellows analyze snapshot/compare` can attach advisory affected-package JSON
  to CI. Its syntactic surface result is diagnostic only and must never skip a
  required Manifold lane.

GPU/browser execution boundaries remain unchanged throughout: compilation and
archives may move, but RTX 5080 golden/pixel tests execute on the GPU runner.
