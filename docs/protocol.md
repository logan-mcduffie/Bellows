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
