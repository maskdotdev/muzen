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
        is_changed_span: false,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
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
        is_changed_span: false,
        representation: ContextEvidenceRepresentation::FullContent,
        skeleton_text: None,
        signals: ContextRankSignals::default(),
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
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(tests_pack.purpose, ContextPackPurpose::Tests);
    assert_eq!(architecture_pack.purpose, ContextPackPurpose::Architecture);
    // The diff manifest anchors every pack near the top; change-rooted
    // evidence leads, and purpose differentiates the ordering of
    // non-changed evidence.
    let diff_position = tests_pack
        .evidence
        .iter()
        .position(|evidence| evidence.kind == ContextEvidenceKind::Diff)
        .expect("diff manifest is present in the pack");
    assert!(
        diff_position < 3,
        "diff manifest ranks near the top, got position {diff_position}"
    );
    let position = |pack: &ContextPack, kind: ContextEvidenceKind| {
        pack.evidence
            .iter()
            .position(|evidence| evidence.kind == kind)
            .unwrap_or(usize::MAX)
    };
    assert!(
        position(&tests_pack, ContextEvidenceKind::Test)
            < position(&tests_pack, ContextEvidenceKind::RepositoryRule),
        "tests purpose ranks tests above repository guidance"
    );
    assert!(
        position(&architecture_pack, ContextEvidenceKind::RepositoryRule)
            < position(&architecture_pack, ContextEvidenceKind::Test),
        "architecture purpose ranks repository guidance above tests"
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
    let fusion = result
        .data
        .as_ref()
        .and_then(|data| data.get("fusion"))
        .and_then(|fusion| fusion.as_array())
        .expect("local semantic search reports fusion ranks");
    assert!(fusion
        .iter()
        .any(|trace| trace.get("semanticRank").is_some() && trace.get("lexicalRank").is_some()));
}

#[tokio::test]
async fn no_vector_search_is_pure_bm25_with_lexical_only_fusion_ranks() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn authorize_request() -> bool { true }\npub fn unrelated() {}\n",
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
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    assert!(index.semantic_vectors.is_none());

    let config = ContextEngineConfig::snapshot_v0();
    let bm25 = index
        .lexical
        .search("authorizeRequest", 10, config.bm25_k1, config.bm25_b);
    assert!(!bm25.is_empty());

    let result = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::Correctness),
                kind: ContextQueryKind::SearchText,
                arguments: serde_json::json!({"query": "authorizeRequest"}),
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
    let returned_ids = result
        .evidence
        .iter()
        .map(|evidence| evidence.id.0.clone())
        .collect::<Vec<_>>();
    let bm25_ids = bm25
        .iter()
        .take(returned_ids.len())
        .map(|(id, _score)| id.0.clone())
        .collect::<Vec<_>>();
    assert_eq!(returned_ids, bm25_ids);
    let fusion = result
        .data
        .as_ref()
        .and_then(|data| data.get("fusion"))
        .and_then(|fusion| fusion.as_array())
        .expect("search reports fusion ranks");
    assert!(fusion
        .iter()
        .all(|trace| trace.get("semanticRank").is_none() && trace.get("lexicalRank").is_some()));
}

#[tokio::test]
async fn hunk_inside_function_maps_to_enclosing_chunk_not_file() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/big.rs"),
        "pub fn first() {\n    let a = 1;\n    let b = 2;\n}\n\npub fn second() {\n    let c = 3;\n}\n",
    )
    .unwrap();
    let diff = "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -2,2 +2,2 @@\n+    let a = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/big.rs"], diff);
    let mut config = ContextEngineConfig::snapshot_v0();
    // Force one chunk per function so enclosure is observable.
    config.chunk_max_tokens = 16;
    let engine = SnapshotContextEngine::new(config);
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let enclosing: Vec<_> = index
        .relationships
        .iter()
        .filter(|relationship| relationship.kind == ContextRelationshipKind::EnclosesHunk)
        .collect();
    assert!(!enclosing.is_empty(), "changed chunk maps to the hunk");
    for relationship in &enclosing {
        let from = index
            .evidence
            .iter()
            .find(|evidence| evidence.id == relationship.from)
            .unwrap();
        let range = from.range.expect("enclosing evidence cites a real range");
        assert!(
            range.start_line <= 2 && range.end_line >= 2,
            "chunk encloses the changed line, got {range:?}"
        );
        assert!(
            range.end_line < 8,
            "maps to the function chunk, not the whole file"
        );
    }
}

#[tokio::test]
async fn changed_ts_export_surfaces_importing_call_sites_without_collisions() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src/a")).unwrap();
    std::fs::create_dir_all(repo.path().join("src/b")).unwrap();
    std::fs::write(
        repo.path().join("src/a/load.ts"),
        "export function loadUser(id: string) { return id; }\n",
    )
    .unwrap();
    // Same exported name in an unrelated module: must not collide.
    std::fs::write(
        repo.path().join("src/b/load.ts"),
        "export function loadUser(id: string) { return id + '!'; }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/app.ts"),
        "import { loadUser } from './a/load';\nexport function main() { return loadUser('1'); }\n",
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/a/load.ts"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let called_by: Vec<_> = index
        .graph_expansion
        .candidates
        .iter()
        .filter(|candidate| candidate.relationship_kind() == ContextRelationshipKind::CalledBy)
        .collect();
    assert!(called_by.iter().any(|candidate| candidate
        .repo_path()
        .map(|path| path.display())
        .as_deref()
        == Some("src/app.ts")));
    assert!(
        !index
            .graph_expansion
            .candidates
            .iter()
            .any(
                |candidate| candidate.repo_path().map(|path| path.display()).as_deref()
                    == Some("src/b/load.ts")
                    && candidate.relationship_kind() == ContextRelationshipKind::CalledBy
            ),
        "same-named module in another directory must not surface as a caller"
    );
    assert!(
        index
            .relationships
            .iter()
            .any(|relationship| { relationship.kind == ContextRelationshipKind::CalledBy }),
        "graph candidates become typed relationships"
    );
}

