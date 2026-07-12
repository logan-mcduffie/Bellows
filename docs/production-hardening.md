# Production hardening for Manifold dogfood

## Objective

Make Bellows dependable for Manifold's private, single-tenant CI cache while
preserving the repository as an independent Cargo-native build project. This
milestone strengthens the existing compiler-cache, CAS, server, and operational
boundaries. It does not claim hostile multi-tenancy, public internet service,
or production remote execution.

## Non-negotiable invariants

1. Cache infrastructure must never turn a cacheable rustc invocation into a
   failed build when ordinary rustc could have run locally.
2. A cache hit is accepted only after manifest shape, action identity, every
   dependency, every stream length, and every blob digest validate.
3. Publication is blobs first, immutable record last. Crashes may waste work,
   but may not make a partial result visible as a hit.
4. Restores never traverse a declared root, follow a symlink, accept duplicate
   paths, or materialize an undeclared compiler output.
5. Concurrent clients, cancellation, server restart, garbage collection, and
   corrupt on-disk data produce safe misses or explicit administrative errors;
   they never produce stale artifacts.
6. Binding a server beyond loopback requires authentication unless an operator
   deliberately acknowledges an unsafe override.
7. Storage and request resources are bounded and observable. Shutdown under
   SIGINT or SIGTERM is graceful.
8. Exact-diagnostics compiler fixtures remain wrapper-free; GPU execution stays
   outside Bellows.

## Workstreams

The implementation is deliberately ordered. Every task lands with its own
regression tests; verification is not deferred to a final testing phase.

## Ordered implementation

1. **Repair the current record format, then bump the protocol.** Compiler stream
   digests are computed from path-normalized bytes, but protocol 2 records their
   pre-normalization lengths. Record normalized lengths, move the compiler
   action-key algorithm from the CLI into `bellows-core`, bump
   `PROTOCOL_VERSION`, and test that protocol-2 records are clean misses. The
   first hardened Manifold run is intentionally cold.
2. **Make the wrapper unconditionally fail open.** Add tests for an unreachable
   service, malformed server URL, corrupt L1 index, corrupt remote candidate,
   and publication failure, then contain those errors around the cache boundary
   so official rustc still determines the build result.
3. **Enforce complete compiler manifests.** Fail capture if any dep-info input
   disappears; stage and validate complete candidates before materialization;
   enforce canonical keys, recomputed action keys, normalized stream lengths,
   unique paths/names, protocol, cardinality, and safe normalized input paths.
4. **Make store mutation and GC crash/concurrency safe.** Add cross-process
   mutation locking, immutable race tests, directory fsync, orphan-temp cleanup,
   symlink/malformed-record handling, and publication-vs-GC exclusion.
5. **Harden server policy and resource behavior.** Add non-loopback auth refusal,
   explicit unsafe override, constant-work token checks, sanitized 500s, bounded
   request/concurrency settings, startup validation, readiness, and SIGTERM.
6. **Update the shipped container and operator contract atomically with task 5.**
   The image must no longer default to an unauthenticated `0.0.0.0` service.
   Document token injection and unsafe development mode; verify authenticated
   start, health, persistent restart, and graceful `docker stop`.
7. **Strengthen repository gates and run the adversarial matrix.** CI runs exact
   MSRV format/clippy/tests, both demos, dependency audit, release build, and the
   failure/race/restart cases introduced with tasks 1–6.
8. **Dogfood only after tasks 1–7 are green.** Audit the diff, publish a Bellows
   candidate commit, and pin Manifold's integration branch to that exact commit.
   Require cold/warm WarpBuild, full CPU, exact-diagnostics, browser, canary, and
   RTX 5080 evidence before merging Bellows and finalizing Manifold's pin.

### 1. Manifest and key validation

- Require canonical 64-character lowercase BLAKE3 keys everywhere.
- Move compiler action-key computation into `bellows-core`, then recompute the
  action key server-side from the candidate dependency and environment manifest.
- Correct normalized stdout/stderr lengths and bump the protocol before strict
  length validation is enabled.
- Reject duplicate files, duplicate environment names, duplicate artifacts,
  invalid normalized input paths, inconsistent stream lengths, unsupported
  protocol versions, and unreasonable manifest cardinalities.
- Treat a dependency named by rustc dep-info that disappears during capture as
  an uncacheable result instead of silently omitting it.

### 2. Client fail-open behavior

- Contain every L1/open/index/restore/network/lease/publication failure around
  the cache boundary and fall back to the already-selected official rustc.
- Never replay or retain a partially restored output set after candidate
  validation fails; stage downloads before atomic materialization.
- Make diagnostics/event-log corruption non-fatal and make each event append a
  single record.
- Bound configurable wait and HTTP timeouts and validate server URLs early in
  user-facing commands while keeping wrapper fallback intact.
- Test protocol skew in both directions: a hardened client with the prior
  server and the prior client with the hardened server must fail open cleanly.

### 3. Durable local store

- Serialize read-modify-write record publication across processes.
- Make immutable declared/archive conflicts deterministic under races.
- fsync containing directories for crash-durable publication where supported.
- Refuse or quarantine symlinks and malformed record files during traversal.
- Make GC mutually exclusive with publication, preserve every live reference,
  clean orphan temporary files, and report corrupt records rather than silently
  treating them as valid state.

### 4. Server operations and security

- Require auth for non-loopback listeners by default; retain an explicit unsafe
  development override.
- Compare bearer tokens without data-dependent early exit and avoid returning
  internal filesystem details in HTTP 500 responses.
- Add separate bounded JSON/blob request limits, request concurrency limits,
  and validation before filesystem work.
- Handle SIGTERM as well as Ctrl-C, reject nonsensical zero limits at startup,
  and expose readiness/storage-health information.
- Keep remote execution disabled by default and document that it is trusted
  single-tenant only.
- Change the Docker entrypoint and operator instructions in the same commit as
  the non-loopback authentication policy, then test start/health/SIGTERM.

### 5. Verification and operability

- Add adversarial unit/integration tests alongside each implementation task for
  traversal, malformed keys and manifests, corruption, publication/GC races,
  cancellation, server restart, auth policy, and offline fallback.
- Run format, clippy, workspace tests, both end-to-end demos, dependency audit,
  minimum-supported Rust, and a release build in CI.
- Add container health/shutdown guidance, cache backup/restore guidance, a
  protocol compatibility policy, and a concise operator runbook.
- Dogfood the hardened commit through Manifold on cold and warm WarpBuild runs,
  measure hit/fallback/corruption counts and cache size, then update Manifold's
  immutable Bellows pin only after all required CPU and GPU gates pass.

## Completion evidence

Accepted on 2026-07-11 against Bellows
`f0d9ed1187f37c578a15f3db2ba23cee39350b66` and Manifold PR
[#133](https://github.com/Manifold-Game/manifold/pull/133). Cold and exact-ref
warm WarpBuild runs, the full CPU matrix, browser parity, the Bellows production
canary, and retained RTX 5080 gates all passed. Cold/warm compiler telemetry
reported zero fallback and corruption events.

- Every repository gate and adversarial test passes repeatedly.
- Forced server loss, corrupt blobs/indexes, malformed manifests, and cancelled
  owners all lead to safe local work or bounded retry.
- A fresh WarpBuild VM restores Manifold compiler outputs from a prior run with
  zero unexpected fallbacks.
- Manifold's full `ci-full` workflow, browser parity workflow, Bellows canary,
  and retained RTX 5080 jobs pass against the immutable hardened commit.
- Remaining limitations are explicit and do not undermine Manifold's current
  single-tenant use.
