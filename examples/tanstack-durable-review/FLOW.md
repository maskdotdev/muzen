# Durable Flow

This is the flow to read when mapping the example to Argus or another host.

## One Large Review

```text
1. User submits review request from the TanStack UI.
2. Host service creates durable review row:
   - status = queued
   - source = local/GitLab/GitHub/etc.
   - change = provider-neutral ChangeSpec
   - sessions = review personas inside the same Muzen run
3. Host worker claims that one review row.
4. Worker starts one Muzen review.
5. Muzen materializes one source snapshot.
   - local targets reuse the service-machine worktree
   - GitHub PR targets fetch `refs/pull/<number>/head` into a temp checkout
6. Muzen runs multiple sessions against the shared snapshot.
7. Sessions call built-in tools such as read_diff, read_file, search_text.
8. Muzen emits ordered events.
9. Host service persists those events with host cursors.
10. Browser consumes SSE and can reconnect using the last cursor.
11. Muzen returns one aggregated result.
12. Host service persists the result and emits review.result_created.
```

## Durable Levels

```text
Durable job:
  one merge request = one durable review

Muzen run:
  one durable review = one Muzen run

Sessions:
  one Muzen run = many sessions, such as correctness/security/performance

Tool batches:
  one session turn can run multiple independent tools concurrently
```

## Event Readback

The browser does not hold the job open. It only holds a subscription:

```text
GET /api/reviews/:id/events/stream?after=cursor
```

If the browser disconnects, reconnect with the last persisted cursor:

```text
GET /api/reviews/:id/events/stream?after=17
```

The service replays events after that cursor before streaming new ones.

## Real GitHub PRs

The example accepts three GitHub target shapes:

```text
https://github.com/owner/repo/pull/123
github:owner/repo#123
owner/repo#123
```

The host converts those into a provider-neutral Muzen source:

```ts
github.pullRequest({ owner, repo, number })
```

If the changed-file field is empty, the runner fetches the PR ref and infers the
changed files from git diff. For private repositories, run the service with
`GITHUB_TOKEN` so the runner can authenticate the fetch.

## Production Swap

The example's in-memory store is only a readable local stand-in. A production
host should swap it for:

- transactional review creation
- durable queue/claiming
- leases and heartbeats
- retry policy
- persisted ordered event log
- final result and artifact storage
- SSE replay from the event log
