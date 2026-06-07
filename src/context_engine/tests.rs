use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::context_engine::*;
use crate::contracts::{
    AgentBudget, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1, Role,
    TokenUsage,
};
use crate::reviewer::events::{InMemoryReviewEventSink, ReviewEvent};
use crate::reviewer::model::{ReviewModel, ReviewModelRequest, ReviewModelTurn};
use crate::reviewer::run::Run;
use crate::reviewer::snapshots::{ChangeSpec, ChangedFileSpec, SnapshotSpec};
use crate::reviewer::spec::{ReviewRunLimits, ReviewSessionSpec, RunSpec};
use crate::runtime::contracts::{RuntimeError, SessionInstruction};
use crate::runtime::repo::RepoSnapshot;

#[test]
fn config_defaults_to_disabled() {
    let config = ContextEngineConfig::default();
    assert_eq!(config.mode, ContextEngineMode::Disabled);
    assert_eq!(config.semantic.mode, ContextSemanticMode::NoVector);
    assert!(config.include_repository_guidance);
    assert!(!config.include_host_context);
}

#[test]
fn context_contracts_serde_round_trip() {
    let evidence = ContextEvidence {
        id: crate::runtime::contracts::EvidenceId("ev_1".to_string()),
        kind: ContextEvidenceKind::FileSpan,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(crate::runtime::contracts::RepoPath::parse("src/lib.rs").unwrap()),
        revision: Some(ContextRevision::head()),
        range: Some(ContextRange {
            start_line: 1,
            end_line: 3,
        }),
        content_hash: Some("hash".to_string()),
        summary: Some("summary".to_string()),
        token_estimate: 10,
        provenance: ContextProvenance {
            provider: "test".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some("snap".to_string()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    };
    let json = serde_json::to_string(&evidence).unwrap();
    let decoded: ContextEvidence = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, evidence);
}

#[test]
fn semantic_config_defaults_to_no_vector_and_blocks_restricted_hosted_inputs() {
    let mut config = ContextEngineConfig::snapshot_v0();
    let evidence = ContextEvidence {
        id: crate::runtime::contracts::EvidenceId("ev_restricted".to_string()),
        kind: ContextEvidenceKind::FileSpan,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Restricted,
        scope: ContextScope::Snapshot,
        path: None,
        revision: None,
        range: None,
        content_hash: None,
        summary: Some("restricted evidence".to_string()),
        token_estimate: 4,
        provenance: ContextProvenance {
            provider: "test".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: None,
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    };

    assert_eq!(
        semantic_input_decision(&config, &evidence),
        SemanticInputDecision::SkippedNoVector
    );

    config.semantic.mode = ContextSemanticMode::Hosted;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Hosted);
    config.semantic.max_embedding_inputs = 8;
    assert_eq!(
        semantic_input_decision(&config, &evidence),
        SemanticInputDecision::SkippedRestrictedHosted
    );
    let error = validate_embedding_batch(
        &config,
        &[EmbeddingInput {
            id: "ev_restricted".to_string(),
            text: "restricted evidence".to_string(),
            sensitivity: ContextSensitivity::Restricted,
        }],
    )
    .unwrap_err();
    assert!(
        matches!(error, RuntimeError::InvalidInput(message) if message.contains("restricted evidence"))
    );
}

#[test]
fn hosted_semantic_config_serializes_and_requires_credential() {
    let mut config = ContextEngineConfig::snapshot_v0();
    config.semantic.mode = ContextSemanticMode::Hosted;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Hosted);
    config.semantic.hosted_base_url = Some("https://embeddings.example/v1".to_string());
    config.semantic.hosted_model = Some("text-embedding-3-small".to_string());
    config.semantic.hosted_credential_ref =
        Some("env:MUZEN_TEST_MISSING_CONTEXT_EMBEDDING_KEY".to_string());
    config.semantic.max_embedding_inputs = 8;

    let serialized = serde_json::to_value(&config).unwrap();
    assert_eq!(
        serialized
            .pointer("/semantic/hostedBaseUrl")
            .and_then(serde_json::Value::as_str),
        Some("https://embeddings.example/v1")
    );
    assert!(matches!(
        HostedEmbeddingProvider::from_config(&config.semantic),
        Err(RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn noop_engine_reports_disabled() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "fn main() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let request = ContextIndexRequest::for_snapshot(snapshot, &ContextEngineConfig::disabled());
    let error = NoopContextEngine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(message) if message.contains("disabled")));
}

#[tokio::test]
async fn snapshot_engine_indexes_captured_snapshot() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CONTEXT.md"), "# Context\n").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn changed_symbol() {}\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.path().join("tests")).unwrap();
    std::fs::write(
        repo.path().join("tests/lib_test.rs"),
        "#[test]\nfn changed_test() {}\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    let request = ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());

    let report = engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(report.snapshot_id, snapshot.snapshot_id);
    assert_eq!(report.indexed_files, 3);
    assert_eq!(report.indexed_changed_files, 1);
    assert_eq!(report.rule_count, 1);
    assert!(report.evidence_count >= 3);

    let index = engine.store().get_index(&snapshot.snapshot_id).unwrap();
    assert!(index
        .evidence
        .iter()
        .any(|evidence| evidence.kind == ContextEvidenceKind::RepositoryRule));
    assert!(index
        .evidence
        .iter()
        .any(|evidence| evidence.kind == ContextEvidenceKind::Symbol));
    assert_eq!(
        index.manifest_artifact.schema_version,
        CONTEXT_MANIFEST_SCHEMA_VERSION
    );
}

#[tokio::test]
async fn snapshot_engine_builds_purpose_specific_pack() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CONTEXT.md"), "# Context\n").unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    std::fs::create_dir_all(repo.path().join("tests")).unwrap();
    std::fs::write(
        repo.path().join("tests/lib_test.rs"),
        "#[test]\nfn changed_test() {}\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let tests_pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: Some("run".to_string()),
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Tests,
                max_tokens: 10_000,
                seed_evidence: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let architecture_pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: Some("run".to_string()),
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Architecture,
                max_tokens: 10_000,
                seed_evidence: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(tests_pack.purpose, ContextPackPurpose::Tests);
    assert_eq!(architecture_pack.purpose, ContextPackPurpose::Architecture);
    assert_eq!(tests_pack.evidence[0].kind, ContextEvidenceKind::Test);
    assert_eq!(
        architecture_pack.evidence[0].kind,
        ContextEvidenceKind::RepositoryRule
    );

    let explanation = engine
        .query(
            ContextQuery {
                run_id: Some("run".to_string()),
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::StandaloneQuery),
                kind: ContextQueryKind::ExplainPack,
                arguments: serde_json::json!({
                    "packId": tests_pack.id.0,
                    "includeOmitted": true,
                }),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let data = explanation.data.unwrap();
    assert_eq!(
        data.get("packId").and_then(serde_json::Value::as_str),
        Some(tests_pack.id.0.as_str())
    );
    assert!(data
        .get("included")
        .and_then(serde_json::Value::as_array)
        .unwrap()
        .iter()
        .any(|item| item
            .get("why")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .any(|why| why
                .as_str()
                .unwrap()
                .contains("tests pack prioritizes related tests"))));
}

#[tokio::test]
async fn snapshot_engine_queries_indexed_evidence() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src/auth")).unwrap();
    std::fs::write(
        repo.path().join("src/auth/token.rs"),
        "pub fn authorize_request() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/auth/routes.rs"),
        "use crate::auth::token::authorize_request;\npub fn route() { authorize_request(); }\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.path().join("tests/auth")).unwrap();
    std::fs::write(
        repo.path().join("tests/auth/token_test.rs"),
        "#[test]\nfn authorize_request_test() {}\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/auth/token.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::SearchText,
                arguments: serde_json::json!({"query": "token"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(result
        .evidence
        .iter()
        .any(|evidence| evidence.path.as_ref().unwrap().display() == "src/auth/token.rs"));

    let span = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::ReadSpan,
                arguments: serde_json::json!({
                    "path": "src/auth/token.rs",
                    "startLine": 1,
                    "endLine": 1
                }),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(span
        .data
        .unwrap()
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .contains("authorize_request"));

    let related_symbols = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::RelatedSymbols,
                arguments: serde_json::json!({"path": "src/auth/token.rs"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(related_symbols.evidence.iter().any(|evidence| evidence.kind
        == ContextEvidenceKind::Symbol
        && evidence
            .summary
            .as_ref()
            .map(|summary| summary.contains("authorize_request"))
            .unwrap_or(false)));
    let authorize_symbol = related_symbols
        .evidence
        .iter()
        .find(|evidence| {
            evidence.kind == ContextEvidenceKind::Symbol
                && evidence
                    .summary
                    .as_ref()
                    .map(|summary| summary.contains("authorize_request"))
                    .unwrap_or(false)
        })
        .unwrap();
    assert_eq!(
        authorize_symbol.range,
        Some(ContextRange {
            start_line: 1,
            end_line: 1,
        })
    );
    assert!(related_symbols.evidence.iter().any(|evidence| evidence
        .path
        .as_ref()
        .unwrap()
        .display()
        == "src/auth/routes.rs"));
}

#[tokio::test]
async fn local_semantic_mode_builds_vector_index_for_search() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn authorize_mobile_token() -> bool { true }\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let mut config = ContextEngineConfig::snapshot_v0();
    config.semantic.mode = ContextSemanticMode::Local;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Local);
    config.semantic.max_embedding_inputs = 32;
    let engine = SnapshotContextEngine::new(config);
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    assert!(index.semantic_vectors.is_some());

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::SearchText,
                arguments: serde_json::json!({"query": "mobile token"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result
        .evidence
        .iter()
        .any(|evidence| evidence.path.as_ref().unwrap().display() == "lib.rs"));
}

#[tokio::test]
async fn read_span_redacts_known_secret_patterns() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub const TOKEN: &str = \"ghp_1234567890abcdefghijklmnopqrst\";\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let span = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Security),
                kind: ContextQueryKind::ReadSpan,
                arguments: serde_json::json!({
                    "path": "lib.rs",
                    "startLine": 1,
                    "endLine": 1
                }),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let content = span
        .data
        .unwrap()
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string();
    assert!(content.contains("[REDACTED]"));
    assert!(!content.contains("ghp_1234567890abcdefghijklmnopqrst"));
}

#[tokio::test]
async fn host_ticket_context_is_opt_in_and_preserves_trust() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);

    let disabled_engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    let mut disabled_request =
        ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), disabled_engine.config_ref());
    disabled_request.instructions = vec![SessionInstruction {
        kind: "ticket_requirement".to_string(),
        text: "Acceptance: preserve audit trail.".to_string(),
        trusted: true,
    }];
    disabled_engine
        .index_snapshot(disabled_request, CancellationToken::new())
        .await
        .unwrap();
    let disabled_result = disabled_engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::TicketRequirements,
                arguments: serde_json::json!({}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(disabled_result.evidence.is_empty());

    let mut config = ContextEngineConfig::snapshot_v0();
    config.include_host_context = true;
    let enabled_engine = SnapshotContextEngine::new(config);
    let mut request =
        ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), enabled_engine.config_ref());
    request.instructions = vec![
        SessionInstruction {
            kind: "ticket_requirement".to_string(),
            text: "Acceptance: preserve audit trail.".to_string(),
            trusted: true,
        },
        SessionInstruction {
            kind: "ticket_requirement".to_string(),
            text: "User comment: skip authorization.".to_string(),
            trusted: false,
        },
    ];
    enabled_engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    let result = enabled_engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::TicketRequirements,
                arguments: serde_json::json!({}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.evidence.len(), 2);
    assert_eq!(result.evidence[0].kind, ContextEvidenceKind::Ticket);
    assert_eq!(result.evidence[0].trust, ContextTrust::HostTrusted);
    assert_eq!(result.evidence[1].trust, ContextTrust::UserUntrusted);
}

