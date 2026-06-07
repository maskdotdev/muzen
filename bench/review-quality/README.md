# Review Quality Benchmarks

These benchmarks run the production reviewer path and then score the emitted
findings against golden issues. They must not synthesize model tool turns,
hardcode exploration, or bypass `muzen run`.

Build the production runner first:

```sh
cargo build --release --bin muzen
```

Run a materialized pull-request worktree:

```sh
MODEL=gpt-4o-mini node bench/review-quality/run-production-review.mjs \
  --repo /tmp/cal-pr-11059-worktree \
  --base-ref aci-martian/pr-11059-base \
  --runner-path target/release/muzen \
  --golden bench/review-quality/goldens/cal-pr-11059.json \
  --sessions 11 \
  --max-active 4 \
  --output bench/results-review-quality/cal-pr-11059.json
```

Or let the wrapper prepare the GitHub PR worktree and then invoke the same
production harness:

```sh
MODEL=gpt-4o-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com \
  --pr 11059 \
  --runner-path target/release/muzen \
  --golden bench/review-quality/goldens/cal-pr-11059.json \
  --sessions 11 \
  --max-active 4 \
  --output bench/results-review-quality/cal-pr-11059.json
```

The harness builds a `ReviewRunJobV1` from git metadata, invokes
`muzen run --job`, stores the JSONL event log, parses the final production review
result, and reports hit rate, false positives, token/tool metrics, and candidate
synthesis diagnostics.

Summarize one or more result files:

```sh
node bench/review-quality/summarize-results.mjs \
  bench/results-review-quality/cal-pr-11059.json \
  bench/results-review-quality/cal-pr-8330.json
```

Golden files are data only:

```json
{
  "issues": [
    {
      "id": "unique-id",
      "title": "Human-readable issue name",
      "path": "src/file.ts",
      "startLine": 10,
      "endLine": 20,
      "keywords": ["required", "terms"]
    }
  ]
}
```

Keywords should describe the bug class, not one benchmark's exact wording.
