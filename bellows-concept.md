# Bellows: Cargo-native remote builds

> Status: product concept / incubator document  
> Working title: **Bellows**  
> One-line pitch: **Bazel-class build reuse for Rust CI without leaving Cargo.**

## Executive summary

Rust CI repeatedly performs expensive work across jobs, runners, branches, and
commits. `sccache` helps, but its unit of reuse is primarily a cacheable `rustc`
invocation. It does not provide a complete Cargo action cache: final binaries,
link steps, Cargo fingerprints, build-script state, test archives, and many
procedural-macro or nested-build outputs remain outside its useful boundary.

Bellows would be a Cargo-compatible remote build platform that makes ephemeral
runners share durable build knowledge. It would begin as a `rustc` wrapper and
remote content-addressed cache, then grow into compiler-aware action caching,
fleet-wide build deduplication, remote execution, compile-once/test-many
workflows, and explainable invalidation.

The project should not begin as a new Rust compiler. The initial objective is to
avoid asking official `rustc` to repeat work, while retaining canonical Rust
semantics and ordinary Cargo commands.

The strongest strategy is open-core:

- Open-source the client, local cache, cache-key inspection, and a useful
  self-hosted server.
- Sell managed storage, remote execution, autoscaling, analytics, security,
  BYOC deployment, support, and an SLA.
- Use Manifold as the first dogfood workload and public benchmark.

## Why this exists

Rust's compilation model combines an expressive frontend, monomorphization,
LLVM code generation, native linking, procedural macros, and arbitrary build
scripts. Large workspaces can therefore spend substantial time in several
different phases, not one universally slow compiler stage.

The important CI observation is:

> A runner may be ephemeral, but the build service does not have to be.

A fresh VM should not imply a fresh build when an equivalent action has already
completed elsewhere in the fleet.

The Rust project's 2025 compiler-performance survey found that incremental
rebuilds were the most common performance complaint, CI performance was a major
blocker for a meaningful subset of CI users, and compile time contributed to
some developers leaving Rust. This validates the pain, though willingness to
pay for this particular solution must still be tested.

## Product thesis

Bellows should promise:

> Keep Cargo. Keep official rustc. Stop rebuilding work the fleet already did.

The defensible product is not merely hosted `sccache`. Existing CI providers
already offer that. Bellows becomes distinct by moving outward from compiler
invocations to the complete Rust CI action graph:

- Remote, content-addressed compiler artifacts
- Final binary and linker-result caching where safe
- Compile-once/test-many archives
- Build-script and nested-Cargo action caching
- Proc-macro execution reuse where correctness permits
- Fleet-wide single-flight compilation
- Critical-path-aware remote execution
- Invalidation explanations and cost attribution
- Cargo-native installation and local fallback

The aspirational positioning is:

> Bazel-class build performance without adopting Bazel.

## Non-goals

At least initially, Bellows is not:

- A replacement implementation of the Rust language
- A release-codegen backend that competes with LLVM
- A requirement to rewrite Cargo manifests as Bazel or Buck rules
- A shared writable `target/` directory
- A cache that guesses when correctness is uncertain
- A system that remotely executes hardware-dependent tests such as Manifold's
  GPU golden-image gates

## Architecture

The initial architecture is deliberately compatible with normal Cargo:

```text
GitHub/GitLab CI job
        |
        v
Bellows client / rustc wrapper
        |
        +---- local L1 cache
        |
        +---- remote content-addressed store
        |
        +---- action cache
        |
        `---- remote execution scheduler
                    |
                    `---- isolated compiler workers
```

Every result is addressed by the complete set of inputs that can affect it.
The minimum key space includes:

- Exact `rustc` identity, including compiler commit
- Compiler mode, such as `rustc`, `clippy-driver`, or rustdoc
- Host and target triples
- CPU features and target configuration
- Cargo profile, feature set, panic strategy, and codegen options
- Source and generated-input digests
- Dependency metadata and build-script outputs
- Relevant environment variables
- Native compiler, linker, SDK, and system-library identities
- Proc-macro host environment
- Paths after deterministic normalization

Human-friendly labels such as `stable` or `threaded` are useful for diagnostics,
but must not be the correctness boundary. The actual input key decides whether
two actions can share a result.