#[tokio::test]
async fn fused_search_records_restricted_evidence_as_omission() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn authorize_request() -> bool { true }\n",
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
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    let config = ContextEngineConfig::snapshot_v0();
    let top = index
        .lexical
        .search("authorize_request", 1, config.bm25_k1, config.bm25_b)
        .first()
        .map(|(id, _score)| id.clone())
        .expect("query matches evidence");
    for evidence in &mut index.evidence {
        if evidence.id == top {
            evidence.sensitivity = ContextSensitivity::Restricted;
        }
    }

    let outcome = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(outcome.evidence.iter().all(|evidence| evidence.id != top));
    assert!(outcome
        .omissions
        .iter()
        .any(|omission| omission.evidence_id == top.0 && omission.reason == "restricted"));
}

async fn sufficiency_fixture() -> (SnapshotContextEngine, Arc<RepoSnapshot>, tempfile::TempDir) {
    sufficiency_fixture_with_extra_files(0).await
}

async fn sufficiency_fixture_with_extra_files(
    extra_files: usize,
) -> (SnapshotContextEngine, Arc<RepoSnapshot>, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::create_dir_all(repo.path().join("tests")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn changed_fn() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/caller.rs"),
        "use crate::lib::changed_fn;\npub fn call() { changed_fn(); }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("tests/lib_test.rs"),
        "use crate::lib::changed_fn;\n#[test]\nfn changed_fn_works() { changed_fn(); }\n",
    )
    .unwrap();
    for index in 0..extra_files {
        std::fs::write(
            repo.path().join(format!("src/extra_{index}.rs")),
            format!(
                "pub fn extra_{index}() {{\n{}\n}}\n",
                (0..80)
                    .map(|line| format!("    let value_{line} = {line};"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .unwrap();
    }
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2,2 +2,2 @@\n+    let a = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/lib.rs"], diff);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (engine, snapshot, repo)
}

fn sufficiency_query(
    snapshot: &RepoSnapshot,
    kind: ContextQueryKind,
    arguments: serde_json::Value,
    current_evidence: Vec<crate::runtime::contracts::EvidenceId>,
) -> ContextQuery {
    ContextQuery {
        run_id: None,
        snapshot_id: snapshot.snapshot_id.clone(),
        session_id: None,
        purpose: Some(ContextPackPurpose::Correctness),
        kind,
        arguments,
        current_evidence,
        limits: ContextQueryLimits {
            max_results: 10,
            max_tokens: 2000,
        },
    }
}

#[tokio::test]
async fn missing_enclosing_definition_gap_clears_after_running_suggested_query() {
    let (engine, snapshot, _repo) = sufficiency_fixture().await;
    let check = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                Vec::new(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sufficiency = check.sufficiency.unwrap();
    assert_eq!(sufficiency.status, ContextSufficiencyStatus::Insufficient);
    let gap = sufficiency
        .gaps
        .iter()
        .find(|gap| {
            gap.missing
                .contains(&ContextCoverageGapKind::EnclosingDefinition)
        })
        .expect("empty evidence reports an enclosing_definition gap");

    // The suggested query is runnable as-is and returns the evidence
    // that clears the gap.
    let kind: ContextQueryKind =
        serde_json::from_value(gap.suggested_query["kind"].clone()).unwrap();
    let filled = engine
        .query(
            sufficiency_query(
                &snapshot,
                kind,
                gap.suggested_query["arguments"].clone(),
                Vec::new(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!filled.evidence.is_empty());
    let recheck = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                filled
                    .evidence
                    .iter()
                    .map(|evidence| evidence.id.clone())
                    .collect(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let recheck_sufficiency = recheck.sufficiency.unwrap();
    assert!(
        !recheck_sufficiency.gaps.iter().any(|gap| {
            gap.missing
                .contains(&ContextCoverageGapKind::EnclosingDefinition)
        }),
        "running the suggested query clears the enclosing_definition gap"
    );
}

#[tokio::test]
async fn unreferenced_private_helper_does_not_demand_callers() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/helper.rs"),
        "fn private_helper() {\n    let a = 1;\n}\n",
    )
    .unwrap();
    let diff = "diff --git a/src/helper.rs b/src/helper.rs\n--- a/src/helper.rs\n+++ b/src/helper.rs\n@@ -2,1 +2,1 @@\n+    let a = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/helper.rs"], diff);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let current_evidence = index
        .evidence
        .iter()
        .filter(|evidence| evidence.is_changed_span)
        .map(|evidence| evidence.id.clone())
        .collect();
    let check = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                current_evidence,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sufficiency = check.sufficiency.unwrap();
    assert!(
        !sufficiency
            .gaps
            .iter()
            .any(|gap| gap.missing.contains(&ContextCoverageGapKind::Callers)),
        "a verifiably unreferenced helper must not demand callers"
    );
}

#[tokio::test]
async fn pack_sufficiency_equals_sufficiency_check_over_same_evidence() {
    let (engine, snapshot, _repo) = sufficiency_fixture().await;
    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens: 10_000,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let check = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                pack.evidence
                    .iter()
                    .map(|evidence| evidence.id.clone())
                    .collect(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let checked = check.sufficiency.unwrap();
    assert_eq!(pack.sufficiency.status, checked.status);
    assert_eq!(pack.sufficiency.gaps, checked.gaps);
}

#[tokio::test]
async fn pack_sufficiency_is_insufficient_when_ranked_candidates_are_omitted() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn changed_fn() {\n    let a = 1;\n}\n",
    )
    .unwrap();
    for index in 0..80 {
        std::fs::write(
            repo.path().join(format!("src/extra_{index}.rs")),
            format!(
                "pub fn extra_{index}() {{\n{}\n}}\n",
                (0..80)
                    .map(|line| format!("    let value_{line} = {line};"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .unwrap();
    }
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2,1 +2,1 @@\n+    let a = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/lib.rs"], diff);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens: 10_000,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !pack.omitted_candidates.is_empty(),
        "fixture must force pack candidate omissions"
    );
    assert_eq!(
        pack.sufficiency.status,
        ContextSufficiencyStatus::Insufficient
    );
    assert!(
        pack.sufficiency
            .missing
            .iter()
            .any(|item| item.contains("context is incomplete")),
        "pack reports why complete coverage is unproven"
    );
    let explanation = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::StandaloneQuery),
                kind: ContextQueryKind::ExplainPack,
                arguments: serde_json::json!({
                    "packId": pack.id.0,
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
    let explanation_data = explanation.data.expect("explain pack data");
    let omitted = explanation_data
        .get("omitted")
        .and_then(serde_json::Value::as_array)
        .expect("omitted candidates are an array");
    assert!(
        omitted.iter().any(|candidate| {
            candidate.get("kind").is_some()
                && candidate.get("path").is_some()
                && candidate.get("tokenEstimate").is_some()
                && candidate.get("rankIndex").is_some()
        }),
        "explain output carries omitted candidate metadata needed to debug budget misses"
    );

    let check = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                pack.evidence
                    .iter()
                    .map(|evidence| evidence.id.clone())
                    .collect(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let checked = check.sufficiency.unwrap();
    assert_eq!(
        checked.status,
        ContextSufficiencyStatus::Sufficient,
        "query checks selected evidence; pack adds budget-omission proof honesty"
    );
}

#[tokio::test]
async fn explain_pack_includes_graph_paths_for_omitted_candidates() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/core.ts"),
        "export function changedFn() {\n  return 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/caller.ts"),
        format!(
            "import {{ changedFn }} from \"./core\"\n\nexport function caller() {{\n  changedFn()\n{}\n}}\n",
            (0..220)
                .map(|line| format!("  const value{line} = {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .unwrap();
    let diff = "diff --git a/src/core.ts b/src/core.ts\n--- a/src/core.ts\n+++ b/src/core.ts\n@@ -2,1 +2,1 @@\n+  return 1\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/core.ts"], diff);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens: 350,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        pack.omitted_candidates.iter().any(|candidate| candidate
            .path
            .as_ref()
            .is_some_and(|path| path.display() == "src/caller.ts")),
        "caller full content should be omitted or downgraded under the small pack budget"
    );

    let explanation = engine
        .query(
            ContextQuery {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: Some(ContextPackPurpose::StandaloneQuery),
                kind: ContextQueryKind::ExplainPack,
                arguments: serde_json::json!({
                    "packId": pack.id.0,
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
    let explanation_data = explanation.data.expect("explain pack data");
    let omitted = explanation_data
        .get("omitted")
        .and_then(serde_json::Value::as_array)
        .expect("omitted candidates are an array");
    assert!(
        omitted.iter().any(|candidate| {
            candidate.get("path").and_then(serde_json::Value::as_str) == Some("src/caller.ts")
                && candidate
                    .get("graphPaths")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|paths| !paths.is_empty())
        }),
        "omitted graph-connected candidates should explain the graph path that made them relevant"
    );
}

#[tokio::test]
async fn budget_exhaustion_stays_insufficient_with_recorded_gaps() {
    let (engine, snapshot, _repo) = sufficiency_fixture().await;
    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!pack.omitted_candidates.is_empty());
    assert_eq!(
        pack.sufficiency.status,
        ContextSufficiencyStatus::Insufficient,
        "budget exhaustion explains the miss but does not prove sufficiency"
    );
    assert!(
        !pack.sufficiency.gaps.is_empty(),
        "unresolved gaps stay recorded"
    );
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
async fn trusted_host_ticket_enters_budgeted_pack_before_unrelated_snapshot_noise() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    for index in 0..40 {
        std::fs::write(
            repo.path().join(format!("src/noise_{index}.rs")),
            format!("pub fn unrelated_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap();
    }
    let snapshot = build_snapshot_with_diff(
        repo.path(),
        vec!["src/lib.rs"],
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn old() {}\n+pub fn changed() {}\n",
    );
    let mut config = ContextEngineConfig::snapshot_v0();
    config.include_host_context = true;
    let engine = SnapshotContextEngine::new(config);
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref());
    request.instructions = vec![SessionInstruction {
        kind: "ticket_requirement".to_string(),
        text: "Acceptance: preserve audit trail.".to_string(),
        trusted: true,
    }];
    engine
        .index_snapshot(request, CancellationToken::new())
        .await
        .unwrap();

    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::GeneralReview,
                max_tokens: 80,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let host_position = pack
        .evidence
        .iter()
        .position(|evidence| {
            evidence.source == ContextEvidenceSource::Host
                && evidence.kind == ContextEvidenceKind::Ticket
                && evidence.trust == ContextTrust::HostTrusted
        })
        .expect("trusted host ticket included under budget pressure");
    let first_noise_position = pack.evidence.iter().position(|evidence| {
        evidence
            .path
            .as_ref()
            .is_some_and(|path| path.display().starts_with("src/noise_"))
    });

    assert!(
        first_noise_position.map_or(true, |noise_position| host_position < noise_position),
        "trusted run-scoped ticket context should rank before unrelated snapshot noise"
    );
    assert!(
        pack.omitted_candidates.iter().any(|candidate| candidate
            .path
            .as_ref()
            .is_some_and(|path| path.display().starts_with("src/noise_"))),
        "fixture must exercise budget pressure from unrelated snapshot files"
    );
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
                cached_input_tokens: 0,
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

fn build_snapshot_with_diff(
    root: &std::path::Path,
    changed_files: Vec<&str>,
    inline_diff: &str,
) -> Arc<RepoSnapshot> {
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
        inline_diff: Some(inline_diff.to_string()),
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

fn many_function_rust_file(functions: usize, lines_per_fn: usize) -> String {
    let mut content = String::new();
    for index in 0..functions {
        content.push_str(&format!("pub fn generated_{index}() {{\n"));
        for line in 0..lines_per_fn {
            content.push_str(&format!("    let value_{line} = {line} + {index};\n"));
        }
        content.push_str("}\n\n");
    }
    content
}

#[tokio::test]
async fn index_emits_chunk_evidence_with_changed_span_flags() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/big.rs"),
        many_function_rust_file(40, 12),
    )
    .unwrap();
    // Hunk touches the first function only.
    let diff = "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -2,3 +2,4 @@\n+    let added = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/big.rs"], diff);
    let index = ContextIndex::build(ContextIndexRequest::for_snapshot(
        snapshot,
        &ContextEngineConfig::snapshot_v0(),
    ))
    .await
    .unwrap();

    let chunks = index
        .evidence
        .iter()
        .filter(|evidence| {
            evidence.kind == ContextEvidenceKind::FileSpan
                && evidence.path.as_ref().map(|path| path.display())
                    == Some("src/big.rs".to_string())
        })
        .collect::<Vec<_>>();
    assert!(chunks.len() > 1, "expected chunk-level evidence");
    for chunk in &chunks {
        let range = chunk.range.expect("chunk evidence must carry a range");
        assert!(range.start_line >= 1 && range.end_line >= range.start_line);
        assert!(
            chunk.token_estimate <= ContextEngineConfig::snapshot_v0().chunk_max_tokens,
            "chunk evidence exceeds chunk_max_tokens"
        );
        assert!(!chunk
            .summary
            .as_deref()
            .unwrap_or_default()
            .starts_with("changed"));
    }
    let changed_chunks = chunks
        .iter()
        .filter(|chunk| chunk.is_changed_span)
        .collect::<Vec<_>>();
    assert!(!changed_chunks.is_empty(), "hunk overlap must be marked");
    assert!(
        changed_chunks
            .iter()
            .all(|chunk| chunk.range.unwrap().start_line <= 5),
        "only the first function overlaps the hunk"
    );
    assert!(
        chunks.iter().any(|chunk| !chunk.is_changed_span),
        "untouched chunks must not be marked changed"
    );
}

#[tokio::test]
async fn chunk_evidence_ids_are_stable_across_index_runs() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/stable.rs"),
        many_function_rust_file(20, 10),
    )
    .unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/stable.rs"]);
    let config = ContextEngineConfig::snapshot_v0();
    let ids = |index: &ContextIndex| {
        index
            .evidence
            .iter()
            .map(|evidence| evidence.id.0.clone())
            .collect::<Vec<_>>()
    };
    let first = ContextIndex::build(ContextIndexRequest::for_snapshot(
        Arc::clone(&snapshot),
        &config,
    ))
    .await
    .unwrap();
    let second = ContextIndex::build(ContextIndexRequest::for_snapshot(
        Arc::clone(&snapshot),
        &config,
    ))
    .await
    .unwrap();
    assert_eq!(ids(&first), ids(&second));
}

#[tokio::test]
async fn pathological_file_respects_chunk_budget_and_records_skip() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/pathological.rs"),
        many_function_rust_file(300, 20),
    )
    .unwrap();
    let mut config = ContextEngineConfig::snapshot_v0();
    config.max_chunks_per_file = 4;
    let snapshot = build_snapshot(repo.path(), vec!["src/pathological.rs"]);
    let index = ContextIndex::build(ContextIndexRequest::for_snapshot(snapshot, &config))
        .await
        .unwrap();

    let chunk_count = index
        .evidence
        .iter()
        .filter(|evidence| {
            evidence.kind == ContextEvidenceKind::FileSpan
                && evidence.provenance.provider == "snapshot_chunk_v1"
        })
        .count();
    assert!(chunk_count <= 4, "chunk budget exceeded: {chunk_count}");
    assert!(index.skips.iter().any(|skip| {
        skip.path.display() == "src/pathological.rs"
            && skip.reason == ContextIndexSkipReason::ChunkBudgetExceeded
    }));
}

/// Fixture for the R7 degradation ladder: a small changed file plus a
/// large related file whose single full chunk dwarfs the pack budget.
/// `chunk_max_tokens` is raised so the large file stays one chunk,
/// making "full content does not fit" deterministic.
async fn skeleton_fixture() -> (SnapshotContextEngine, Arc<RepoSnapshot>, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn changed_fn() {\n    let a = 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/big.rs"),
        many_function_rust_file(40, 18),
    )
    .unwrap();
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2,1 +2,1 @@\n+    let a = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/lib.rs"], diff);
    let mut config = ContextEngineConfig::snapshot_v0();
    config.chunk_max_tokens = 16_000;
    let engine = SnapshotContextEngine::new(config);
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (engine, snapshot, repo)
}

fn assert_pack_budget_and_dedup_invariants(pack: &ContextPack) {
    let total: usize = pack
        .evidence
        .iter()
        .map(|evidence| evidence.token_estimate)
        .sum();
    assert_eq!(
        pack.budget.used_tokens, total,
        "budget usage equals the sum of included content estimates"
    );
    assert!(pack.budget.used_tokens <= pack.budget.max_tokens);
    for skeleton in pack
        .evidence
        .iter()
        .filter(|evidence| evidence.representation == ContextEvidenceRepresentation::Skeleton)
    {
        assert!(skeleton.skeleton_text.is_some());
        let skeleton_range = skeleton.range.expect("skeleton evidence carries a range");
        assert!(
            !pack.evidence.iter().any(|other| {
                other.representation == ContextEvidenceRepresentation::FullContent
                    && other.kind != ContextEvidenceKind::Symbol
                    && other.path == skeleton.path
                    && other.range.is_some_and(|range| {
                        range.start_line <= skeleton_range.end_line
                            && skeleton_range.start_line <= range.end_line
                    })
            }),
            "a chunk and its skeleton are never both included"
        );
    }
}

#[tokio::test]
async fn large_related_file_enters_budget_constrained_pack_as_skeleton() {
    let (engine, snapshot, _repo) = skeleton_fixture().await;
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let big_path = crate::runtime::contracts::RepoPath::parse("src/big.rs").unwrap();
    let big_chunk = index
        .evidence
        .iter()
        .filter(|evidence| {
            evidence.path.as_ref() == Some(&big_path)
                && evidence.kind == ContextEvidenceKind::FileSpan
        })
        .min_by_key(|evidence| evidence.token_estimate)
        .expect("big.rs chunk evidence");
    let skeleton = index
        .skeletons
        .get(&big_chunk.id.0)
        .expect("skeleton twin for the src/big.rs chunk");
    let big_chunk_tokens = big_chunk.token_estimate;
    let other_tokens: usize = index
        .evidence
        .iter()
        .filter(|evidence| evidence.path.as_ref() != Some(&big_path))
        .map(|evidence| evidence.token_estimate)
        .sum();
    let max_tokens = other_tokens + skeleton.token_estimate;
    assert!(
        max_tokens < big_chunk_tokens,
        "fixture invariant: the full chunk must not fit the budget"
    );

    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let included_skeleton = pack
        .evidence
        .iter()
        .find(|evidence| evidence.representation == ContextEvidenceRepresentation::Skeleton)
        .expect("large related file enters the pack as a skeleton");
    assert_eq!(included_skeleton.path.as_ref(), Some(&big_path));
    let text = included_skeleton.skeleton_text.as_deref().unwrap();
    assert!(text.contains("    1| "), "line numbers preserved: {text}");
    assert!(text.contains("     | ..."), "bodies elided to ...");
    assert!(
        text.contains("pub fn generated_0()"),
        "signature text preserved for coverage checks"
    );
    assert!(
        pack.omitted_candidates.iter().any(|candidate| {
            candidate.path.as_ref() == Some(&big_path)
                && candidate.reason == ContextOmissionReason::DowngradedToSkeleton
        }),
        "the downgrade is recorded in omission data"
    );
    assert_pack_budget_and_dedup_invariants(&pack);
}

#[tokio::test]
async fn full_content_suppresses_skeleton_when_budget_allows() {
    let (engine, snapshot, _repo) = skeleton_fixture().await;
    let pack = engine
        .build_pack(
            ContextPackRequest {
                run_id: None,
                snapshot_id: snapshot.snapshot_id.clone(),
                session_id: None,
                purpose: ContextPackPurpose::Correctness,
                max_tokens: 50_000,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let big_path = crate::runtime::contracts::RepoPath::parse("src/big.rs").unwrap();
    assert!(pack.evidence.iter().any(|evidence| {
        evidence.path.as_ref() == Some(&big_path)
            && evidence.representation == ContextEvidenceRepresentation::FullContent
            && evidence.kind == ContextEvidenceKind::FileSpan
    }));
    assert!(!pack
        .evidence
        .iter()
        .any(|evidence| evidence.representation == ContextEvidenceRepresentation::Skeleton));
    assert!(!pack
        .omitted_candidates
        .iter()
        .any(|candidate| candidate.reason == ContextOmissionReason::DowngradedToSkeleton));
    assert_pack_budget_and_dedup_invariants(&pack);
}

#[tokio::test]
async fn skeleton_evidence_does_not_satisfy_hunk_coverage() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/big.rs"),
        many_function_rust_file(40, 12),
    )
    .unwrap();
    let diff = "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -2,3 +2,4 @@\n+    let added = 1;\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/big.rs"], diff);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let big_path = crate::runtime::contracts::RepoPath::parse("src/big.rs").unwrap();
    let changed_chunk = index
        .evidence
        .iter()
        .find(|evidence| {
            evidence.kind == ContextEvidenceKind::FileSpan
                && evidence.is_changed_span
                && evidence.path.as_ref() == Some(&big_path)
        })
        .expect("enclosing chunk evidence");
    let skeleton = index
        .skeletons
        .get(&changed_chunk.id.0)
        .expect("skeleton twin for the changed chunk");

    // Bodies are elided, so a skeleton cannot stand in for the enclosing
    // definition of a hunk.
    let check = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                vec![skeleton.id.clone()],
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        check.evidence.iter().any(|item| item.id == skeleton.id),
        "the sufficiency check resolves skeleton evidence ids"
    );
    let sufficiency = check.sufficiency.unwrap();
    assert!(sufficiency.gaps.iter().any(|gap| {
        gap.missing
            .contains(&ContextCoverageGapKind::EnclosingDefinition)
    }));

    // The full chunk enclosing the hunk clears the gap.
    let recheck = engine
        .query(
            sufficiency_query(
                &snapshot,
                ContextQueryKind::SufficiencyCheck,
                serde_json::json!({}),
                vec![changed_chunk.id.clone()],
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!recheck.sufficiency.unwrap().gaps.iter().any(|gap| {
        gap.missing
            .contains(&ContextCoverageGapKind::EnclosingDefinition)
    }));
}

#[tokio::test]
async fn skeleton_text_passes_redaction() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    let secret = "ghp_1234567890abcdefghijklmnopqrst";
    let content = format!(
        "pub const TOKEN: &str = \"{secret}\";\n\n{}",
        many_function_rust_file(20, 12)
    );
    std::fs::write(repo.path().join("src/secret.rs"), content).unwrap();
    let snapshot = build_snapshot(repo.path(), vec!["src/secret.rs"]);
    let index = ContextIndex::build(ContextIndexRequest::for_snapshot(
        snapshot,
        &ContextEngineConfig::snapshot_v0(),
    ))
    .await
    .unwrap();
    assert!(!index.skeletons.is_empty(), "skeleton twins exist");
    for skeleton in index.skeletons.values() {
        let text = skeleton.skeleton_text.as_deref().unwrap();
        assert!(!text.contains(secret), "secret retained in skeleton view");
    }
    assert!(
        index.skeletons.values().any(|skeleton| skeleton
            .skeleton_text
            .as_deref()
            .unwrap()
            .contains("[REDACTED]")),
        "the retained signature line carries the redaction marker"
    );
}

