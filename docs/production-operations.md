# Muzen Production Operations

Muzen can run as an HTTP service through `muzen-service`. The current service
host is suitable for preview deployments where storage, authentication, and
network controls are provided explicitly by the operator.

## Storage

Set `DATABASE_URL` to use Postgres-backed review sessions and workspace
profiles:

```sh
DATABASE_URL=postgres://...
cargo run --bin muzen-service -- --bind 127.0.0.1:7341
```

If `DATABASE_URL` is absent, `muzen-service` starts with in-memory stores. That
mode is for local development only; review sessions, profiles, logs, events,
and artifacts disappear when the process exits.

The service runs its Postgres schema setup on startup. During the preview
period, Review Session storage uses a versioned fresh-reset migration strategy:
old preview schemas may be replaced when the expected schema version changes.
Do not point preview builds at a database whose data must be preserved without
first taking a backup.

## Authentication

Muzen does not currently enforce general HTTP API authentication inside the
Rust router. Production deployments must put `muzen-service` behind an
authenticating reverse proxy, API gateway, service mesh, or private network
boundary.

Webhook verification is enforced separately by provider-specific secrets:

```sh
GITHUB_WEBHOOK_SECRET=...
GITLAB_WEBHOOK_TOKEN=...
```

These webhook secrets validate GitHub/GitLab webhook requests. They do not
authenticate normal `/v1/reviews`, `/v1/workspaces`, result, event, artifact, or
profile API calls.

## Runtime Behavior

`muzen-service` binds to `127.0.0.1:7341` by default. Use `--bind` to change the
address:

```sh
cargo run --bin muzen-service -- --bind 0.0.0.0:7341
```

Review creation through the HTTP API queues durable sessions. Workers claim
queued sessions through the Review Session store, execute the review through
the Rust runner path, then persist events, logs, results, and artifacts.

Events and logs are persisted in append order. Artifact payloads are persisted
with separate redacted and raw visibility. Operators should treat raw artifacts
as sensitive data and back up or expire them according to their own retention
policy.

## Backup And Retention

Back up the Postgres database before deploying a new preview build. The
database contains review state, event history, logs, artifacts, retry state,
leases, workspace model profiles, and workspace provider profiles.

Muzen does not currently provide automatic retention or compaction for review
sessions, logs, events, or artifacts. Add external retention jobs before using
the service for high-volume workloads.

## Shutdown And Health

The current service host relies on the process supervisor for startup,
shutdown, restart, and health policy. Run it under a supervisor that can restart
failed processes and drain traffic before termination.

There is no dedicated health endpoint yet. Use process-level checks and, for
Postgres deployments, database connectivity checks at the platform layer.

## Known Hardening Gaps

- General HTTP API auth is external to Muzen.
- Postgres integration and provider smoke tests should be run in deployment CI.
- Live GitHub/GitLab materialization depends on configured provider credentials
  and network access.
- Preview schema migrations may reset older Review Session tables.
- Retention, metrics export, and health checks are operator-owned today.
