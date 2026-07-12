# Bellows

**Cargo-native remote builds. Keep Cargo. Keep official rustc. Skip work the
fleet already did.**

Bellows is a working five-phase remote-build demonstrator for Rust. It combines
a Cargo-compatible compiler cache, immutable test archives, declared Cargo
action caching, authenticated remote execution, and advisory compiler-aware
impact analysis over one verified content-addressed store.

```text
cold runner A                 bellowsd                 fresh runner B
    rustc  ── publish ──>  action index + CAS  <── restore ── Cargo
      │                           │                              │
 official compiler         verified BLAKE3 blobs          local final link
```

## Try the five-phase showcase

Requirements are Rust 1.92+ and `curl`. Then run:

```bash
./scripts/five-phase-demo.sh
```

The showcase proves, with machine-checked assertions:

1. Runner-local L1 and fresh-runner remote compiler hits.
2. Compile-once/test-many archives restored to two independent executors.
3. Cached final links, build-script output, and a nested Cargo generator.
4. Authenticated, toolchain-attested, single-flight remote Cargo execution.
5. Advisory private/public-surface change analysis and downstream closure.
6. Publish-once archive safety, precise invalidation, reference-aware GC, and
   publication-race protection.

For the shorter compiler-cache correctness suite, run `./scripts/demo.sh`. It
proves these seven Phase 1 behaviors in a disposable workspace:

1. A cold runner compiles libraries with official `rustc` and publishes them.
2. A fresh runner restores `.rmeta`, `.rlib`, and dep-info remotely.
3. Restarting `bellowsd` retains the same durable cache.
4. Editing a transitive `mod` file produces a safe miss naming that exact file.
5. Two concurrent cold runners compile each identical library action once.
6. Corrupted remote blobs fail integrity checks and are rebuilt, never used.
7. Stopping `bellowsd` still leaves a successful ordinary Cargo build.

Nothing from the demo is retained outside its temporary directory.

## Manual use

Build and start the server:

```bash
cargo build --workspace --bins
./target/debug/bellowsd --listen 127.0.0.1:7878 --data-dir .bellows/server
```

In another terminal, from any Rust workspace:

```bash
/path/to/Bellows/target/debug/bellows doctor
/path/to/Bellows/target/debug/bellows run -- cargo build
rm -rf target
/path/to/Bellows/target/debug/bellows run -- cargo build
/path/to/Bellows/target/debug/bellows stats
/path/to/Bellows/target/debug/bellows explain
```

The second build uses a completely empty local target directory. Bellows checks
its verified runner-local L1 before the organization cache; set `BELLOWS_L1=0`
when measuring remote behavior in isolation.

For authentication, start `bellowsd` with `--auth-token` or
`BELLOWS_AUTH_TOKEN` and give the client the same environment variable. A
non-loopback listener refuses to start without authentication. Server data is
durable across restarts. See the [operator runbook](docs/operations.md) before
running a shared service.

## What is safe to cache today

The MVP caches compiler-produced Rust libraries and metadata when it can prove
an unambiguous action identity. Its key covers:

- exact `rustc -vV` identity;
- normalized compiler arguments and relevant environment;
- primary sources and explicitly supplied `--extern` artifacts;
- all transitive files learned from rustc dep-info;
- all `env!` and `option_env!` values reported as dep-info env dependencies;
- Cargo's unique `-C extra-filename` output identity.

The static key includes compiler-, Cargo-, target-, and native-toolchain
environment variables, but deliberately excludes per-run CI metadata such as
`GITHUB_RUN_ID`. Arbitrary variables actually consumed through `env!` or
`option_env!` remain safe because rustc reports and Bellows revalidates them in
the candidate manifest.

A static command identity retains several dependency manifests, allowing old
branches to become hits again. Every candidate is revalidated before restore.
Every downloaded blob is rehashed. Dep-info and compiler streams are normalized
against workspace, target, Cargo, Rustup, and home roots, then localized on the
receiving runner.

The transparent rustc wrapper conservatively bypasses final binaries, tests, examples, procedural
macros, dynamic/static libraries, compiler probes, incremental sessions, and
ambiguous emit sets. Link arguments are inert for the cached library-only
outputs and remain part of the cache key. Native search paths/libraries,
explicit linkers, response files, unstable flags, and external codegen inputs
still bypass until modeled. Final binaries,
build-script products, and nested Cargo output can instead use `bellows action
run`, whose explicit input/output manifest and isolated offline sandbox become
the correctness boundary. A bypass is visible, not a silent correctness bet.

