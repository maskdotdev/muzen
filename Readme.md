# Muzen

Muzen is a Rust-first review-session runtime with TypeScript and Python SDK
previews layered over the `muzen-runner` protocol.

The current preview supports local repository reviews end to end. Rust core
also owns durable review records, worker claims/leases/retries, workspace
profile records, effective config snapshots, and GitHub/GitLab webhook
verification helpers. GitHub/GitLab provider materialization, production
database persistence, remote clients, and framework-facing webhook/SSE helpers
are still tracked RFC work.

## Build The Runner

```sh
cargo build --bin muzen-runner
export MUZEN_RUNNER_PATH="$PWD/target/debug/muzen-runner"
```

## Rust Core

The Rust crate owns the core review-session contracts and local execution
facade:

```rust
use muzen::review_session::{Muzen, ReviewSource};

fn main() -> Result<(), muzen::review_session::ReviewSessionError> {
    let muzen = Muzen::new();
    let review = muzen.review(ReviewSource::local_with_changed_files(
        ".",
        ["Cargo.toml"],
    ))?;

    let result = review.wait()?;
    println!("{}", result.summary);
    Ok(())
}
```

## TypeScript Preview

```sh
cd sdk/typescript/packages/muzen-sdk
npm install
npm test
```

```ts
import { createMuzen, local } from "@muzen/sdk";

const muzen = await createMuzen({
  runnerPath: process.env.MUZEN_RUNNER_PATH,
});

const review = await muzen.review(
  local(".", {
    changedFiles: ["Cargo.toml"],
  }),
);

review.subscribe((event) => {
  console.log(event.type);
});

const result = await review.wait();
console.log(result.conclusion);
console.log(result.summary);

const artifacts = await review.exportArtifacts();
console.log(artifacts.artifactCount);

await muzen.close();
```

Runnable examples:

- `examples/typescript/basic-review`
- `examples/typescript/events`

Remote client preview:

```ts
import { createMuzenClient } from "@muzen/sdk";

const muzen = createMuzenClient({
  baseUrl: "https://muzen.example",
  token: process.env.MUZEN_TOKEN,
});

const workspace = muzen.workspace("acme");

await workspace.models.set("default", {
  provider: "openai_compatible",
  model: "gpt-5",
  secretRef: "vault://workspaces/acme/models/default",
});

await workspace.providers.set("github", {
  provider: "github",
  secretRef: "vault://workspaces/acme/providers/github",
});

const review = await workspace.review("github:maskdotdev/heimdaal#123", {
  model: "default",
});
```

## Python Preview

```sh
cd sdk/python
PYTHONPATH="$PWD" python3 -m unittest discover -s tests
```

```py
import asyncio
import os

from muzen import Client, local


async def main() -> None:
    client = await Client.create(
        runner_path=os.environ.get("MUZEN_RUNNER_PATH"),
    )
    try:
        review = await client.review(
            local(".", changed_files=["Cargo.toml"]),
        )

        for event in [event async for event in review.events()]:
            print(event.type)

        result = await review.wait()
        print(result.conclusion)
        print(result.summary)

        artifacts = await review.export_artifacts()
        print(artifacts.artifact_count)
    finally:
        await client.close()


asyncio.run(main())
```

Runnable examples:

- `examples/python/basic_review.py`
- `examples/python/notebook-review/notebook_review.ipynb`

## RFC Progress

The implementation ledger lives at
`docs/rfcs/0001-implementation-progress.md`.

Current completed slices:

- Rust SDK-facing review-session contracts.
- Rust local review-session execution facade over `muzen-runner`.
- Rust durable session records, worker scheduling semantics, and queued worker
  execution loop.
- Rust workspace model/provider profiles, host scheduling configuration, and
  secret-reference-only config snapshots.
- Rust GitHub/GitLab webhook verification and source mapping helpers.
- TypeScript SDK preview over stdio JSON-RPC.
- TypeScript remote client preview with workspace profile APIs.
- Python SDK preview over stdio JSON-RPC.

Major open slices:

- Production database persistence for durable sessions/profile records.
- Provider source materialization for GitHub and GitLab.
- Framework-facing webhook response helpers, remote HTTP client mode, and SSE
  streaming.
