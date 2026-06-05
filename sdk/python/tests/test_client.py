import os
import tempfile
import unittest
from pathlib import Path

from muzen import Client, ReviewAgentSession, ReviewOptions, local
from muzen.client import MuzenUnsupportedFeatureError


@unittest.skipUnless(os.environ.get("MUZEN_RUNNER_PATH"), "MUZEN_RUNNER_PATH is not set")
class ClientTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.client = await Client.create(runner_path=os.environ["MUZEN_RUNNER_PATH"])

    async def asyncTearDown(self) -> None:
        await self.client.close()

    async def test_runs_local_review_replays_events_and_waits_for_result(self) -> None:
        with tempfile.TemporaryDirectory() as repo:
            Path(repo, "Cargo.toml").write_text(
                '[package]\nname = "fixture"\nversion = "0.0.0"\n',
                encoding="utf-8",
            )

            review = await self.client.review(
                local(repo, changed_files=["Cargo.toml"]),
                ReviewOptions(
                    sessions=[
                        ReviewAgentSession(
                            id="security",
                            role="security",
                            objective="Find security regressions",
                        )
                    ]
                ),
            )
            result = await review.wait()
            replayed = []
            review.subscribe(lambda event: replayed.append(event.type))

            self.assertEqual(review.status, "completed")
            self.assertEqual(result.status, "completed")
            self.assertIn("Review completed", result.summary)
            self.assertIn("session.completed", replayed)
            self.assertEqual((await review.refresh()).id, review.id)

    async def test_provider_sources_wait_for_materialization(self) -> None:
        with self.assertRaises(MuzenUnsupportedFeatureError):
            await self.client.review("github:maskdotdev/heimdaal#123")


if __name__ == "__main__":
    unittest.main()