// ---- R9: incremental, persistent indexing ----

fn derived_cache_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("CONTEXT.md"), "# Context\n").unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/auth.rs"),
        many_function_rust_file(12, 8),
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/api.rs"),
        "use crate::auth;\n\npub fn handle() {\n    auth::generated_0();\n}\n",
    )
    .unwrap();
    repo
}

async fn index_with_cache(
    repo: &std::path::Path,
    cache_path: &std::path::Path,
) -> (SnapshotContextEngine, Arc<RepoSnapshot>, ContextIndexReport) {
    let snapshot = build_snapshot(repo, vec!["src/api.rs"]);
    let engine = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0())
        .with_derived_cache_file(cache_path);
    let report = engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (engine, snapshot, report)
}

#[tokio::test]
async fn warm_reindex_recomputes_nothing_and_reproduces_the_index() {
    let repo = derived_cache_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("derived.json");

    let (cold_engine, cold_snapshot, cold) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(cold.derived_cache_hits, 0);
    assert!(
        cold.derived_cache_misses > 0,
        "cold build derives all files"
    );

    // A fresh engine process over the same checkout: every file's derived
    // data comes from the durable cache, and the index is reproduced
    // exactly.
    let (warm_engine, warm_snapshot, warm) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(warm.derived_cache_misses, 0, "warm build derives nothing");
    assert_eq!(warm.derived_cache_hits, cold.derived_cache_misses);
    assert_eq!(warm.index_id, cold.index_id);

    let cold_index = cold_engine.get_index(&cold_snapshot.snapshot_id).unwrap();
    let warm_index = warm_engine.get_index(&warm_snapshot.snapshot_id).unwrap();
    assert_eq!(warm_index.manifest_artifact, cold_index.manifest_artifact);
    assert_eq!(warm_index.evidence, cold_index.evidence);
    assert_eq!(warm_index.skeletons, cold_index.skeletons);
}

