# Diagnosing Bellows builds

`bellows cargo`, `bellows local`, and `bellows run` print a build ID and a short
summary on stderr after the wrapped command finishes. Cargo's stdout, including
JSON output, remains available for consumers. A failed cache setup warns and
runs the requested command normally; the command's exit status is preserved.

Start with the most recent wrapped build in the current workspace:

```bash
bellows explain --local --latest --summary
bellows explain --local --latest --crate manifold_render
bellows stats --local --latest --json
```

For a custom cache, add `--cache-dir /path/to/cache`. For a server-backed build,
omit `--local`; server stats still require the configured server. `explain`
itself only reads local diagnostics and works offline. Without explicit state
configuration, `explain` selects the newer of the workspace and default local
logs. `BELLOWS_EVENT_LOG` overrides log selection for both stats and explain.

Inspect an earlier or concurrent build using its printed ID:

```bash
bellows explain --local --session 1789850000000-12345 --summary --json
bellows explain --local --session 1789850000000-12345 --crate manifold_render --limit 100
```

`--latest` selects the most recently *started* wrapped build in this workspace,
even when an older concurrent build finishes after it. `--session` selects that
exact build. Without either option, queries include the retained log history.
An ordinary Cargo no-op build correctly reports zero compiler-cache decisions.
Direct `RUSTC_WRAPPER` use without a Bellows parent produces ungrouped events;
omit `--latest` to inspect those, or wrap the Cargo command with `bellows run`.

## What a miss means

| Reason | Evidence and next step |
|---|---|
| `input_changed` | Names the dependency or explicit compiler input whose digest changed. |
| `input_missing` | Names a previously recorded input that can no longer be read. |
| `environment_changed` | Names the changed environment variable; literal rustc-reported `env!` values are validated, including embedded absolute paths. |
| `compiler_changed` | The exact compiler version identity changed. |
| `identity_changed` | Lists protocol or argument-group differences with old/new hash prefixes. |
| `cold_identity` | No earlier diagnostic identity exists for this crate/source locally. Bellows cannot attribute the miss to a particular edit yet. |
| `entry_missing` | This static identity was observed before, but no usable record remains. Possible eviction or failed/never-completed publication is stated explicitly rather than guessed. |
| `artifact_unavailable` | The `corrupt` event identifies the missing/corrupt blob or restore path; the miss links to that event. |
| `remote_unavailable` | The accompanying fallback contains the request failure. Compilation continues locally. |

Static-key changes are compared against up to eight recent identities for the
same crate/source. The comparison closest to the current one supplies the
explanation; it is identified by key prefix. These snapshots are diagnostic
evidence only and never authorize cache hits. Compiler identity, flags, and
environment values are stored as hashes. Paths and variable/flag names remain
readable, while flag/environment values are not copied into the identity log.
On a fresh CI runner with no diagnostic history, Bellows reports that limitation.
Preserve the client state directory if cross-run identity comparisons are wanted.

A different Cargo target-directory path can legitimately produce
`environment_changed: OUT_DIR`, followed by `input_changed` for crates that
depend on the rebuilt metadata. The Manifold canary demonstrated this with
`serde_core` and `manifold_mod_types`. Clearing and reusing the same target path
avoids that particular difference; moving to a new path must retain exact
environment validation.

Concurrent builds can share a static key while requiring different candidates.
After the first owner finishes, a `lease_acquired` event means the waiting build
has taken the released lease to compile its own variant. Rejected candidates
are not logged again on every wait poll. A `wait_timeout` fallback identifies
an owner that did not supply a usable result or release its lease within the
configured wait budget.

Bypasses are distinct from misses. Reasons include `incremental`, `proc_macro`,
`native_inputs`, `linked_output`, `compiler_probe`, and `unsupported_outputs`.
They describe work outside the transparent cache's correctness boundary.
`symlink_input` identifies a compiler input whose resolution is not yet modeled;
if discovered in dep-info after compilation, capture is skipped safely.

## Structured output and retention

`explain --json` remains a JSON event array. Events add optional `reason`,
`session_id`, `workspace`, and `duration_ms` fields; older event records remain
readable. The existing `kind`, `detail`, `crate_name`, `static_key`, and
`action_key` fields are retained.

`explain --summary --json` returns counts grouped by decision and reason, all
affected crate names, and up to three example details per group. Text summaries
show the most common groups; `--limit` controls their number. `stats --json`
retains its `remote` and `events` fields and adds a `diagnostics` summary.
`elapsed_ms` is wrapped-command wall time for one selected build.
`compiler_process_ms` sums wrapper durations, including lookup/restore/compile
work, and may exceed wall time because compilers run concurrently. It is not an
estimate of time saved.

The event log rotates at 16 MiB and retains two previous files (`events.jsonl.1`
and `.2`). Readers include all three under the same cross-process lock used by
writers. Old sessions eventually age out; summaries cover retained events.
Malformed lines are counted and skipped with a warning. Diagnostic failures
never change the compiler's result.