### Cache hierarchy

```text
runner-local L1
      |
regional/organization L2
      |
durable content-addressed storage
```

The client should compare estimated transfer cost with estimated recomputation
cost. Large artifacts should remain close to downstream remote actions when
possible rather than being downloaded after every step.

### Single-flight execution

If several jobs request the same missing action simultaneously, one worker
should execute it while the other callers await the same immutable result. This
turns a burst of identical cold misses into one compilation across the fleet.

### Correctness and fallback

Bellows must prefer a safe miss over an unsafe hit. If the service is
unavailable or an action cannot be modeled safely, the command should fall back
to ordinary local Cargo/rustc behavior.

Canonical release lanes should continue to use official rustc and LLVM. Faster
experimental backends such as Cranelift can be an opt-in validation lane, not a
silent substitute for production artifacts.

## How Bellows fits Manifold

Manifold is an unusually strong dogfood workload because it combines:

- A large Rust workspace
- Native and `wasm32-unknown-unknown` targets
- Stable and pinned-nightly toolchains
- Default and no-default-feature graphs
- Clippy, rustdoc, nextest, and compile-contract workloads
- Wasmtime, WGPU, networking, native libraries, and procedural macros
- Nested WASM/component builds launched through build scripts
- Gitignored test-mod components
- CPU, browser, and hardware-bound GPU test lanes

Manifold has already implemented several Bellows ideas locally:

- `CARGO_INCREMENTAL=0` to favor `sccache` reuse
- Path normalization with `SCCACHE_BASEDIRS`
- Persistent runner-local target namespaces
- LLD for native links
- A content-addressed test-mod cache
- CPU test partitioning and scarce-GPU isolation
- Explicit disk budgets and eviction

In other words, Manifold's current shell scripts and composite actions are a
small, workload-specific prototype of the more general service.

### Integration seam

The first integration point is
`.github/actions/setup-sccache/action.yml`. Rust-heavy jobs already use this
action, which ultimately configures `RUSTC_WRAPPER`.

The first Bellows version could replace that setup while leaving almost every
Cargo command unchanged:

```text
setup-sccache
      becomes
setup-bellows
```

Initially Manifold should retain:

- Rust 1.92.0 and the pinned threaded-build nightly
- `CARGO_INCREMENTAL=0`
- Existing feature and target separation
- LLD
- Existing test topology
- Existing GPU execution boundaries
- Local compilation fallback

### Compile once, test many

The native CPU matrix currently has unit, integration, and compile-contract
jobs that invoke Cargo/nextest independently. Bellows should support a producer
job that compiles the default-feature test set once and publishes an immutable
nextest archive:

```text
native-test-build
        |
        +---- unit executor
        +---- integration executor
        `---- compile-contract executor
```

Compile-contract tests that deliberately launch nested compilers remain
isolated executions, but their nested successful compiler work can also pass
through Bellows.

### Test-mod components

`scripts/build-test-mod-cached.sh` already implements a coarse content-addressed
action: hash inputs, lock population, restore on hit, build on miss, atomically
publish, and evict old results.

Bellows should generalize this into a precise action whose inputs include the
selected mod, reachable workspace crates, `cargo-mod`, WIT inputs, Rust and WASM
toolchains, transformation flags, and `wasm-tools`. Native integration and
browser-parity jobs could then share the same component result across runners.

### Nested builds and build scripts

`flagship-game-bundle` launches a nested WASM/component build from `build.rs`.
This should eventually become an explicit action subgraph rather than opaque
process work:

```text
compile builder
      |
build guest WASM
      |
component transformation
      |
generate bundle input
      |