#[tokio::test]
async fn one_file_change_recomputes_only_that_file() {
    let repo = derived_cache_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("derived.json");

    let (_, _, cold) = index_with_cache(repo.path(), &cache_path).await;
    std::fs::write(
        repo.path().join("src/api.rs"),
        "use crate::auth;\n\npub fn handle() {\n    auth::generated_1();\n}\n",
    )
    .unwrap();

    let (_, _, warm) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(warm.derived_cache_misses, 1, "only the changed file pays");
    assert_eq!(warm.derived_cache_hits, cold.derived_cache_misses - 1);
}

#[tokio::test]
async fn derivation_version_bump_invalidates_and_rebuilds() {
    let repo = derived_cache_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("derived.json");

    let (_, _, cold) = index_with_cache(repo.path(), &cache_path).await;
    let stale = std::fs::read_to_string(&cache_path)
        .unwrap()
        .replace(&derived_version_tag(), "0.0.0/stale");
    std::fs::write(&cache_path, stale).unwrap();

    let (_, _, warm) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(
        warm.derived_cache_hits, 0,
        "stale-version entries never hit"
    );
    assert_eq!(warm.derived_cache_misses, cold.derived_cache_misses);
    assert!(
        !warm
            .warnings
            .iter()
            .any(|warning| warning.code == "derived_cache_recovered"),
        "a version mismatch is clean invalidation, not corruption"
    );
}

