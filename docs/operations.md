# Bellows operator runbook

This runbook covers a trusted, single-tenant production deployment of the
compiler cache. Bellows does not terminate TLS and is not a hostile
multi-tenant execution sandbox.

## Secure startup

Generate a high-entropy bearer token, store it in the CI secret manager, and
inject it into both server and clients as `BELLOWS_AUTH_TOKEN`. A listener on a
non-loopback address refuses to start without authentication. The escape hatch
`--allow-insecure-no-auth` is only for isolated disposable development
networks.

```bash
export BELLOWS_AUTH_TOKEN="$(openssl rand -hex 32)"
bellowsd \
  --listen 127.0.0.1:7878 \
  --data-dir /var/lib/bellows \
  --max-blob-mb 512 \
  --max-requests 128
```

Put a TLS reverse proxy or a private authenticated network in front of the
loopback service. Never place the plain HTTP listener directly on the public
internet. Only one `bellowsd` may own a data directory; a second process fails
at startup instead of risking split-brain leases.

The container listens on `0.0.0.0:7878`, so `BELLOWS_AUTH_TOKEN` is mandatory:

```bash
docker run --detach --name bellows \
  --restart unless-stopped \
  --env BELLOWS_AUTH_TOKEN \
  --publish 127.0.0.1:7878:7878 \
  --volume bellows-data:/var/lib/bellows \
  bellows:VERSION
```

`GET /live` is an unauthenticated liveness-only endpoint used by the container
health check. Every `/v1/*` endpoint requires the token when authentication is
configured. Readiness is verified with `bellows doctor` from the same network
and credentials as CI.

## Backups and upgrades

The data directory is a disposable cache, not a system of record. Backups are
optional. If retained, stop the daemon with SIGTERM and wait for a clean exit
before snapshotting the volume. Atomic record publication and directory fsyncs
make ordinary process interruption recoverable; malformed records are moved to
`quarantine/` and become safe cache misses.

Bellows currently has no stable wire or on-disk compatibility guarantee. The
client and server must use the same `PROTOCOL_VERSION`. For an upgrade:

1. Stop CI jobs that publish to the service.
2. Send SIGTERM and wait for `bellowsd` to exit.
3. Keep or snapshot the old data volume for rollback.
4. Deploy matching client and server binaries.
5. Start the daemon and run authenticated `bellows doctor`.
6. If the protocol changed, start with an empty data directory. Old records are
   rejected rather than trusted.
7. Run one cold canary and one fresh-runner warm canary before restoring normal
   concurrency.

Rolling mixed-version deployments are unsupported. A protocol mismatch is a
clean miss or an explicit doctor failure, never authorization to restore an old
artifact.

## Capacity and recovery

Use `bellows stats` for blob bytes, record counts, candidates, and leases. Use
`bellows gc --max-mb MEBIBYTES` to enforce a cache budget. Newly uploaded blobs
receive a one-hour publication grace so concurrent GC cannot remove them
between blob and record publication. A zero-byte target can therefore remain
temporarily above target while uploads are active.

Compiler-cache failures are fail-open: the wrapper records the failure and
runs official rustc. A CI job should still fail closed at setup by running
`bellows doctor`, so an unintended cache outage is visible before expensive
work starts. If a daemon is unhealthy:

1. Preserve logs and `quarantine/` for diagnosis.
2. Stop the daemon cleanly.
3. Restart against the same volume and run `bellows doctor`.
4. If corruption persists, move the volume aside and start with an empty one.
5. Run cold and warm canaries; do not copy individual records into the new
   store.

Remote execution remains disabled unless `--enable-execution` is explicitly
set. Keep it disabled for the Manifold compiler-cache deployment.
