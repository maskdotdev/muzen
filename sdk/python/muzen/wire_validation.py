from __future__ import annotations

from typing import Any, Dict, List, Optional

from .runner_mapping import _map_runner_artifact, _map_runner_status
from .types import (
    ModelProfile,
    ProviderProfile,
    ReviewArtifact,
    ReviewArtifactExport,
    ReviewCoverage,
    ReviewEvent,
    ReviewFinding,
    ReviewFindingEvidence,
    ReviewResult,
    ReviewSessionSnapshot,
    ReviewSource,
)


def _unwrap_review_snapshot(value: Any) -> ReviewSessionSnapshot:
    snapshot = value.get("review") if isinstance(value, dict) and isinstance(value.get("review"), dict) else value
    if not isinstance(snapshot, dict):
        raise RuntimeError("Muzen remote returned an invalid review session snapshot")
    return ReviewSessionSnapshot(
        id=snapshot["id"],
        status=_map_runner_status(snapshot["status"]),
        source=_remote_source(snapshot["source"]),
        result=_unwrap_optional_review_result(snapshot.get("result")),
    )


def _unwrap_optional_review_result(value: Any) -> Optional[ReviewResult]:
    if value is None:
        return None
    result = value.get("result") if isinstance(value, dict) and "result" in value else value
    if result is None:
        return None
    if not isinstance(result, dict):
        raise RuntimeError("Muzen remote returned an invalid review result")
    return _remote_result(result)


def _unwrap_review_events(value: Any) -> List[ReviewEvent]:
    events = value.get("events") if isinstance(value, dict) and isinstance(value.get("events"), list) else value
    if not isinstance(events, list):
        raise RuntimeError("Muzen remote returned invalid review events")
    return [
        ReviewEvent(
            cursor=str(event["cursor"]),
            type=event["type"],
            review_id=event["reviewId"],
            timestamp_utc=event["timestampUtc"],
            payload=event.get("payload"),
        )
        for event in events
    ]


def _unwrap_review_artifact(value: Any) -> ReviewArtifact:
    artifact = value.get("artifact") if isinstance(value, dict) and isinstance(value.get("artifact"), dict) else value
    if not isinstance(artifact, dict):
        raise RuntimeError("Muzen remote returned an invalid review artifact")
    return _map_runner_artifact(artifact)


def _unwrap_artifact_export(value: Any) -> ReviewArtifactExport:
    if not isinstance(value, dict):
        raise RuntimeError("Muzen remote returned an invalid artifact export")
    return ReviewArtifactExport(
        view=value.get("view", "redacted"),
        artifact_count=value.get("artifactCount", 0),
        total_bytes=value.get("totalBytes", 0),
        artifacts=[
            _map_runner_artifact(artifact)
            for artifact in value.get("artifacts", [])
            if isinstance(artifact, dict)
        ],
    )


def _unwrap_model_profile(value: Any) -> ModelProfile:
    profile = value.get("profile") if isinstance(value, dict) and isinstance(value.get("profile"), dict) else value
    if not isinstance(profile, dict):
        raise RuntimeError("Muzen remote returned an invalid model profile")
    return ModelProfile(
        workspace_id=profile["workspaceId"],
        name=profile["name"],
        version=profile["version"],
        provider=profile["provider"],
        model=profile["model"],
        secret_ref=profile.get("secretRef"),
        base_url=profile.get("baseUrl"),
        routing=profile.get("routing") or {},
        updated_at_utc=profile.get("updatedAtUtc", ""),
    )


def _unwrap_model_profiles(value: Any) -> List[ModelProfile]:
    profiles = value.get("profiles") if isinstance(value, dict) and isinstance(value.get("profiles"), list) else value
    if not isinstance(profiles, list):
        raise RuntimeError("Muzen remote returned invalid model profiles")
    return [_unwrap_model_profile(profile) for profile in profiles]


def _unwrap_provider_profile(value: Any) -> ProviderProfile:
    profile = value.get("profile") if isinstance(value, dict) and isinstance(value.get("profile"), dict) else value
    if not isinstance(profile, dict):
        raise RuntimeError("Muzen remote returned an invalid provider profile")
    return ProviderProfile(
        workspace_id=profile["workspaceId"],
        name=profile["name"],
        version=profile["version"],
        provider=profile["provider"],
        secret_ref=profile.get("secretRef"),
        base_url=profile.get("baseUrl"),
        routing=profile.get("routing") or {},
        updated_at_utc=profile.get("updatedAtUtc", ""),
    )


def _unwrap_provider_profiles(value: Any) -> List[ProviderProfile]:
    profiles = value.get("profiles") if isinstance(value, dict) and isinstance(value.get("profiles"), list) else value
    if not isinstance(profiles, list):
        raise RuntimeError("Muzen remote returned invalid provider profiles")
    return [_unwrap_provider_profile(profile) for profile in profiles]


def _remote_source(value: Dict[str, Any]) -> ReviewSource:
    if value["type"] == "local":
        return ReviewSource(
            type="local",
            repo=value["repo"],
        )
    if value["type"] == "raw_snapshot":
        return ReviewSource(
            type="raw_snapshot",
            root=value["root"],
        )
    if value["type"] == "perforce_changelist":
        return ReviewSource(
            type="perforce_changelist",
            server=value.get("server"),
            changelist=value.get("changelist"),
            client=value.get("client"),
            depot_paths=value.get("depotPaths", []),
        )
    if value["type"] == "custom":
        return ReviewSource(
            type="custom",
            provider=value.get("provider"),
            id=value.get("id"),
        )
    return ReviewSource(
        type=value["type"],
        owner=value.get("owner"),
        repo=value.get("repo"),
        number=value.get("number"),
    )


def _remote_result(value: Dict[str, Any]) -> ReviewResult:
    return ReviewResult(
        review_id=value["reviewId"],
        session_id=value["sessionId"],
        status=_map_runner_status(value["status"]),
        conclusion=value["conclusion"],
        summary=value["summary"],
        findings=[
            ReviewFinding(
                id=finding.get("id", ""),
                severity=finding.get("severity", "info"),
                category=finding.get("category", "other"),
                title=finding.get("title", ""),
                message=finding.get("message", ""),
                location=finding.get("location"),
                suggested_fix=finding.get("suggestedFix"),
                confidence=finding.get("confidence"),
                validation_status=finding.get("validationStatus"),
                evidence=[
                    ReviewFindingEvidence(
                        evidence_id=evidence.get("evidenceId", ""),
                        artifact_id=evidence.get("artifactId", ""),
                        kind=evidence.get("kind", ""),
                        content_hash=evidence.get("contentHash", ""),
                        producing_tool_call_id=evidence.get("producingToolCallId", ""),
                    )
                    for evidence in finding.get("evidence", [])
                    if isinstance(evidence, dict)
                ],
                discovered_by=list(finding.get("discoveredBy", [])),
                validated_by=list(finding.get("validatedBy", [])),
                challenged_by=list(finding.get("challengedBy", [])),
            )
            for finding in value.get("findings", [])
        ],
        coverage=ReviewCoverage(
            files_considered=value.get("coverage", {}).get("filesConsidered", 0),
            files_reviewed=value.get("coverage", {}).get("filesReviewed", 0),
            files_skipped=value.get("coverage", {}).get("filesSkipped", 0),
        ),
        metadata=value.get("metadata") or {},
    )