#[tokio::test]
async fn corrupt_derived_cache_degrades_to_full_rebuild_with_warning() {
    let repo = derived_cache_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("derived.json");
    std::fs::write(&cache_path, "{definitely not json").unwrap();

    let (engine, snapshot, report) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(report.derived_cache_hits, 0);
    assert!(report.derived_cache_misses > 0, "full rebuild");
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.code == "derived_cache_recovered"));
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    assert!(index
        .manifest_artifact
        .warnings
        .iter()
        .any(|warning| warning.code == "derived_cache_recovered"));

    // The flush replaced the corrupt file: the next build runs warm.
    let (_, _, warm) = index_with_cache(repo.path(), &cache_path).await;
    assert_eq!(warm.derived_cache_misses, 0);
}

#[tokio::test]
async fn warm_reindex_spends_nothing_on_embeddings() {
    let repo = derived_cache_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("derived.json");
    let mut config = ContextEngineConfig::snapshot_v0();
    config.semantic.mode = ContextSemanticMode::Local;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Local);
    config.semantic.max_embedding_inputs = 64;

    let snapshot = build_snapshot(repo.path(), vec!["src/api.rs"]);
    let cold_engine =
        SnapshotContextEngine::new(config.clone()).with_derived_cache_file(&cache_path);
    let cold = cold_engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), cold_engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(cold.embeddings_computed > 0);
    assert_eq!(cold.embeddings_cached, 0);

    let warm_engine = SnapshotContextEngine::new(config).with_derived_cache_file(&cache_path);
    let warm = warm_engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), warm_engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(warm.embeddings_computed, 0, "warm embed work is zero");
    assert_eq!(warm.embeddings_cached, cold.embeddings_computed);
    let index = warm_engine.get_index(&snapshot.snapshot_id).unwrap();
    assert!(index.semantic_vectors.is_some());
}