Injected `--remap-path-prefix` flags make library metadata checkout-independent
but also cause rendered compiler diagnostics to use `/bellows/workspace` paths.
Cargo metadata can itself encode path-dependency package IDs, so fleets should
still use consistent checkout layouts for the best hit rate.

## Commands

| Command | Purpose |
|---|---|
| `bellows run -- <command>` | Run Cargo-compatible work with the wrapper installed |
| `bellows doctor` | Verify protocol, server, compiler, and fallback |
| `bellows stats` | Show CAS size, actions, leases, hits, misses, and bypasses |
| `bellows explain` | Show recent invalidation and fallback reasons |
| `bellows archive publish/restore` | Distribute immutable compile-once/test-many trees |
| `bellows action run` | Cache a declared local Cargo/rustc action and final outputs |
| `bellows remote run` | Schedule the same declared action on an authenticated executor |
| `bellows analyze snapshot/compare` | Explain advisory source/API and downstream impact |
| `bellows gc` | Evict old records and unreferenced blobs to a storage budget |
| `bellowsd` | Run the durable HTTP CAS/action-cache service |

Configuration is available as flags or `BELLOWS_SERVER`, `BELLOWS_AUTH_TOKEN`,
`BELLOWS_DATA_DIR`, `BELLOWS_LISTEN`, `BELLOWS_MAX_BLOB_MB`, and
`BELLOWS_MAX_REQUESTS` environment variables. The default per-blob upload
ceiling is 512 MiB and the default concurrent request ceiling is 128.
Client connection and overall request timeouts can be bounded with
`BELLOWS_CONNECT_TIMEOUT_MS` (100–30,000) and `BELLOWS_REQUEST_TIMEOUT_MS`
(1,000–600,000). `BELLOWS_MAX_WAIT_MS` bounds single-flight waiting from zero
to 600,000 ms.

## Manifold

Manifold can replace its `setup-sccache` seam while retaining its stable,
clippy, browser-stable, threaded-nightly, and GPU target namespaces. The
composite action in this repository performs that setup for a preinstalled
Bellows binary. See [the Manifold integration guide](docs/manifold.md).

The first gains come from sharing stable Rust library work across fresh cloud
runners and suppressing simultaneous cold duplication. GPU execution remains
on the RTX 5080; Bellows only supplies portable compiler products there.
The [validation record](docs/validation.md) includes the two-run smoke,
full-pipeline cold/warm WarpBuild dogfood, browser parity, production canary,
RTX 5080 evidence, and the exact local acceptance commands.

## Architecture and scope

The workspace contains:

- `bellows-core`: protocol, manifests, normalization, digesting, and filesystem CAS;
- `bellows`: user command, rustc wrapper, cache client, diagnostics, and fallback;
- `bellowsd`: authenticated Axum service, durable action index, CAS, and leases.

The [product concept](bellows-concept.md) describes the commercial and
compiler-internal destination. The [five-phase plan](five-phase-plan.md) records
the implemented demonstrator's correctness boundaries and acceptance gates.

Bellows' compiler cache is hardened for Manifold's trusted single-tenant use,
but it is not yet a generally managed multi-tenant service. In particular, it
does not provide TLS termination, tenant isolation, account quotas,
microVM sandboxing, or a stable public protocol. Remote execution is disabled by
default, requires bearer authentication, and provides trusted-single-tenant
process/workspace isolation—not a hostile multi-tenant security boundary.
Declared Cargo actions require `--locked --offline`, a synthetic credential-free
`CARGO_HOME`, and path-only or explicitly vendored dependencies.
The client and server reject path-qualified cargo/rustc wrappers, absolute
arguments, and parent traversal. Build scripts invoking additional host tools
remain a trusted-worker-image boundary until tracing or worker attestation is
expanded beyond the Rust/Cargo/linker toolchain.

Phase 5 computes a syntactic public-surface heuristic. Its JSON embeds the
generic/inline/default-method/macro caveat, and it never authorizes a cache hit;
stable downstream skipping still requires compiler-produced metadata.
The current acceptance suite and Manifold integration target Linux runners;
Windows dep-info and artifact behavior have not yet been qualified.

The wire and on-disk formats are versioned but not yet stable. Review the
[protocol policy](docs/protocol.md) before upgrading a running service.
