# Local build acceleration

`bellows local` makes Bellows useful without CI infrastructure:

```bash
cargo build --release --manifest-path /path/to/Bellows/Cargo.toml --bins
cd /path/to/manifold
/path/to/Bellows/target/release/bellows cargo run --release -p flagship
```

`bellows cargo …` is the normal local interface. It expands to `bellows
local -- cargo …`; the explicit form remains available for non-Cargo commands
and cache-directory overrides.

The parent command performs store maintenance once, installs itself as Cargo's
`RUSTC_WRAPPER`, and marks the wrapper as local-only. Wrapper processes open
the store through a constant-time hot path and never construct the HTTP client.
Nested Cargo commands inherit the wrapper and local-only environment.

`bellows run` uses a `bellowsd` server instead (default
`http://127.0.0.1:7878`). If the server does not answer, every compile falls
back to plain, uncached rustc; `bellows run` says so before and after the
build. Set `BELLOWS_REQUIRE_SERVER=1` to make an unreachable server an error
instead, for example on a build machine that must never run uncached. On a
machine with no server, use `bellows cargo …`.

Cargo passes rustc its arguments in a response file (`@path`) when the
command line is too long for the platform, as `-Zbuild-std` builds are on
Windows. Bellows reads the file's arguments into the identity (the file's own
path is scratch) and compiles through a response file of its own.

## What gets faster

Bellows complements Cargo rather than replacing its local fingerprints:

- Cargo skips artifacts already present in the selected `target/`.
- Bellows restores rlibs, metadata, and dep-info when Cargo invokes rustc after
  a clean, branch switch, worktree change, or target-dir change. That includes
  crates that load procedural macros, crates that receive Cargo's propagated
  `-L native=` search paths, and crates built with `-C linker=`.
- Final links are restored too: binaries, `--test` harnesses, examples,
  `cdylib`/`dylib`, procedural-macro crates, and build-script executables. Their
  identity includes every file the linker reads (see
  [linked outputs](linked-outputs.md)).
- Build-script runs are restored: their `OUT_DIR` tree and Cargo directives.
- Profiles that pass rustc identical arguments share results, for example
  `release` and a `test-fast` profile that inherits it.
- Exact declared actions restore complete output trees.

A result is shared between worktrees unless its bytes embed a checkout path.
For example, a test that bakes `env!("CARGO_MANIFEST_DIR")` to find fixtures is
restored only in the checkout that produced it. `bellows stats` reports stored
results as *shareable* or *pinned to checkout*. Reading `CARGO_MANIFEST_DIR`
at run time (Cargo and nextest set it for test processes) makes such a test
shareable.

## Incremental edit loops

`bellows run` no longer forces `CARGO_INCREMENTAL=0`. An incremental rustc
invocation is looked up with its session directory removed from the key, so a
fresh worktree restores workspace members like any other crate. On a miss rustc
compiles with incremental enabled, in a per-unit session directory beneath
Cargo's own (`{crate}-bellows{extra}`, still removed by `cargo clean -p`).
Only a compile that started without a session is published. Edit-loop
recompiles that reuse a session stay in their checkout.

## Build-script runs

After a build script is compiled or restored, a Bellows launcher takes its
place and Cargo runs that instead. The launcher restores a verified earlier
run, or runs the real script and records it. Identity follows Cargo's rerun
model: the script binary, `$RUSTC -vV`, the resolved C/C++ drivers, the
relevant environment, and the declared `rerun-if-changed` paths (the whole
package when none are declared) and `rerun-if-env-changed` values. A
directory input counts by its recursive listing as well as its files'
contents, so adding, removing or renaming a file in it is a miss, as it is a
rerun for Cargo. (Before 0.3.8 only the files present when a run was recorded
were checked, so a file added later could restore a stale run; 0.3.8 records
build scripts under a new identity and never reads those runs.) Nested
Cargo target directories inside `OUT_DIR` (marked by `CACHEDIR.TAG`) are
scratch and are not stored. Only runs that began with an empty `OUT_DIR` are
published. Other host tools a script invokes are a trusted boundary, as for
declared actions. Set `BELLOWS_BUILD_SCRIPTS=0` to run scripts directly.
Without a Bellows session, the launcher simply runs the real script.

## Sharing a service with CI

`BELLOWS_READ_ONLY=1` restores verified results but never publishes. A CI job
on a developer's machine can consume the local service this way without
writing results produced by pull-request code into it.

## Storage

The default state directory is:

- `$BELLOWS_STATE_DIR`, when set;
- otherwise `$XDG_CACHE_HOME/bellows`;
- otherwise `~/.cache/bellows` on Unix;
- otherwise `%LOCALAPPDATA%/Bellows` on Windows.

Inspect and bound it with:

```bash
bellows stats --local
bellows stats --local --json
bellows gc --local --max-mb 20000
bellows explain --local --latest --summary
```

This version stores cache objects in `store-v6` below this state directory,
leaving older caches intact. Large file digests are memoized by file identity
in `digests-v1`, and entries unused for 14 days are pruned. Each wrapped build has an ID; `--latest`, `--session ID`,
and `--crate NAME` select relevant diagnostics. Logs rotate at 16 MiB with two
backups. See [diagnosing builds](diagnostics.md) for precise miss reasons and JSON.

Collection is reference-aware. If collection or manual damage leaves a
declared record without all of its blobs, the next local action removes the
stale record and executes normally. Compiler artifacts retain the existing
eight-candidate-per-static-key policy.

## Declared local actions

`bellows action run --local` uses the same sandbox and correctness boundary as
remote declared actions. Cargo commands require `--locked --offline`; inputs
and outputs must be workspace-relative, and environment inputs must be named
explicitly with `--env`.

Cargo resolves rustflags normally, including `RUSTFLAGS`, encoded flags, and
declared `.cargo/config.toml` settings; the remapping wrapper appends only its
own path remap. Declared output directories are replaced as complete trees,
so removed outputs cannot linger. Inputs and outputs must not overlap.

Build scripts can run arbitrary host tools. Their identities remain part of the
trusted local toolchain boundary unless explicitly represented by an input or
declared environment value. A safe miss is preferable to broad declarations
that omit relevant files.

## Validation

`./scripts/local-demo.sh` proves:

1. A live sentinel server receives no connection from local mode.
2. A release build restored into a wiped target directory produces local hits.
3. A transitive edit safely misses and changes the resulting binary.
4. Concurrent Cargo processes share one store without corruption or deadlock.
5. Declared actions hit, self-heal after a referenced blob is removed, and
   recover after zero-budget local collection.