// ---- R8: real embeddings and optional reranking ----

/// Loopback provider serving the OpenAI-compatible `/embeddings` and
/// Cohere-style `/rerank` contracts. Embedding vectors are a
/// deterministic hash of the input text; rerank scores reverse the
/// offered document order so reordering is observable. Every request
/// body is captured for policy assertions.
struct LoopbackContextProviderServer {
    base_url: String,
    requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

impl LoopbackContextProviderServer {
    fn spawn() -> Self {
        use crate::tests::support::{http_content_length, read_http_request, split_http_body};
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let request_bytes = read_http_request(&mut stream);
                let (headers, body) = split_http_body(&request_bytes);
                let content_length = http_content_length(headers);
                let path = headers
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                let request: serde_json::Value =
                    serde_json::from_slice(&body[..content_length]).unwrap();
                let response_body = if path.ends_with("/rerank") {
                    let documents = request["documents"].as_array().unwrap();
                    // Reverse the offered order: the last document gets
                    // the highest relevance score.
                    let results = documents
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            serde_json::json!({
                                "index": index,
                                "relevance_score": index as f32,
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::json!({ "results": results })
                } else {
                    let inputs = request["input"].as_array().unwrap();
                    let data = inputs
                        .iter()
                        .enumerate()
                        .map(|(index, input)| {
                            let text = input.as_str().unwrap_or_default();
                            let seed = text
                                .bytes()
                                .fold(0u32, |hash, byte| hash.wrapping_mul(31) + u32::from(byte));
                            let embedding = (0..4)
                                .map(|dim| ((seed >> (dim * 8)) & 0xff) as f32 + 1.0)
                                .collect::<Vec<_>>();
                            serde_json::json!({ "index": index, "embedding": embedding })
                        })
                        .collect::<Vec<_>>();
                    serde_json::json!({ "data": data })
                };
                captured.lock().unwrap().push((path, request));
                let payload = serde_json::to_vec(&response_body).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .unwrap();
                stream.write_all(&payload).unwrap();
            }
        });
        Self { base_url, requests }
    }

    fn requests_for(&self, path_suffix: &str) -> Vec<serde_json::Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.ends_with(path_suffix))
            .map(|(_, request)| request.clone())
            .collect()
    }
}

