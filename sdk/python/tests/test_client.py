import json
import os
import tempfile
import unittest
from pathlib import Path

from muzen import (
    Client,
    ModelProfileInput,
    ProviderProfileInput,
    ReviewAgentSession,
    WebhookDelivery,
    ReviewOptions,
    create_webhook_response,
    create_muzen_client,
    local,
)
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
            artifacts = await review.export_artifacts()
            artifact = await review.read_artifact(artifacts.artifacts[0].artifact_id)
            replayed = []
            review.subscribe(lambda event: replayed.append(event.type))

            self.assertEqual(review.status, "completed")
            self.assertEqual(result.status, "completed")
            self.assertIn("Review completed", result.summary)
            self.assertGreater(artifacts.artifact_count, 0)
            self.assertEqual(artifact.artifact_id, artifacts.artifacts[0].artifact_id)
            self.assertGreater(len(artifact.content), 0)
            self.assertIn("session.completed", replayed)
            self.assertEqual((await review.refresh()).id, review.id)

    async def test_provider_sources_wait_for_materialization(self) -> None:
        with self.assertRaises(MuzenUnsupportedFeatureError):
            await self.client.review("github:maskdotdev/heimdaal#123")


class RemoteClientTests(unittest.IsolatedAsyncioTestCase):
    async def test_remote_workspace_profiles_and_review_contract(self) -> None:
        requests = []

        def transport(method, path, body, headers):
            requests.append(
                {
                    "method": method,
                    "path": path,
                    "body": body,
                    "authorization": headers.get("Authorization"),
                }
            )
            model_profile = {
                "workspaceId": "acme",
                "name": "default",
                "version": "1",
                "provider": "openai_compatible",
                "model": "gpt-5",
                "secretRef": "vault://workspaces/acme/models/default",
                "baseUrl": "https://models.example.test",
                "routing": {"region": "us-east"},
                "updatedAtUtc": "1780620000.000000000Z",
            }
            provider_profile = {
                "workspaceId": "acme",
                "name": "github",
                "version": "1",
                "provider": "github",
                "secretRef": "vault://workspaces/acme/providers/github",
                "baseUrl": "https://api.github.com",
                "routing": {"installation": "123"},
                "updatedAtUtc": "1780620000.000000000Z",
            }
            if path == "/v1/workspaces/acme/models/default" and method == "PUT":
                return {"profile": model_profile}
            if path == "/v1/workspaces/acme/models/default" and method == "GET":
                return model_profile
            if path == "/v1/workspaces/acme/models":
                return {"profiles": [model_profile]}
            if path == "/v1/workspaces/acme/providers/github" and method == "PUT":
                return {"profile": provider_profile}
            if path == "/v1/workspaces/acme/providers/github" and method == "GET":
                return provider_profile
            if path == "/v1/workspaces/acme/providers":
                return {"profiles": [provider_profile]}
            if path == "/v1/workspaces/acme/reviews" and method == "POST":
                return {
                    "review": {
                        "id": "review-workspace-1",
                        "status": "queued",
                        "source": body["source"],
                    }
                }
            if path == "/v1/reviews/review-workspace-1/result":
                return {
                    "result": {
                        "reviewId": "review-workspace-1",
                        "sessionId": "review-workspace-1",
                        "status": "completed",
                        "conclusion": "approved",
                        "summary": "Remote review completed.",
                        "findings": [],
                        "coverage": {
                            "filesConsidered": 1,
                            "filesReviewed": 1,
                            "filesSkipped": 0,
                        },
                    }
                }
            raise AssertionError(f"unexpected request {method} {path}")

        workspace = create_muzen_client(
            base_url="https://muzen.example",
            token="test-token",
            transport=transport,
        ).workspace("acme")

        model = await workspace.models.set(
            "default",
            ModelProfileInput(
                provider="openai_compatible",
                model="gpt-5",
                secret_ref="vault://workspaces/acme/models/default",
                base_url="https://models.example.test",
                routing={"region": "us-east"},
            ),
        )
        loaded_model = await workspace.models.get("default")
        models = await workspace.models.list()
        provider = await workspace.providers.set(
            "github",
            ProviderProfileInput(
                provider="github",
                secret_ref="vault://workspaces/acme/providers/github",
                base_url="https://api.github.com",
                routing={"installation": "123"},
            ),
        )
        loaded_provider = await workspace.providers.get("github")
        providers = await workspace.providers.list()
        review = await workspace.review("github:maskdotdev/heimdaal#123", ReviewOptions(model="default"))
        result = await review.wait(timeout="1s")

        self.assertEqual(workspace.id, "acme")
        self.assertEqual(model.model, "gpt-5")
        self.assertEqual(loaded_model.secret_ref, "vault://workspaces/acme/models/default")
        self.assertEqual(len(models), 1)
        self.assertEqual(provider.provider, "github")
        self.assertEqual(loaded_provider.secret_ref, "vault://workspaces/acme/providers/github")
        self.assertEqual(len(providers), 1)
        self.assertEqual(review.id, "review-workspace-1")
        self.assertEqual(result.conclusion, "approved")
        self.assertEqual(requests[0]["authorization"], "Bearer test-token")
        self.assertEqual(
            [request["path"] for request in requests],
            [
                "/v1/workspaces/acme/models/default",
                "/v1/workspaces/acme/models/default",
                "/v1/workspaces/acme/models",
                "/v1/workspaces/acme/providers/github",
                "/v1/workspaces/acme/providers/github",
                "/v1/workspaces/acme/providers",
                "/v1/workspaces/acme/reviews",
                "/v1/reviews/review-workspace-1/result",
            ],
        )


class WebhookResponseTests(unittest.TestCase):
    def test_create_webhook_response_returns_framework_neutral_http_response(self) -> None:
        response = create_webhook_response(
            WebhookDelivery(
                type="review_created",
                delivery_id="delivery-1",
                review_id="review-1",
                status="queued",
            ),
            headers={"X-Muzen-Test": "yes"},
        )
        deduped = create_webhook_response(
            {
                "type": "review_deduped",
                "deliveryId": "delivery-1",
                "reviewId": "review-1",
                "status": "queued",
            }
        )
        ignored = create_webhook_response(
            {
                "type": "ignored",
                "deliveryId": "delivery-2",
                "reason": "unsupported event",
            }
        )

        self.assertEqual(response.status_code, 202)
        self.assertEqual(response.headers["Content-Type"], "application/json")
        self.assertEqual(response.headers["X-Muzen-Test"], "yes")
        self.assertEqual(
            json.loads(response.body),
            {
                "type": "review_created",
                "deliveryId": "delivery-1",
                "reviewId": "review-1",
                "status": "queued",
            },
        )
        self.assertEqual(deduped.status_code, 200)
        self.assertEqual(ignored.status_code, 202)


if __name__ == "__main__":
    unittest.main()
