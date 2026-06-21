# Review Quality Benchmarks

These benchmarks run the production reviewer path and then score the emitted
findings against golden issues. They must not synthesize model tool turns,
hardcode exploration, or bypass `muzen-runner stdio`.

Build the production runner first:

```sh
cargo build --release --bin muzen-runner
```

Run a materialized pull-request worktree:

```sh
MODEL=gpt-4o-mini node bench/review-quality/run-production-review.mjs \
  --repo /tmp/cal-pr-11059-worktree \
  --base-ref aci-martian/pr-11059-base \
  --runner-path target/release/muzen-runner \
  --golden bench/review-quality/goldens/cal-pr-11059.json \
  --sessions 0 \
  --max-active 8 \
  --output bench/results-review-quality/cal-pr-11059.json
```

Or let the wrapper prepare the GitHub PR worktree and then invoke the same
production harness:

```sh
MODEL=gpt-4o-mini node bench/review-quality/run-github-pr-review.mjs \
  --repo-slug calcom/cal.com \
  --pr 11059 \
  --runner-path target/release/muzen-runner \
  --golden bench/review-quality/goldens/cal-pr-11059.json \
  --sessions 0 \
  --max-active 8 \
  --output bench/results-review-quality/cal-pr-11059.json
```

The harness builds a `run.start` request from git metadata, invokes
`muzen-runner stdio`, stores the JSON-RPC frames, parses the final runner result,
and reports hit rate, false positives, token/tool metrics, coverage, challenge,
and candidate synthesis diagnostics. In the current autonomous-review path,
`--sessions 0` creates the default adaptive `review-orchestrator`; `--sessions 1`
uses an explicit caller-provided orchestrator template, so `--max-turns` and
`--max-tool-calls` are hard caps for that orchestrator. The completion
diagnostics in each result report the effective `maxToolCalls` used by the
orchestrator.

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