fn hosted_semantic_test_config(base_url: &str, model: &str) -> ContextEngineConfig {
    std::env::set_var("MUZEN_TEST_CONTEXT_EMBED_KEY", "test-key");
    let mut config = ContextEngineConfig::snapshot_v0();
    config.semantic.mode = ContextSemanticMode::Hosted;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Hosted);
    config.semantic.hosted_base_url = Some(base_url.to_string());
    config.semantic.hosted_model = Some(model.to_string());
    config.semantic.hosted_credential_ref = Some("env:MUZEN_TEST_CONTEXT_EMBED_KEY".to_string());
    config.semantic.max_embedding_inputs = 64;
    config
}

fn rerank_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("alpha.rs"),
        "pub fn authorize_request_alpha() -> bool { true }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("beta.rs"),
        "pub fn authorize_request_beta() -> bool { false }\n",
    )
    .unwrap();
    repo
}

#[tokio::test]
async fn hosted_index_records_embedding_model_provenance() {
    let server = LoopbackContextProviderServer::spawn();
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = hosted_semantic_test_config(&server.base_url, "test-embed-model");
    let engine = SnapshotContextEngine::new(config);
    let report = engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.semantic_provider.as_deref(),
        Some("hosted:test-embed-model")
    );
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    assert_eq!(
        index.manifest_artifact.semantic_provider.as_deref(),
        Some("hosted:test-embed-model")
    );
    assert!(index.semantic_vectors.is_some());
    assert!(!server.requests_for("/embeddings").is_empty());
}

#[tokio::test]
async fn rerank_disabled_output_is_unchanged_from_fusion_output() {
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = ContextEngineConfig::snapshot_v0();
    let engine = SnapshotContextEngine::new(config.clone());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    let baseline = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();

    // A fully configured but disabled rerank stage must not change a bit
    // of the fusion output.
    index.semantic.rerank = ContextRerankConfig {
        enabled: false,
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: Some("test-rerank-model".to_string()),
        credential_ref: None,
        top_n: 50,
    };
    let with_disabled_rerank = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert_eq!(with_disabled_rerank.evidence, baseline.evidence);
    assert!(with_disabled_rerank
        .fusion
        .iter()
        .all(|trace| trace.rerank_rank.is_none() && trace.rerank_score.is_none()));
    assert!(with_disabled_rerank.degraded.is_empty());
}

#[tokio::test]
async fn rerank_reorders_fused_candidates_and_records_ranks() {
    let server = LoopbackContextProviderServer::spawn();
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = ContextEngineConfig::snapshot_v0();
    let engine = SnapshotContextEngine::new(config.clone());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    let baseline = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(
        baseline.evidence.len() >= 2,
        "fixture yields fused candidates"
    );

    index.semantic.rerank = ContextRerankConfig {
        enabled: true,
        base_url: Some(server.base_url.clone()),
        model: Some("test-rerank-model".to_string()),
        credential_ref: None,
        top_n: 50,
    };
    let reranked = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(reranked.degraded.is_empty());
    // The loopback reranker reverses the fused order.
    let baseline_ids: Vec<_> = baseline
        .evidence
        .iter()
        .map(|evidence| evidence.id.clone())
        .collect();
    let reranked_ids: Vec<_> = reranked
        .evidence
        .iter()
        .map(|evidence| evidence.id.clone())
        .collect();
    let mut reversed = baseline_ids.clone();
    reversed.reverse();
    assert_eq!(reranked_ids, reversed);
    for (position, trace) in reranked.fusion.iter().enumerate() {
        assert_eq!(trace.rerank_rank, Some(position + 1));
        assert!(trace.rerank_score.is_some());
    }
    let rerank_requests = server.requests_for("/rerank");
    assert_eq!(rerank_requests.len(), 1);
    assert_eq!(
        rerank_requests[0]["model"].as_str(),
        Some("test-rerank-model")
    );
    assert_eq!(
        rerank_requests[0]["documents"].as_array().unwrap().len(),
        baseline.evidence.len()
    );
}

#[tokio::test]
async fn rerank_request_never_contains_restricted_evidence() {
    let server = LoopbackContextProviderServer::spawn();
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = ContextEngineConfig::snapshot_v0();
    let engine = SnapshotContextEngine::new(config.clone());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    // Every evidence item derived from alpha.rs is restricted: none of
    // its text may reach the reranker through any evidence kind.
    let mut restricted_ids = Vec::new();
    for evidence in &mut index.evidence {
        if evidence
            .path
            .as_ref()
            .is_some_and(|path| path.display() == "alpha.rs")
        {
            evidence.sensitivity = ContextSensitivity::Restricted;
            restricted_ids.push(evidence.id.clone());
        }
    }
    assert!(!restricted_ids.is_empty(), "fixture yields alpha evidence");
    index.semantic.rerank = ContextRerankConfig {
        enabled: true,
        base_url: Some(server.base_url.clone()),
        model: None,
        credential_ref: None,
        top_n: 50,
    };
    let outcome = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(restricted_ids.iter().any(|restricted| outcome
        .omissions
        .iter()
        .any(|omission| omission.evidence_id == restricted.0)));
    for request in server.requests_for("/rerank") {
        let serialized = serde_json::to_string(&request).unwrap();
        assert!(
            !serialized.contains("authorize_request_alpha"),
            "restricted evidence text must never reach the reranker"
        );
    }
}

#[test]
fn rerank_batch_rejects_restricted_without_opt_in() {
    let candidates = vec![RerankCandidate {
        id: "ev_restricted".to_string(),
        text: "restricted".to_string(),
        sensitivity: ContextSensitivity::Restricted,
    }];
    let error = validate_rerank_batch(false, &candidates).unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(message) if message.contains("restricted")));
    validate_rerank_batch(true, &candidates).expect("explicit opt-in allows restricted inputs");
}

#[tokio::test]
async fn embedding_provider_failure_degrades_index_to_lexical_with_warning() {
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    // Connection-refused port: a provider failure, not a config error.
    let config = hosted_semantic_test_config("http://127.0.0.1:9", "test-embed-model");
    let engine = SnapshotContextEngine::new(config.clone());
    let report = engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.code == "semantic_provider_failed"));
    assert_eq!(report.semantic_provider, None);
    assert_eq!(report.embeddings_computed, 0);
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    assert!(index.semantic_vectors.is_none());
    let outcome = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(
        !outcome.evidence.is_empty(),
        "lexical retrieval still answers"
    );
}

