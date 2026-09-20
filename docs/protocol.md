# Protocol compatibility policy

`PROTOCOL_VERSION` covers manifest meaning, key construction, normalization,
and restore safety. Bellows increments it whenever either side could interpret
the same bytes differently.

- Client and server releases are deployed as one unit.
- Mixed protocol versions are unsupported.
- Health/doctor reports a protocol mismatch explicitly.
- Compiler candidates, declared actions, and archives with another protocol
  are rejected before any artifact is restored.
- An on-disk protocol change requires a fresh cache directory. Cache contents
  are reproducible and are never migrated by weakening validation.
- Unknown or malformed records are quarantined or treated as misses; they do
  not become hits.

Backward compatibility can be added later through explicit decoders and
migration tests. It must never be inferred from similar JSON shapes.

## Version 0.2.1 / protocol 5

Protocol 4 invalidated compiler records captured with path-normalized macro
environment values or unresolved symlink dependencies. Declared-action keys now
include the protocol, and their new execution semantics preserve Cargo rustflags.
Protocol 5 also stops normalizing literal compiler flag values and narrows the
eligible toolchain/native-input boundary. Do not reuse older server data
directories: deploy matching client/server
binaries with a fresh directory and update CI's immutable Bellows pin together.

Local mode automatically uses `store-v5`, and runner-local L1 uses `l1-v5`,
beneath the configured state directory. Old `store`/`l1` and version-4 directories remain
untouched and are not read by the new cache. They may be removed after rollback
is no longer needed; new-version local GC only operates on `store-v5`.
The first eligible build in the new namespace is intentionally cold.
