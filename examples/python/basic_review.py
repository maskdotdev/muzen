import asyncio
import os
import sys

from muzen import Client, ReviewChangeSpec, ReviewChangedFile, ReviewOptions, local


async def main() -> None:
    repo = sys.argv[1] if len(sys.argv) > 1 else "."
    changed_files = sys.argv[2:]
    if not changed_files:
        raise SystemExit("usage: basic_review.py <repo> <changed-file> [changed-file ...]")
    client = await Client.create(
        runner_path=os.environ.get("MUZEN_RUNNER_PATH"),
    )
    try:
        review = await client.review(
            local(repo),
            ReviewOptions(
                change=ReviewChangeSpec(
                    kind="revision_range",
                    changed_files=[
                        ReviewChangedFile(path=path, status="modified")
                        for path in changed_files
                    ],
                )
            ),
        )

        async for event in review.events():
            print(event.type)

        result = await review.wait()
        print(result.conclusion)
        print(result.summary)
    finally:
        await client.close()


if __name__ == "__main__":
    asyncio.run(main())
