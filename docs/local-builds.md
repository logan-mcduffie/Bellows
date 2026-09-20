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

## What gets faster

Bellows complements Cargo rather than replacing its local fingerprints:

- Cargo skips artifacts already present in the selected `target/`.
- Bellows restores eligible rlibs, metadata, and dep-info when Cargo invokes
  rustc after a clean, branch switch, worktree change, or target-dir change.
- Exact declared actions restore complete output trees.
- Final binaries, procedural-macro consumers, native/external codegen inputs,
  and incremental rustc sessions remain conservative bypasses.

Consequently, changing `flagship` itself still requires its final compile and
link. The largest gains come from recovering its unchanged dependency graph and
the eligible libraries compiled by `flagship-game-bundle`'s nested release
WASM build. That build script retains `RUSTC_WRAPPER`, so no Manifold-specific
integration is required.

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

Version 0.2.1 stores cache objects in `store-v5` below this state directory, leaving
older caches intact. Each wrapped build has an ID; `--latest`, `--session ID`,
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
