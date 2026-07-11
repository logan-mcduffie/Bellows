# Bellows MVP implementation plan

## Outcome

Deliver a runnable Cargo-native remote compiler cache that demonstrates the
core Bellows promise on an ordinary Rust workspace: the first build compiles
with official `rustc`, a clean runner restores the same compiler actions from a
durable remote cache, changed inputs miss safely, and concurrent identical
misses are compiled only once.

## Architecture

Bellows is a Rust workspace with three layers:

- `bellows-core`: action identities, content digests, artifact manifests,
  filesystem CAS operations, configuration, and the HTTP protocol types.
- `bellows`: a user-facing command and Cargo-compatible `RUSTC_WRAPPER`. It
  executes official `rustc`, talks to `bellowsd`, restores immutable artifacts,
  emits structured diagnostics, and falls back locally when the service is
  unavailable. The first demo deliberately has no L1 so every reported hit is
  visibly remote.
- `bellowsd`: an authenticated HTTP service backed by an on-disk CAS and action
  index. It verifies uploaded content and coordinates leases for fleet-wide
  single-flight compilation.

The demo uses `bellows run -- cargo build` so existing Cargo commands do not
change. A GitHub composite action exposes the same integration seam as
Manifold's current `setup-sccache` action.

## Correctness boundary

The wrapper's static identity includes the exact compiler identity, normalized
invocation, target-relevant environment, primary source, and explicitly named
compiler inputs such as `--extern` artifacts. Successful misses also record the
complete rustc dep-info file set and every `# env-dep` value. A static identity
maps to multiple immutable candidates so ordinary branch churn can reuse any
previously observed dependency set. A candidate hit is accepted only after
every recorded file digest and environment value still matches.

Rustc writes directly into Cargo's output directory so its mid-compile artifact
messages and metadata pipelining retain normal semantics. Cargo's unique
`-C extra-filename` is required as the capture boundary; Bellows selects only
the exact `.rmeta`, `.rlib`, and `.d` names belonging to that invocation.
Probe invocations and actions without an unambiguous output identity pass
through untouched. Compiler stdout and stderr are stored in normalized form,
re-localized, and replayed on hits.

Paths are normalized against explicit workspace, target, Cargo-home,
Rustup-home, and home bases before keys or manifests are published. Restored
dep-info is re-localized to the current runner. Bellows injects deterministic
`--remap-path-prefix` arguments for the workspace and target roots so cached
Rust metadata does not retain checkout-specific paths. This is the equivalent
of Manifold's current `SCCACHE_BASEDIRS` contract, but part of the action model.

The MVP caches metadata and Rust library products (`.rmeta`/`.rlib`). It
conservatively bypasses binaries, tests, examples, proc macros, dylibs, cdylibs,
and static libraries because those may invoke a machine-specific linker. Link
actions become cacheable only after linker, SDK, native search paths, and native
library identities are modeled.

Invocations with `-C incremental` bypass the cache; `bellows run` sets
`CARGO_INCREMENTAL=0`, matching Manifold. Loading any `--extern` whose path has
the platform dynamic-library extension is treated as proc-macro execution and
also bypasses. Cargo metadata hashes can encode absolute path-dependency package
IDs, so different checkout layouts may safely miss even after Bellows path
normalization; consistent CI checkout roots provide the intended fleet reuse.

Unknown or unsafe invocations bypass the cache. Corrupt or unavailable cache
data becomes a local compile, never a build failure or an unchecked hit.

## MVP scope

- Remote content-addressed blob storage.
- Durable action candidates with integrity verification.
- Rustc wrapper compatible with stable Cargo.
- Best-effort remote leases and bounded waiter polling for single-flight
  compilation; timeout always falls back to local rustc.
- Optional bearer authentication.
- Local fallback, configurable timeouts, and conservative bypasses.
- Human-readable stats and miss explanations plus JSONL event logs.
- Self-contained demonstration workspace and script.
- Manifold-ready composite action and migration notes.

This MVP does not claim hermetic build-script caching, remote linker execution,
portable rustc incremental state, or hardware-test execution. Those remain
later phases in `bellows-concept.md`.

Procedural macros can perform untracked I/O during expansion. Since Cargo and
rustc dep-info cannot describe that arbitrary behavior, actions that load proc
macros are initially marked uncacheable. Relaxing that rule requires sandboxing
or a trace/declaration mechanism; it is not silently treated as safe.

## Acceptance gates

1. All Rust unit tests and the shell-driven end-to-end integration demo pass.
2. A cold demo build records misses and uploads artifacts.
3. Removing the demo `target` directory produces remote hits for cacheable
   Rust library/compiler actions; final binary linking still runs locally.
4. Editing a transitive Rust module produces a safe miss and the new behavior.
5. Two simultaneous cold requests share one lease; the waiter restores the
   owner's published action.
6. Deliberate blob corruption is detected and never restored.
7. With `bellowsd` stopped, Cargo still succeeds through local fallback.
8. The README provides a copy-paste demo and a concrete Manifold integration.
