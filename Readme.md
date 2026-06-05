# Muzen

Muzen is a Rust-first review-session runtime with TypeScript and Python SDK
previews layered over the `muzen-runner` protocol.

The current preview supports local repository reviews end to end. GitHub and
GitLab source strings and typed builders parse into stable descriptors, but
provider materialization, durable scheduling, workspaces, BYOK profile storage,
webhooks, remote clients, and production workers are still tracked RFC work.

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

## RFC Progress

The implementation ledger lives at
`docs/rfcs/0001-implementation-progress.md`.

Current completed slices:

- Rust SDK-facing review-session contracts.
- Rust local review-session execution facade over `muzen-runner`.
- TypeScript SDK preview over stdio JSON-RPC.
- Python SDK preview over stdio JSON-RPC.

Major open slices:

- Durable session store, replay cursors, workers, leases, retries, and
  cancellation.
- Provider source materialization for GitHub and GitLab.
- Workspace-owned model/provider profiles and secret references.
- Webhook helpers and remote HTTP client mode.
- Artifact helper APIs and production examples.