compile native dependent
```

This is later-stage work because Cargo build scripts can execute arbitrary code
and observe undeclared inputs.

### Browser and GPU lanes

Manifold's stable browser, threaded-nightly browser, and native CI graphs must
remain distinct whenever their full inputs differ. The GPU runner can restore
portable compiler products and WASM artifacts, but pixel/golden tests must
execute on the RTX 5080. Native artifacts may be shared only when the target,
CPU, linker, native libraries, and platform image are equivalent.

## Why not start with a custom compiler?

A drop-in Rust compiler must reproduce parsing, macro expansion, name
resolution, type inference, trait solving, borrow checking, const evaluation,
monomorphization, metadata compatibility, diagnostics, target behavior, and
language evolution before it can compete on speed.

That is an organization-scale, multi-year effort. It also attacks only one part
of CI latency.

Bellows can instead preserve official rustc and attack repeated work around it.
A targeted rustc fork becomes reasonable only after measurements show that
whole-crate and whole-action reuse have exhausted their gains.

Possible later compiler changes include:

- Stable hashes for exported crate metadata
- Separately cacheable frontend and codegen products
- Portable incremental query state
- Cached proc-macro expansion
- Remote execution of codegen units
- Better invalidation explanations
- Compiler-server mode
- "Relink, do not rebuild" support when an upstream change is not observable

## Why this has not already won

Most pieces have been built before. `sccache` provides remote compiler caching
and distributed compilation. Bazel `rules_rust` and Buck2 can execute Rust build
actions against remote caches and workers. BuildBuddy, EngFlow, Depot, and
Namespace monetize adjacent caching, runner, and remote-execution products.

The remaining Cargo-native gap exists because several hard constraints meet in
one place.

### Cargo is not inherently hermetic

`build.rs` programs can inspect files, environment variables, Git state, native
libraries, the system clock, the network, and arbitrary processes. Declarations
such as `rerun-if-changed` can be incomplete. Compatibility with arbitrary Cargo
projects and strict remote-execution hermeticity pull in opposite directions.

### Rust artifacts are environment-sensitive

Correct reuse may depend on the compiler commit, host and target platforms,
features, flags, paths, native libraries, linker, proc-macro host, and generated
inputs. Exact keys are possible, but fragment the cache.

### Compiler query state is not a ready-made distributed cache

Rustc's incremental system persists selected query and codegen information for
another compiler session. Turning that state into a portable, concurrent CAS
requires stable serialization, path independence, exact compiler compatibility,
dependency identity, and careful transfer economics.

### Cache correctness is asymmetric

Over-invalidation wastes time. Under-invalidation can silently ship a stale or
incorrect binary. A product must be conservative, observable, and extremely
trustworthy.

### Network and storage can erase the gain

Rust artifacts are large. Downloading an artifact can cost more than rebuilding
it. Useful remote execution needs local caches, regional placement, compression,
chunking, lazy materialization, and cost-aware scheduling.

### Multi-tenant execution is hostile

Build scripts, procedural macros, and dependencies are untrusted code. A hosted
service must prevent worker escape, cache poisoning, secret leakage, artifact
disclosure, and cross-tenant side channels. Tenant isolation reduces global
reuse, creating a product/economics tradeoff.

### Distribution cannot remove the critical path

Independent crates can compile in parallel, but one large, high-fan-out crate
can block the graph. Scheduling must prioritize the critical path and avoid
duplicate work, but cannot invent parallelism.

### Existing complete solutions impose migration cost

Bazel and Buck2 solve much of the action-graph problem by changing the build
model. Many Rust teams do not want duplicate build definitions or different
local and CI semantics. Cargo compatibility is therefore both the opportunity
and the difficult constraint.

There is no single fatal technical reason Bellows cannot work. The challenge is
delivering correctness, compatibility, security, and positive transfer
economics at the same time.

## Market and business thesis

The pain is broad, but willingness to pay is concentrated.

### Likely users

- Game engines and WGPU applications
- WASM and Wasmtime-heavy systems
- Databases and data infrastructure
- Blockchain nodes
- Browsers and developer tools
- Embedded and cross-platform workspaces
- Proc-macro-heavy SaaS backends
- Large Rust monorepos with frequent pull requests

Individual developers are likely to star and self-host the project. Engineering
managers and platform teams with private repositories, long CI queues, and
meaningful compute or developer-wait costs are the likely buyers.

### Open-core split

The open-source layer should include enough value to earn trust and adoption:

- Cargo/rustc client
- Local cache
- Cache-key and invalidation inspection
- Useful self-hosted single-node server
- Standard protocol support where practical
- Graceful local fallback

The hosted product can monetize:

- Durable, regional content-addressed storage
- Remote compilation workers and autoscaling
- Fleet-wide single-flight execution
- Retention and eviction policy
- Build and test analytics
- Organization access control, SSO, and audit logs
- Tenant isolation and encrypted storage
- BYOC/VPC data planes
- Support and SLA

### Value demonstration

The service should translate cache mechanics into business outcomes:

```text
4,218 compilations avoided
611 runner-hours saved
median PR feedback: 31m -> 9m
estimated compute savings: $X
estimated developer wait avoided: Y hours
```

Latency reduction may be more valuable than direct runner savings.

### Competitive boundary

Hosted `sccache` alone is not a durable differentiator. The product becomes
interesting when it safely caches and schedules work that existing compiler
caches miss, while remaining easier to adopt than Bazel or Buck2.

The long-term market can grow from "Rust cache" into a Cargo-native remote build
platform. Rust-only scope may support a strong bootstrapped business; a larger
company likely requires expanding the platform, customer base, or language
coverage without losing the Rust-first advantage.

## Open-source and Manifold strategy

Bellows should live in a separate repository and brand. Manifold should be the
origin story, dogfood client, and benchmark rather than the package namespace.

A useful public story is:

> Bellows was created to make Manifold's multi-target, 4,000-test native/WASM/GPU
> pipeline tolerable.

This can earn Manifold technical credibility and attention, though build-system
users will not automatically become engine contributors or players. Bellows may
develop a larger identity than Manifold; separating the repositories lets that
be an asset instead of a source of product confusion.

## Phased execution

### Phase 0: Measure

- Capture Cargo timings and every rustc invocation.
- Separate frontend, codegen, linking, build-script, proc-macro, and test time.
- Record cold-local, warm-local, cold-remote, and warm-remote scenarios.
- Identify duplicate work across Manifold jobs and runners.

### Phase 1: Cargo-compatible remote compiler cache

- Build the wrapper/client and local L1.
- Use a remote CAS with exact toolchain and environment keys.
- Add authentication, integrity verification, observability, and local fallback.
- Dogfood it through Manifold's existing setup action.

This phase must already provide better visibility and ergonomics than manually
configuring hosted `sccache`, even before it surpasses sccache's cache boundary.

### Phase 2: Compile once, test many

- Produce immutable nextest archives.
- Distribute test executors independently from compilation.
- Generalize Manifold's test-mod component cache.
- Add fleet-wide single-flight execution.

### Phase 3: Cargo action cache

- Model link actions and final test binaries.
- Sandbox and trace build-script inputs.
- Mark non-hermetic actions uncacheable or require declarations.
- Model nested Cargo invocations as explicit subgraphs where possible.

### Phase 4: Remote execution

- Add isolated compiler workers.
- Schedule according to critical path, transfer cost, locality, and historical
  duration.
- Keep hardware-dependent test execution on appropriate runners.
- Offer managed and BYOC data planes.

### Phase 5: Compiler-aware research

- Explore metadata-stable downstream skipping.
- Explore portable incremental/query caching.
- Explore remote codegen units or a persistent compiler server.
- Fork rustc only when a measured bottleneck and credible maintenance plan
  justify it.

## Validation gates

Before treating Bellows as a company rather than an internal or open-source
project, validate all of the following:

- Installation takes less than roughly fifteen minutes.
- A completely fresh runner benefits from a warm remote cache.
- Representative repeat PR builds improve by at least 30-50 percent.
- There are no incorrect cache hits.
- Transfer and storage costs leave healthy gross margin.
- At least three outside teams keep it installed.
- At least two outside teams agree to pay meaningful money.
- Users request hosted storage, compute, security, or support rather than only
  additional free client features.

Recruit five to ten design partners before undertaking compiler-internal work.
Ideal partners have private Rust repositories, builds longer than ten minutes,
several CI jobs per pull request, and someone who owns developer productivity.

If developers star Bellows but will not connect private CI or pay, it is a
valuable open-source project rather than a validated business. If teams provide
CI traces, endure security review, and pay for the service, the business signal
is real.

## Success metrics

- End-to-end workflow wall time, not merely compiler time
- Cold-runner performance with warm organization cache
- Cache hit rate by bytes and avoided compute time
- Single-flight duplicate executions avoided
- Critical-path duration
- Time and bytes spent uploading/downloading
- Build-script and link-action cacheability rate
- Incorrect-hit count, which must remain zero
- Installation time and fallback reliability
- Compute cost and gross margin per customer

## Naming

### Recommended working name: Bellows

A bellows accelerates a forge by feeding it air. It does not replace the forge;
it makes the forge work faster. That maps unusually well to a service that does
not replace Cargo or rustc, but feeds them cached artifacts and remote compute.

It is memorable, visual, Rust-adjacent without putting `rust` in the company
name, and broad enough to cover caching, execution, analytics, and eventually
other native toolchains.

Possible positioning:

- **Bellows — Cargo-native remote builds.**
- **Bellows — Keep Cargo. Skip the rebuild.**
- **Bellows — Rust builds, remembered.**
- **Feed the forge. Skip the wait.**
- **Never build the same work twice.**

Naming caveat: an unrelated `bellows` crate already exists for durable task
processing. That does not automatically prevent a company/product brand, but it
means the bare crate and likely CLI name are unavailable. `cargo-bellows`,
`bellows-client`, and `bellowsd` are plausible package names. Trademark, domain,
GitHub organization, package, and social-handle clearance must happen before a
public launch.

### Shortlist

| Name | Strength | Concern |
|---|---|---|
| **Bellows** | Best metaphor and product story | Existing unrelated Rust crate; needs legal/domain clearance |
| **Tuyere** | The nozzle that injects air into a furnace; distinctive and technically exact | Hard to spell and pronounce |
| **Forgewind** | Evokes accelerating the forge; likely brandable | Coined name, less immediately clear; collision check incomplete |
| **Cratewind** | Rust-specific and fast | "Crate" may limit expansion beyond Rust |
| **Firedraft** | Forced air and speed; broad | Less polished as a company name |

Names rejected during the initial collision pass:

- **Temper**: active programming language and toolchain
- **Smelter**: active Rust-based media product
- **Sinter**: already used by a build tool
- **Stoke**: established compiler-research/superoptimizer project
- **Quench**: active programming language
- **Anneal**: active Rust verification tool

## Open questions

- Does the first release speak the Bazel Remote Execution API, a smaller custom
  protocol, or both?
- Can Cargo's unstable build-analysis facilities provide enough action metadata
  without patching Cargo?
- Which final linker and test outputs can be made safely path-independent?
- How should build scripts declare hermetic inputs without making adoption feel
  like a build-system migration?
- Is organization-isolated reuse sufficient economically, or is verified
  cross-tenant reuse of public crates important?
- Does managed compute materially outperform fast hosted runners plus cache?
- Which Manifold job provides the clearest first public benchmark?

## References

- [Rust compiler performance survey 2025](https://blog.rust-lang.org/2025/09/10/rust-compiler-performance-survey-2025-results/)
- [Cargo: optimizing build performance](https://doc.rust-lang.org/cargo/guide/build-performance.html)
- [Cargo build cache](https://doc.rust-lang.org/stable/cargo/reference/build-cache.html)
- [Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html)
- [Cargo build timings](https://doc.rust-lang.org/beta/cargo/reference/timings.html)
- [rustc incremental compilation in detail](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation-in-detail.html)
- [sccache](https://github.com/mozilla/sccache)
- [Bazel rules_rust](https://bazelbuild.github.io/rules_rust/)
- [Buck2 remote execution](https://buck2.build/docs/users/remote_execution/)
- [Depot GitHub Actions runners](https://depot.dev/docs/github-actions/overview)
- [Namespace sccache integration](https://namespace.so/docs/integrations/sccache)
- [BuildBuddy](https://www.buildbuddy.io/)
- [EngFlow](https://www.engflow.com/product/remoteExecution)
- [Manifold CI documentation](https://github.com/Manifold-Game/manifold/blob/main/docs/ci.md)
- [Manifold CI compile-cache topology](https://github.com/Manifold-Game/manifold/blob/main/docs/superpowers/plans/2026-07-11-ci-compile-cache.md)
