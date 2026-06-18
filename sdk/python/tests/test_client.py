import json
import os
import tempfile
import unittest
from pathlib import Path

from muzen import (
    Client,
    ContextEngineConfig,
    ContextQueryLimits,
    ModelProfileInput,
    ProviderProfileInput,
    ReviewChangeSpec,
    ReviewChangedFile,
    ReviewInstruction,
    WebhookDelivery,
    ReviewOptions,
    ReviewTool,
    SwarmAgent,
    SwarmOptions,
    anthropic,
    create_webhook_response,
    create_muzen_client,
    local,
    openai,
    parse_review_source,
)
from muzen.client import _to_runner_start_params, _to_swarm_start_params
from muzen.runner_mapping import _map_runner_result, _map_swarm_result


class RunnerMappingTests(unittest.TestCase):
    def test_local_workspace_context_uses_runner_rpcs(self) -> None:
        class FakeRunner:
            def __init__(self) -> None:
                self.calls = []

            async def request(self, method, params):
                self.calls.append({"method": method, "params": params})
                if method == "context.index":
                    return {
                        "schemaVersion": "muzen.context_manifest.v1",
                        "engineVersion": "0.1.0",
                        "snapshotId": "snap-1",
                        "ruleCount": 1,
                        "evidenceCount": 2,
                        "relationshipCount": 0,
                        "skippedCount": 0,
                        "createdAtUtc": "1780620000.000000000Z",
                    }
                if method == "context.pack":
                    return {
                        "id": "ctxpack-1",
                        "snapshotId": "snap-1",
                        "purpose": params["purpose"],
                        "evidence": [],
                        "relationships": [],
                        "omittedCandidates": [],
                        "budget": {"maxTokens": params["maxTokens"], "usedTokens": 0},
                        "sufficiency": {
                            "status": "probably_sufficient",
                            "missing": [],
                        },
                        "compilerVersion": "0.1.0",
                        "createdAtUtc": "1780620000.000000000Z",
                    }
                if method == "context.query":
                    return {
                        "kind": params["kind"],
                        "evidence": [],
                        "omitted": 0,
                    }
                if method == "context.feedback":
                    return {
                        "accepted": True,
                        "message": "stored",
                        "proposedLearning": {
                            "id": "learning-1",
                            "snapshotId": "snap-1",
                            "source": "human_feedback",
                            "status": "proposed",
                            "scope": "repository",
                            "evidenceIds": [],
                            "summary": params["feedback"],
                            "createdAtUtc": "1780620000",
                        },
                    }
                if method == "context.learning.approve":
                    return {
                        "accepted": True,
                        "learning": {
                            "id": params["learningId"],
                            "snapshotId": params["snapshotId"],
                            "source": "human_feedback",
                            "status": "approved",
                            "scope": "repository",
                            "evidenceIds": [],
                            "summary": "Suppress duplicate warning.",
                            "createdAtUtc": "1780620000",
                        },
                    }
                raise AssertionError(f"unexpected method {method}")

        async def run() -> None:
            runner = FakeRunner()
            workspace = Client(runner).workspace("local")

            manifest = await workspace.context.index(
                source=local("/repo"),
                changed_files=["src/auth.py"],
            )
            pack = await workspace.context.build_pack(
                source=local("/repo"),
                changed_files=["src/auth.py"],
                purpose="security",
                max_tokens=4000,
            )
            query = await workspace.context.query(
                source=local("/repo"),
                changed_files=["src/auth.py"],
                kind="related_tests",
                arguments={"path": "src/auth.py"},
            )
            feedback = await workspace.context.record_feedback(
                source=local("/repo"),
                changed_files=["src/auth.py"],
                feedback="Suppress duplicate warning.",
            )
            approval = await workspace.context.approve_learning(
                snapshot_id="snap-1",
                learning_id="learning-1",
                approve=True,
            )

            self.assertEqual(manifest["snapshotId"], "snap-1")
            self.assertEqual(pack["purpose"], "security")
            self.assertEqual(query["kind"], "related_tests")
            self.assertEqual(feedback["proposedLearning"]["status"], "proposed")
            self.assertEqual(approval["learning"]["status"], "approved")
            self.assertEqual(
                [call["method"] for call in runner.calls],
                [
                    "context.index",
                    "context.index",
                    "context.pack",
                    "context.index",
                    "context.query",
                    "context.index",
                    "context.feedback",
                    "context.learning.approve",
                ],
            )

        import asyncio

        asyncio.run(run())

    def test_provider_sources_are_forwarded_to_rust_runner(self) -> None:
        source = parse_review_source("github:maskdotdev/heimdaal#123")

        params = _to_runner_start_params("review-1", source, ReviewOptions())

        self.assertNotIn("repo", params)
        self.assertEqual(
            params["source"],
            {
                "type": "github_pull_request",
                "owner": "maskdotdev",
                "repo": "heimdaal",
                "number": 123,
            },
        )
        self.assertEqual(params["changedFiles"], [])

    def test_provider_neutral_options_are_forwarded_to_rust_runner(self) -> None:
        source = local("/repo")

        params = _to_runner_start_params(
            "review-1",
            source,
            ReviewOptions(
                metadata={"hostRunId": "flow-1"},
                change=ReviewChangeSpec(
                    kind="revision_range",
                    base_revision="base",
                    head_revision="head",
                    changed_files=[
                        ReviewChangedFile(path="src/auth.py", status="modified")
                    ],
                ),
                instructions=[
                    ReviewInstruction(
                        kind="host_policy",
                        text="Prefer concrete regressions.",
                        trusted=True,
                    )
                ],
            ),
        )

        self.assertEqual(params["changedFiles"], [])
        self.assertEqual(params["metadata"], {"hostRunId": "flow-1"})
        self.assertNotIn("contextEngine", params)
        self.assertEqual(params["change"]["headRevision"], "head")
        self.assertEqual(
            params["change"]["changedFiles"],
            [{"path": "src/auth.py", "status": "modified"}],
        )
        self.assertEqual(params["instructions"][0]["kind"], "host_policy")
        self.assertEqual(params["tools"], [])
        self.assertEqual(params["sessions"], [])

    def test_openai_models_are_mapped_to_runner_profiles(self) -> None:
        params = _to_runner_start_params(
            "review-1",
            local("/repo"),
            ReviewOptions(
                change=ReviewChangeSpec(
                    kind="revision_range",
                    changed_files=[
                        ReviewChangedFile(path="src/auth.py", status="modified")
                    ],
                ),
                model=openai(
                    "gpt-5.4-mini",
                    credential={"env": "OPENAI_API_KEY"},
                    max_output_tokens=4096,
                ),
            ),
        )

        self.assertEqual(params["model"]["defaultModelProfileId"], "default")
        self.assertEqual(len(params["model"]["modelProfiles"]), 1)
        self.assertEqual(params["model"]["modelProfiles"][0]["provider"], "openai_compatible")
        self.assertEqual(params["model"]["modelProfiles"][0]["model"], "gpt-5.4-mini")
        self.assertEqual(params["sessions"], [])

    def test_anthropic_models_are_mapped_to_messages_profiles(self) -> None:
        params = _to_runner_start_params(
            "review-1",
            local("/repo"),
            ReviewOptions(
                change=ReviewChangeSpec(
                    kind="revision_range",
                    changed_files=[
                        ReviewChangedFile(path="src/auth.py", status="modified")
                    ],
                ),
                model=anthropic("claude-opus-4-8"),
            ),
        )

        profiles = params["model"]["modelProfiles"]
        self.assertEqual(params["model"]["defaultModelProfileId"], "default")
        self.assertEqual(profiles[0]["provider"], "anthropic")
        self.assertEqual(profiles[0]["apiProtocol"], "messages")
        self.assertEqual(profiles[0]["credential"], {"env": "ANTHROPIC_API_KEY"})
        self.assertEqual(len(profiles), 1)
        self.assertEqual(params["sessions"], [])

    def test_runner_result_preserves_finding_provenance(self) -> None:
        result = _map_runner_result(
            "review-1",
            local("/repo"),
            {
                "runId": "review-1",
                "status": "completed",
                "summary": {
                    "sessions": 1,
                    "completedSessions": 1,
                    "modelCalls": 1,
                    "toolCalls": 2,
                    "totalTokens": 12,
                },
                "findings": [
                    {
                        "id": "finding-1",
                        "title": "Unsafe unwrap",
                        "claim": "The code can panic.",
                        "publishable": True,
                        "severity": "high",
                        "confidence": 0.81,
                        "validationStatus": "validated",
                        "evidence": [
                            {
                                "evidenceId": "ev-1",
                                "artifactId": "art-1",
                                "kind": "file_slice",
                                "contentHash": "hash-1",
                                "producingToolCallId": "call-1",
                            }
                        ],
                        "discoveredBy": ["security"],
                        "validatedBy": ["call-1"],
                    }
                ],
                "snapshots": [{"files": 2, "capturedFiles": 2}],
            },
        )

        finding = result.findings[0]
        self.assertEqual(finding.severity, "error")
        self.assertEqual(finding.confidence, 0.81)
        self.assertEqual(finding.validation_status, "validated")
        self.assertEqual(finding.evidence[0].evidence_id, "ev-1")
        self.assertEqual(finding.discovered_by, ["security"])
        self.assertEqual(finding.validated_by, ["call-1"])

    def test_swarm_options_are_forwarded_as_direct_sessions(self) -> None:
        params = _to_swarm_start_params(
            "swarm-1",
            SwarmOptions(
                repo="/repo",
                files=["src/auth.py"],
                model=openai("gpt-5.4-mini"),
                metadata={"hostRunId": "swarm-host-1"},
                tools=[
                    ReviewTool(
                        id="host.lookup",
                        description="Look up host context.",
                        parameters={"type": "object", "properties": {}},
                        effects=["read_host"],
                    )
                ],
                agents=[
                    SwarmAgent(
                        id="planner",
                        objective="Plan the migration.",
                        instructions=[
                            ReviewInstruction(
                                kind="session_objective",
                                text="Prefer small steps.",
                                trusted=True,
                            )
                        ],
                        tool_grants=["read_file"],
                    ),
                    SwarmAgent(
                        id="implementer",
                        objective="Implement the migration.",
                        model=openai("gpt-5.4"),
                    ),
                ],
            ),
        )

        self.assertNotIn("mode", params)
        self.assertEqual(params["repo"], "/repo")
        self.assertEqual(params["changedFiles"], ["src/auth.py"])
        self.assertEqual(params["metadata"], {"hostRunId": "swarm-host-1"})
        self.assertEqual(params["sessions"][0]["role"], "generalist")
        self.assertEqual(params["sessions"][0]["objective"], "Plan the migration.")
        self.assertEqual(params["sessions"][0]["toolGrants"], ["read_file"])
        self.assertEqual(params["sessions"][1]["modelProfileId"], "session:implementer")
        self.assertEqual(params["tools"][0]["id"], "host.lookup")
        self.assertEqual(params["tools"][0]["effects"], ["read_host"])
        self.assertEqual(params["model"]["defaultModelProfileId"], "default")
        self.assertEqual(len(params["model"]["modelProfiles"]), 2)

    def test_swarm_requires_at_least_one_agent(self) -> None:
        with self.assertRaisesRegex(ValueError, "requires at least one agent"):
            _to_swarm_start_params(
                "swarm-1",
                SwarmOptions(repo="/repo", agents=[]),
            )

    def test_maps_swarm_results(self) -> None:
        result = _map_swarm_result(
            "swarm-1",
            {
                "runId": "runner-1",
                "status": "partial",
                "summary": {
                    "sessions": 2,
                    "completedSessions": 1,
                    "modelCalls": 3,
                    "toolCalls": 4,
                    "inputTokens": 10,
                    "outputTokens": 8,
                    "totalTokens": 18,
                },
                "sessionOutputs": [
                    {
                        "sessionId": "planner",
                        "status": "done",
                        "completed": True,
                        "output": "Plan complete.",
                    },
                    {
                        "sessionId": "implementer",
                        "status": "failed",
                        "completed": False,
                        "output": None,
                    },
                ],
                "metadata": {"hostRunId": "swarm-host-1"},
            },
        )

        self.assertEqual(result.run_id, "swarm-1")
        self.assertEqual(result.status, "partial")
        self.assertEqual(result.usage.agents, 2)
        self.assertEqual(result.usage.completed_agents, 1)
        self.assertEqual(result.usage.input_tokens, 10)
        self.assertEqual(result.outputs[0].agent_id, "planner")
        self.assertEqual(result.outputs[0].output, "Plan complete.")
        self.assertEqual(result.outputs[1].status, "failed")
        self.assertIsNone(result.outputs[1].output)
        self.assertEqual(result.metadata, {"hostRunId": "swarm-host-1"})


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
                local(repo),
                ReviewOptions(
                    change=ReviewChangeSpec(
                        kind="revision_range",
                        changed_files=[
                            ReviewChangedFile(path="Cargo.toml", status="modified")
                        ],
                    ),
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
            if path == "/v1/workspaces/acme/context/index" and method == "POST":
                return {
                    "manifest": {
                        "schemaVersion": "muzen.context_manifest.v1",
                        "engineVersion": "0.1.0",
                        "snapshotId": "snap-1",
                        "ruleCount": 1,
                        "evidenceCount": 3,
                        "relationshipCount": 0,
                        "skippedCount": 0,
                        "createdAtUtc": "1780620000.000000000Z",
                    }
                }
            if path == "/v1/workspaces/acme/context/packs" and method == "POST":
                return {
                    "pack": {
                        "id": "ctxpack-1",
                        "snapshotId": "snap-1",
                        "purpose": body["purpose"],
                        "evidence": [],
                        "relationships": [],
                        "omittedCandidates": [],
                        "budget": {
                            "maxTokens": body["maxTokens"],
                            "usedTokens": 0,
                        },
                        "sufficiency": {
                            "status": "probably_sufficient",
                            "missing": [],
                        },
                        "compilerVersion": "0.1.0",
                        "createdAtUtc": "1780620000.000000000Z",
                    }
                }
            if path == "/v1/workspaces/acme/context/query" and method == "POST":
                return {
                    "result": {
                        "kind": body["kind"],
                        "evidence": [],
                        "omitted": 0,
                    }
                }
            if path == "/v1/workspaces/acme/context/feedback" and method == "POST":
                return {
                    "receipt": {
                        "accepted": True,
                        "message": "stored",
                        "proposedLearning": {
                            "id": "learning-1",
                            "snapshotId": "snap-1",
                            "source": "human_feedback",
                            "status": "proposed",
                            "scope": "repository",
                            "evidenceIds": [],
                            "summary": body["feedback"],
                            "createdAtUtc": "1780620000",
                        },
                    }
                }
            if (
                path == "/v1/workspaces/acme/context/learnings/approve"
                and method == "POST"
            ):
                return {
                    "receipt": {
                        "accepted": True,
                        "learning": {
                            "id": body["learningId"],
                            "snapshotId": body["snapshotId"],
                            "source": "human_feedback",
                            "status": "approved",
                            "scope": "repository",
                            "evidenceIds": [],
                            "summary": "Suppress duplicate warning.",
                            "createdAtUtc": "1780620000",
                        },
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
        review = await workspace.review(
            "github:maskdotdev/heimdaal#123",
            ReviewOptions(
                model=openai(
                    "gpt-5",
                    credential={"secretRef": "vault://workspaces/acme/models/default"},
                    base_url="https://models.example.test",
                )
            ),
        )
        manifest = await workspace.context.index(
            source=local("/repo"),
            changed_files=["src/auth.py"],
        )
        pack = await workspace.context.build_pack(
            source=local("/repo"),
            changed_files=["src/auth.py"],
            purpose="security",
            max_tokens=4000,
        )
        query = await workspace.context.query(
            source=local("/repo"),
            changed_files=["src/auth.py"],
            kind="related_tests",
            arguments={"path": "src/auth.py"},
            limits=ContextQueryLimits(max_results=10, max_tokens=1000),
        )
        feedback = await workspace.context.record_feedback(
            source=local("/repo"),
            changed_files=["src/auth.py"],
            feedback="Suppress duplicate warning.",
        )
        approval = await workspace.context.approve_learning(
            snapshot_id="snap-1",
            learning_id="learning-1",
            approve=True,
        )
        result = await review.wait(timeout="1s")

        self.assertEqual(workspace.id, "acme")
        self.assertEqual(model.model, "gpt-5")
        self.assertEqual(loaded_model.secret_ref, "vault://workspaces/acme/models/default")
        self.assertEqual(len(models), 1)
        self.assertEqual(provider.provider, "github")
        self.assertEqual(loaded_provider.secret_ref, "vault://workspaces/acme/providers/github")
        self.assertEqual(len(providers), 1)
        self.assertEqual(review.id, "review-workspace-1")
        self.assertEqual(manifest["schemaVersion"], "muzen.context_manifest.v1")
        self.assertEqual(pack["purpose"], "security")
        self.assertEqual(query["kind"], "related_tests")
        self.assertEqual(feedback["proposedLearning"]["status"], "proposed")
        self.assertEqual(approval["learning"]["status"], "approved")
        self.assertEqual(result.conclusion, "approved")
        self.assertEqual(requests[0]["authorization"], "Bearer test-token")
        feedback_request = requests[10]["body"]
        self.assertEqual(feedback_request["source"]["type"], "local")
        self.assertEqual(feedback_request["source"]["repo"], "/repo")
        self.assertNotIn("learningSource", feedback_request)
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
                "/v1/workspaces/acme/context/index",
                "/v1/workspaces/acme/context/packs",
                "/v1/workspaces/acme/context/query",
                "/v1/workspaces/acme/context/feedback",
                "/v1/workspaces/acme/context/learnings/approve",
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