#[tokio::test]
async fn query_embedding_failure_degrades_to_lexical_with_record() {
    let server = LoopbackContextProviderServer::spawn();
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = hosted_semantic_test_config(&server.base_url, "test-embed-model");
    let engine = SnapshotContextEngine::new(config.clone());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    assert!(index.semantic_vectors.is_some());
    // The provider goes away between index build and query time.
    index.semantic.hosted_base_url = Some("http://127.0.0.1:9".to_string());
    let outcome = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert!(
        !outcome.evidence.is_empty(),
        "lexical retrieval still answers"
    );
    assert!(outcome
        .degraded
        .iter()
        .any(|degradation| degradation.stage == "semantic"));
}

#[tokio::test]
async fn rerank_provider_failure_degrades_to_fused_order_with_record() {
    let repo = rerank_repo();
    let snapshot = build_snapshot(repo.path(), vec!["alpha.rs"]);
    let config = ContextEngineConfig::snapshot_v0();
    let engine = SnapshotContextEngine::new(config.clone());
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut index = (*engine.get_index(&snapshot.snapshot_id).unwrap()).clone();
    let baseline = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    index.semantic.rerank = ContextRerankConfig {
        enabled: true,
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: None,
        credential_ref: None,
        top_n: 50,
    };
    let outcome = super::retrieval::fused_search(
        &index,
        "authorize_request",
        10,
        config.bm25_k1,
        config.bm25_b,
        config.rrf_k,
    )
    .await
    .unwrap();
    assert_eq!(outcome.evidence, baseline.evidence, "fused order is kept");
    assert!(outcome
        .degraded
        .iter()
        .any(|degradation| degradation.stage == "rerank"));
}

#[tokio::test]
async fn semantic_change_signal_ranks_change_similar_evidence() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/auth.rs"),
        "pub fn authorize_token_session(token: Token, session: Session) -> bool { true }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/auth_like.rs"),
        "pub fn validate_token_session(token: Token, session: Session) -> bool { false }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("src/billing.rs"),
        "pub fn compute_invoice_totals(rate: Decimal) -> Decimal { rate }\n",
    )
    .unwrap();
    // A real hunk makes the auth.rs chunk a change anchor.
    let diff = "diff --git a/src/auth.rs b/src/auth.rs\n--- a/src/auth.rs\n+++ b/src/auth.rs\n@@ -1,1 +1,1 @@\n+pub fn authorize_token_session(token: Token, session: Session) -> bool { true }\n";
    let snapshot = build_snapshot_with_diff(repo.path(), vec!["src/auth.rs"], diff);

    // No-vector mode: the signal does not exist, ranking stays untouched.
    let no_vector = SnapshotContextEngine::new(ContextEngineConfig::snapshot_v0());
    no_vector
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), no_vector.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let no_vector_index = no_vector.get_index(&snapshot.snapshot_id).unwrap();
    assert!(no_vector_index
        .evidence
        .iter()
        .all(|evidence| evidence.signals.semantic_change_score == 0.0));

    let mut config = ContextEngineConfig::snapshot_v0();
    config.semantic.mode = ContextSemanticMode::Local;
    config.semantic.provider = Some(ContextEmbeddingProviderKind::Local);
    config.semantic.max_embedding_inputs = 64;
    let engine = SnapshotContextEngine::new(config);
    engine
        .index_snapshot(
            ContextIndexRequest::for_snapshot(Arc::clone(&snapshot), engine.config_ref()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let index = engine.get_index(&snapshot.snapshot_id).unwrap();
    let score_for = |path: &str| {
        index
            .evidence
            .iter()
            .filter(|evidence| {
                evidence
                    .path
                    .as_ref()
                    .is_some_and(|candidate| candidate.display() == path)
            })
            .map(|evidence| evidence.signals.semantic_change_score)
            .fold(0.0f32, f32::max)
    };
    let similar = score_for("src/auth_like.rs");
    let dissimilar = score_for("src/billing.rs");
    assert!(
        similar > dissimilar,
        "token-overlapping evidence scores above unrelated evidence \
         (similar {similar}, dissimilar {dissimilar})"
    );
    assert!(similar > 0.0);
    // Changed spans are credited by weight_changed_span, not the
    // semantic signal.
    assert_eq!(score_for("src/auth.rs"), 0.0);
}

/// Real-model integration check for the local ONNX tier. Skipped unless
/// `MUZEN_TEST_ONNX_MODEL_DIR` points at a directory with
/// `model(.quantized)?.onnx` + `tokenizer.json`; the bench harness is
/// the always-on quality gate for this provider.
#[tokio::test]
async fn local_onnx_provider_ranks_semantically_similar_code() {
    let Some(model_dir) = std::env::var_os("MUZEN_TEST_ONNX_MODEL_DIR") else {
        return;
    };
    let provider = LocalOnnxEmbeddingProvider::shared(std::path::Path::new(&model_dir)).unwrap();
    let input = |id: &str, text: &str| EmbeddingInput {
        id: id.to_string(),
        text: text.to_string(),
        sensitivity: ContextSensitivity::Restricted,
    };
    let vectors = provider
        .embed(vec![
            input(
                "auth",
                "fn authorize_token(token: &str) -> bool { validate_session_token(token) }",
            ),
            input(
                "billing",
                "fn compute_invoice_total(items: &[LineItem]) -> Decimal { items.iter().map(|i| i.amount).sum() }",
            ),
        ])
        .await
        .unwrap();
    let query = provider
        .embed(vec![input("query", "session token authorization check")])
        .await
        .unwrap()
        .remove(0);
    let mut index = InMemoryVectorIndex::new();
    index.put("auth".to_string(), vectors[0].clone()).unwrap();
    index
        .put("billing".to_string(), vectors[1].clone())
        .unwrap();
    let results = index.search(&query, 2).unwrap();
    assert_eq!(
        results[0].0, "auth",
        "code-tuned embeddings rank the semantically related function first"
    );
    assert!(results[0].1 > results[1].1);
}
