# RFC 0001 Remote HTTP API Contract

This document defines the preview HTTP contract that `createMuzenClient({
baseUrl })` uses. The contract mirrors the Rust-owned review-session nouns and
keeps runner ids, worker leases, attempts, and queue internals out of the
friendly SDK surface.

## Authentication

Clients may send `Authorization: Bearer <token>`. The host decides how tokens
map to users, workspaces, and allowed profile access.

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
