# RFC 0001 Remote HTTP API Contract

This document defines the preview HTTP contract that `createMuzenClient({
baseUrl })` uses. The contract mirrors the Rust-owned review-session nouns and
keeps runner ids, worker leases, attempts, and queue internals out of the
friendly SDK surface.

## Authentication

Clients may send `Authorization: Bearer <token>`. The host decides how tokens
map to users, workspaces, and allowed profile access.

## Rust Router Binding

Rust core exposes a framework-neutral `ReviewHttpRouter` that accepts a
`ReviewHttpRequest` and returns a `ReviewHttpResponse`. The router covers the
contract routes below without depending on Axum, Actix, Hyper, or another
framework. Concrete hosts are responsible for adapting framework request and
response types, authenticating callers, resolving webhook secrets, and choosing
the production session/profile stores.

The repository also ships an Axum adapter and `muzen-service` binary. The
service binds the core router to a real HTTP listener and resolves webhook
secrets from environment variables. It currently uses the in-memory Rust stores;
production deployments still need database-backed session and profile stores.

## Create Review

`POST /v1/reviews`

Request:

```json
{
  "source": {
    "type": "local",
    "repo": ".",
    "changedFiles": ["Cargo.toml"]
  },
  "options": {
    "dedupe": "source",
    "model": "default",
    "metadata": {
      "requestedBy": "api"
    }
  }
}
```

Response:

```json
{
  "review": {
    "id": "review-123",
    "status": "queued",
    "source": {
      "type": "github_pull_request",
      "owner": "maskdotdev",
      "repo": "heimdaal",
      "number": 123
    }
  }
}
```

The response MAY also be the review snapshot object directly. SDK clients must
accept both shapes.

## Get Review

`GET /v1/reviews/{reviewId}`

Returns a `ReviewSessionSnapshot`.

## Get Result

`GET /v1/reviews/{reviewId}/result`

Returns one of:

- `204 No Content` when no final result exists.
- `{ "result": ReviewResult }`.
- `ReviewResult` directly.

## Cancel Review

`POST /v1/reviews/{reviewId}/cancel`

Request:

```json
{
  "reason": "superseded"
}
```

Returns `204 No Content` or a review snapshot. Cancellation is durable and must
be honored by workers.

## Replay Events

`GET /v1/reviews/{reviewId}/events?after={cursor}`

Response:

```json
{
  "events": [
    {
      "cursor": "1",
      "type": "session.queued",
      "reviewId": "review-123",
      "timestampUtc": "1780620000.000000000Z",
      "payload": {}
    }
  ]
}
```

The response MAY also be the event array directly.

## Stream Events

`GET /v1/reviews/{reviewId}/events/stream?after={cursor}`

Returns `text/event-stream` frames using review event cursors as SSE ids:

```text
id: 1
event: session.queued
data: {"cursor":"1","type":"session.queued","reviewId":"review-123"}
```

## Artifacts

`GET /v1/reviews/{reviewId}/artifacts/{artifactId}?view=redacted`

Returns `{ "artifact": ReviewArtifact }` or `ReviewArtifact` directly.

`POST /v1/reviews/{reviewId}/artifacts/export`

Request:

```json
{
  "view": "redacted",
  "artifactIds": [],
  "maxArtifacts": 10,
  "maxBytes": 1048576
}
```

Returns `ReviewArtifactExport`.

## Workspace Reviews

`POST /v1/workspaces/{workspaceId}/reviews`

Request and response shapes match `POST /v1/reviews`, but the host schedules
the review in the named workspace and captures workspace-owned model/provider
profile snapshots.

## Webhooks

`POST /v1/webhooks/github`

`POST /v1/webhooks/gitlab`

Hosts receive the original webhook request body and provider headers. The host
MUST verify signatures/tokens in Rust core, map supported provider events to
review sources, schedule a durable review, and return a webhook delivery body:

```json
{
  "type": "review_created",
  "deliveryId": "delivery-1",
  "reviewId": "review-123",
  "status": "queued"
}
```

`review_deduped` responses use status `200 OK`; `review_created` and
`ignored` responses use status `202 Accepted`.

Hosts MAY also expose workspace-scoped webhook routes:

`POST /v1/workspaces/{workspaceId}/webhooks/github`

`POST /v1/workspaces/{workspaceId}/webhooks/gitlab`

The TypeScript remote SDK forwards `muzen.webhooks.github.response(request)`
and `muzen.webhooks.gitlab.response(request)` to these routes, preserving the
raw body and provider headers. Passing `{ workspaceId }` selects the
workspace-scoped route.

## Workspace Model Profiles

`PUT /v1/workspaces/{workspaceId}/models/{name}`

Request:

```json
{
  "provider": "openai_compatible",
  "model": "gpt-5",
  "secretRef": "vault://workspaces/acme/models/default",
  "baseUrl": "https://models.example.test",
  "routing": {
    "region": "us-east"
  }
}
```

Response:

```json
{
  "profile": {
    "workspaceId": "acme",
    "name": "default",
    "version": "1",
    "provider": "openai_compatible",
    "model": "gpt-5",
    "secretRef": "vault://workspaces/acme/models/default",
    "baseUrl": "https://models.example.test",
    "routing": {
      "region": "us-east"
    },
    "updatedAtUtc": "1780620000.000000000Z"
  }
}
```

The response MAY also be the profile object directly.

`GET /v1/workspaces/{workspaceId}/models/{name}`

Returns `{ "profile": ModelProfile }`, `ModelProfile` directly, or
`204 No Content` when absent.

`GET /v1/workspaces/{workspaceId}/models`

Returns `{ "profiles": ModelProfile[] }` or the array directly.

## Workspace Provider Profiles

`PUT /v1/workspaces/{workspaceId}/providers/{name}`

Request:

```json
{
  "provider": "github",
  "secretRef": "vault://workspaces/acme/providers/github",
  "baseUrl": "https://api.github.com",
  "routing": {
    "installation": "123"
  }
}
```

Response:

```json
{
  "profile": {
    "workspaceId": "acme",
    "name": "github",
    "version": "1",
    "provider": "github",
    "secretRef": "vault://workspaces/acme/providers/github",
    "baseUrl": "https://api.github.com",
    "routing": {
      "installation": "123"
    },
    "updatedAtUtc": "1780620000.000000000Z"
  }
}
```

The response MAY also be the profile object directly.

`GET /v1/workspaces/{workspaceId}/providers/{name}`

Returns `{ "profile": ProviderProfile }`, `ProviderProfile` directly, or
`204 No Content` when absent.

`GET /v1/workspaces/{workspaceId}/providers`

Returns `{ "profiles": ProviderProfile[] }` or the array directly.

## Webhooks

Framework-facing SDK webhook helpers should verify provider signatures/tokens,
map payloads to review sources, schedule reviews through `POST /v1/reviews`,
and return the Rust delivery body shape:

```json
{
  "type": "review_created",
  "deliveryId": "provider-delivery-id",
  "reviewId": "review-123",
  "status": "queued"
}
```

The Rust core already owns provider verification and source mapping helpers.
