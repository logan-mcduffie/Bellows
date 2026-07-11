# Bellows five-phase demonstrator

## Definition of completion

This milestone turns every phase in `bellows-concept.md` into an executable,
end-to-end vertical slice. “Complete” here means each architectural capability
works locally, composes with the earlier phases, has a machine-checked demo, and
states its safety boundary. It does not mean the managed multi-tenant service,
global infrastructure, or experimental rustc changes are production-ready.

## Shared declared-action model

Phases 2–4 share an immutable declared-action record:

- normalized command, platform/toolchain identity, declared environment;
- relative input paths and BLAKE3 content digests;
- expected relative outputs and their CAS digests;
- normalized stdout/stderr, exit status, duration, and executor identity.

Paths must remain beneath the declared workspace. Cacheable local actions run
in an invocation-private sandbox containing only declared inputs. Remote actions
materialize the same input manifest into a server-side sandbox. Successful
outputs are uploaded before the immutable action record becomes visible.

The command must use the bare, attested `cargo` or `rustc` executable. Absolute
arguments, parent traversal, and path-qualified wrapper executables are rejected
by both client and server. This prevents a declaration from reaching back into
the producer workspace or substituting an unattested compiler wrapper.

The sandbox has one allowed read-only ambient root: the selected Rust toolchain.
Its identity is part of the action key: exact `rustc -vV`, exact `cargo -vV`,
and host OS/architecture. Each action receives a synthetic empty `CARGO_HOME`,
so ambient Cargo configuration and credentials are neither readable nor able to
change outputs. Cacheable Cargo commands must use `--locked --offline` and use
only path dependencies or vendored sources supplied as declared inputs;
workspace `.cargo/config.toml` is a normal declared input. The demonstrator uses
dependency-free/path-only workspaces. Network-capable or otherwise ambient
commands are not admitted as hermetic cache entries.

The trusted demonstrator does not trace arbitrary host tools launched internally
by `build.rs`. Actions invoking tools beyond the attested Rust/Cargo/linker image
must treat that worker image or the tool binaries as additional declared inputs;
production admission requires tracing or a hardened image identity.

The explicit action model is conservative by construction: undeclared inputs
are absent from the sandbox, missing outputs fail the action, and unsuccessful
commands are never cached.

## Phase 1: production-minded compiler cache

Extend the existing rustc action cache with:

- optional verified client L1 storage;
- remote `HEAD` checks so existing blobs are not uploaded again;
- cache-origin events distinguishing L1 and remote hits;
- server retention/garbage-collection operations and storage limits;
- durable operational statistics.

Acceptance: cold remote publication, L1 hit, remote hit after clearing L1,
deduplicated upload statistics, corruption recovery, and bounded storage.

## Phase 2: compile once, test many

Add immutable named archives backed by the CAS. A producer publishes a directory
tree (for example a nextest archive); any number of executors restore it without
compiling. Archives preserve paths and executable permissions and have an exact
input/action provenance record.

An archive name is a publish-once alias for a content-addressed tree digest.
Publishing different content under the same name fails; versioned producers use
a different name. Manifests contain regular files only. Absolute paths, parent
components, platform prefixes, and symlinks are rejected during both publication
and restore.

Add a generalized local declared-action command which can build and restore
Manifold-style component outputs in a sandbox.

Acceptance: compile a test executable once, publish an archive, remove all local
build outputs, restore it for two independent executors, and run both without
invoking Cargo again.

## Phase 3: Cargo action cache

Use declared sandbox actions to cache outputs outside the rustc boundary:

- final linked binaries and test programs;
- generated build-script products;
- nested Cargo/component commands.

The MVP requires explicit input/output declarations rather than pretending an
arbitrary Cargo project is hermetic. A later tracer can generate declarations;
the declared manifest remains the correctness boundary.

Acceptance: a sandboxed Cargo build produces a final binary on a miss and
restores it on a fresh workspace hit; a nested generator action does the same;
changing a declared input invalidates both precisely.

## Phase 4: remote execution

Add an opt-in execution endpoint to `bellowsd`. The server schedules requests
through a bounded semaphore, materializes declared inputs into a temporary
workspace, clears the environment except for an explicit execution baseline,
runs the command, captures outputs, uploads them to the CAS, and returns the
immutable record. Cache hits skip scheduling.

Every request carries the client's platform/toolchain identity. Before running,
the executor independently computes its identity and requires an exact match;
a mismatch is an explicit non-execution response and cannot publish a record.
Enabling execution requires bearer authentication.

This is process isolation for a trusted single tenant, not a hardened hosted
sandbox. Production multi-tenancy still requires containers/microVMs, network
policy, resource accounting, secret isolation, and worker attestation.

Acceptance: a client with no Rust target directory uploads sources, the server
compiles remotely, the client restores and runs the binary, a second request is
a cache hit, and concurrent identical requests execute once.

## Phase 5: compiler-aware research

Add advisory workspace snapshots and impact analysis:

- Cargo package/dependency graph;
- per-package complete source digest;
- advisory syntactic-public-surface digest for Rust `pub` declarations;
- comparison explaining source changes, public-surface changes, affected
  downstream packages, and potential “relink, do not rebuild” candidates;
- critical-path estimates derived from Bellows event durations.

The syntactic surface digest is research telemetry and never authorizes a cache
hit. It does not model generic/`#[inline]` bodies, default trait methods, exported
macros, or other body-level downstream effects; this caveat is embedded in JSON
output. Actual downstream skipping requires compiler-produced stable metadata
or a rustc change, exactly as the concept document anticipates.

Acceptance: changing a private implementation reports the package changed but
downstream public API stable; changing a public signature reports the complete
affected downstream closure. JSON output is suitable for a future dashboard.

## Showcase

`scripts/five-phase-demo.sh` starts one Bellows service and proves all five
phases in sequence using disposable workspaces. Every headline claim is asserted
from structured output or executable results. The existing Phase 1 demo remains
the fast correctness regression suite.