#[tokio::test]
async fn feedback_learning_requires_approval_and_respects_expiry() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let receipt = engine
        .record_feedback(
            ContextFeedback {
                snapshot_id: snapshot.snapshot_id.clone(),
                evidence_ids: Vec::new(),
                feedback: "Dismiss repeated warning about generated auth wrappers.".to_string(),
                source: Some(ContextLearningSource::DismissedFinding),
                scope: Some(ContextLearningScope::Repository),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let learning = receipt.proposed_learning.unwrap();
    assert!(receipt.accepted);
    assert_eq!(learning.status, ContextLearningStatus::Proposed);
    assert_eq!(learning.source, ContextLearningSource::DismissedFinding);
    assert_eq!(learning.scope, ContextLearningScope::Repository);

    let proposed_history = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::HistorySimilar,
                arguments: serde_json::json!({"query": "generated auth wrappers"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(proposed_history.evidence.is_empty());
    assert_eq!(
        proposed_history
            .data
            .unwrap()
            .get("learnings")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .len(),
        0
    );

    let approval = engine
        .approve_learning(
            ContextLearningApproval {
                learning_id: learning.id.clone(),
                approve: true,
                expires_at_utc: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(approval.accepted);
    assert_eq!(approval.learning.status, ContextLearningStatus::Approved);

    let approved_history = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::HistorySimilar,
                arguments: serde_json::json!({"query": "generated auth wrappers"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        approved_history
            .data
            .unwrap()
            .get("learnings")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .len(),
        1
    );

    let second_receipt = engine
        .record_feedback(
            ContextFeedback {
                snapshot_id: snapshot.snapshot_id.clone(),
                evidence_ids: Vec::new(),
                feedback: "Do not learn this rejected pattern.".to_string(),
                source: Some(ContextLearningSource::HumanFeedback),
                scope: Some(ContextLearningScope::Workspace),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let rejected_learning = second_receipt.proposed_learning.unwrap();
    let rejection = engine
        .approve_learning(
            ContextLearningApproval {
                learning_id: rejected_learning.id,
                approve: false,
                expires_at_utc: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(rejection.learning.status, ContextLearningStatus::Rejected);

    let expired_receipt = engine
        .record_feedback(
            ContextFeedback {
                snapshot_id: snapshot.snapshot_id.clone(),
                evidence_ids: Vec::new(),
                feedback: "Expired generated auth wrapper learning.".to_string(),
                source: Some(ContextLearningSource::ManualRule),
                scope: Some(ContextLearningScope::Repository),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let expired_learning = expired_receipt.proposed_learning.unwrap();
    engine
        .approve_learning(
            ContextLearningApproval {
                learning_id: expired_learning.id,
                approve: true,
                expires_at_utc: Some("1".to_string()),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let filtered_history = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::HistorySimilar,
                arguments: serde_json::json!({"query": "learning"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let learning_count = filtered_history
        .data
        .unwrap()
        .get("learnings")
        .and_then(serde_json::Value::as_array)
        .unwrap()
        .len();
    assert_eq!(learning_count, 0);
}

#[tokio::test]
async fn file_learning_store_persists_approved_learnings_across_engine_restarts() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let store_dir = tempfile::tempdir().unwrap();
    let store_path = store_dir.path().join("context-learnings.json");

    let engine = SnapshotContextEngine::with_learning_store_file(
        ContextEngineConfig::snapshot_v0(),
        &store_path,
    )
    .unwrap();
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let receipt = engine
        .record_feedback(
            ContextFeedback {
                snapshot_id: snapshot.snapshot_id.clone(),
                evidence_ids: Vec::new(),
                feedback: "Remember generated auth wrappers are intentionally duplicated."
                    .to_string(),
                source: Some(ContextLearningSource::ManualRule),
                scope: Some(ContextLearningScope::Repository),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let learning = receipt.proposed_learning.unwrap();
    engine
        .approve_learning(
            ContextLearningApproval {
                learning_id: learning.id,
                approve: true,
                expires_at_utc: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let restarted = SnapshotContextEngine::with_learning_store_file(
        ContextEngineConfig::snapshot_v0(),
        &store_path,
    )
    .unwrap();
    restarted
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), restarted.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let history = restarted
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::HistorySimilar,
                arguments: serde_json::json!({"query": "generated auth wrappers"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .data
            .unwrap()
            .get("learnings")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn cross_repo_contracts_report_capability_omission_without_host_evidence() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Architecture),
                kind: ContextQueryKind::CrossRepoContracts,
                arguments: serde_json::json!({"query": "consumer"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(result.evidence.is_empty());
    assert_eq!(
        result.data.unwrap()["omissions"][0]["reason"],
        serde_json::json!("requires_ungranted_capability")
    );
}

#[tokio::test]
async fn cross_repo_contracts_returns_host_provided_scoped_evidence() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let mut config = ContextEngineConfig::snapshot_v0();
    config.include_host_context = true;
    let engine = SnapshotContextEngine::new(config);
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());
    request.host_metadata.insert(
        "crossRepoContract:consumer-api".to_string(),
        serde_json::json!({
            "repository": "acme/mobile",
            "contract": "auth token response must keep expires_at"
        }),
    );
    engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Architecture),
                kind: ContextQueryKind::CrossRepoContracts,
                arguments: serde_json::json!({"query": "expires_at"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.evidence.len(), 1);
    assert_eq!(
        result.evidence[0].kind,
        ContextEvidenceKind::CrossRepoContract
    );
    assert_eq!(result.evidence[0].source, ContextEvidenceSource::Host);
    assert_eq!(result.evidence[0].scope, ContextScope::Run);
    assert!(result.evidence[0].path.is_none());
}

#[tokio::test]
async fn cross_repo_contracts_require_granted_provider_resource() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());
    request
        .cross_repo_contracts
        .push(CrossRepoContractCandidate {
            resource_id: "github/acme/mobile".to_string(),
            repository: "acme/mobile".to_string(),
            summary: "consumer requires expires_at on auth token response".to_string(),
            original_url: Some("https://example.invalid/acme/mobile/contracts/auth".to_string()),
        });
    engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Architecture),
                kind: ContextQueryKind::CrossRepoContracts,
                arguments: serde_json::json!({"query": "expires_at"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(result.evidence.is_empty());
    assert_eq!(result.data.unwrap()["omissions"][0]["deniedCandidates"], 1);
}

#[tokio::test]
async fn cross_repo_contracts_return_granted_provider_resource() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["lib.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());
    request
        .allowed_cross_repo_resources
        .insert("github/acme/mobile".to_string());
    request
        .cross_repo_contracts
        .push(CrossRepoContractCandidate {
            resource_id: "github/acme/mobile".to_string(),
            repository: "acme/mobile".to_string(),
            summary: "consumer requires expires_at on auth token response".to_string(),
            original_url: Some("https://example.invalid/acme/mobile/contracts/auth".to_string()),
        });
    request
        .cross_repo_contracts
        .push(CrossRepoContractCandidate {
            resource_id: "github/acme/admin".to_string(),
            repository: "acme/admin".to_string(),
            summary: "admin consumer requires legacy token field".to_string(),
            original_url: None,
        });
    engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Architecture),
                kind: ContextQueryKind::CrossRepoContracts,
                arguments: serde_json::json!({"query": "expires_at"}),
                current_evidence: Vec::new(),
                limits: ContextQueryLimits {
                    max_results: 10,
                    max_tokens: 1000,
                },
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.evidence.len(), 1);
    assert_eq!(result.evidence[0].source, ContextEvidenceSource::External);
    assert_eq!(result.evidence[0].trust, ContextTrust::ToolProvider);
    assert_eq!(result.evidence[0].scope, ContextScope::External);
    assert_eq!(
        result.evidence[0].provenance.original_url.as_deref(),
        Some("https://example.invalid/acme/mobile/contracts/auth")
    );
    assert_eq!(result.data.unwrap()["deniedCandidates"], 1);
}

#[tokio::test]
async fn enabled_context_engine_emits_index_and_pack_events_for_run() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CONTEXT.md"), "# Context\n").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();

    let snapshot = SnapshotSpec::new(
        repo.path(),
        ChangeSpec::local(
            "local",
            "head",
            vec![ChangedFileSpec::modified("src/lib.rs")],
        ),
    );
    let session = ReviewSessionSpec::review_read_only(
        "correctness",
        Role::Correctness,
        "Review changed code",
        AgentBudget {
            max_turns: 1,
            max_tool_calls: 0,
            max_prompt_tokens: 4000,
            max_output_tokens: 1000,
        },
    );
    let events = Arc::new(InMemoryReviewEventSink::default());
    let report = Run::builder(RunSpec::single_snapshot(
        "context-run",
        snapshot,
        vec![session],
        ReviewRunLimits::standard(1, 200 * 1024, 20),
    ))
    .review_model(Arc::new(CleanModel))
    .context_engine(Arc::new(SnapshotContextEngine::new(
        ContextEngineConfig::snapshot_v0(),
    )))
    .review_event_sink(events.clone())
    .build()
    .unwrap()
    .execute()
    .await;

    assert_eq!(report.summary.sessions, 1);
    assert!(report.summary.artifacts >= 2);
    let artifact_contents = report
        .redacted_artifacts()
        .export()
        .unwrap()
        .artifacts
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>();
    assert!(artifact_contents
        .iter()
        .any(|content| content.contains("muzen.context_manifest.v1")));
    assert!(artifact_contents
        .iter()
        .any(|content| content.contains("\"purpose\": \"correctness\"")));
    assert!(artifact_contents
        .iter()
        .any(|content| content.contains("muzen.context_findings_evidence.v1")));
    let emitted = events.events();
    assert!(emitted
        .iter()
        .any(|event| matches!(event, ReviewEvent::ContextIndexCompleted { .. })));
    assert!(emitted
        .iter()
        .any(|event| matches!(event, ReviewEvent::ContextPackCompleted { .. })));
}

struct CleanModel;

#[async_trait]
impl ReviewModel for CleanModel {
    async fn complete_review(
        &self,
        _request: ReviewModelRequest,
        _cancel: crate::reviewer::adapters::Cancellation,
    ) -> crate::runtime::contracts::RuntimeResult<ReviewModelTurn> {
        Ok(ReviewModelTurn::Text {
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
            },
            content: serde_json::json!({
                "summary": "clean",
                "fileVerdicts": [{
                    "path": "src/lib.rs",
                    "verdict": "clean",
                    "summary": "clean",
                    "relatedPaths": []
                }],
                "findings": []
            })
            .to_string(),
        })
    }
}

fn build_snapshot(root: &std::path::Path, changed_files: Vec<&str>) -> Arc<RepoSnapshot> {
    let change = ChangeScopeV1 {
        kind: crate::contracts::ChangeKind::LocalDiff,
        change_id: "local".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: Some("@@ -1 +1 @@\n+changed\n".to_string()),
        snapshot_mode: crate::contracts::SnapshotMode::WorktreeHead,
        rename_detection: crate::contracts::RenameDetection::None,
        changed_files: changed_files
            .into_iter()
            .map(|path| ChangedFileEntryV1 {
                status: ChangedFileStatus::Modified,
                old_path: Some(std::path::PathBuf::from(path)),
                new_path: Some(std::path::PathBuf::from(path)),
                old_content_hash: None,
                new_content_hash: None,
                is_binary: false,
                is_generated: false,
            })
            .collect(),
    };
    RepoSnapshot::build(root, &PathPolicyV1::bench(200, 120), &change).unwrap()
}
