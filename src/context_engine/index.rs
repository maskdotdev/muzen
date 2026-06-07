use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::review_plan::ReviewPlan;
use crate::runtime::contracts::{
    stable_id, ArtifactId, EvidenceId, RepoPath, RuntimeError, RuntimeResult, SessionInstruction,
    SnapshotCaptureStatus, SnapshotId,
};
use crate::runtime::repo::{FileMeta, RepoSnapshot};

use super::{
    context_embedding_text, validate_embedding_batch, ContextEngineConfig, ContextEvidence,
    ContextEvidenceKind, ContextEvidenceSource, ContextIndexId, ContextProvenance, ContextRevision,
    ContextScope, ContextSemanticConfig, ContextSemanticMode, ContextSensitivity,
    ContextSymbolGraph, ContextTrust, EmbeddingInput, EmbeddingProvider, HostedEmbeddingProvider,
    InMemoryVectorIndex, LocalHashEmbeddingProvider, VectorIndex,
};

pub const CONTEXT_ENGINE_VERSION: &str = "0.1.0";
pub const CONTEXT_MANIFEST_SCHEMA_VERSION: &str = "muzen.context_manifest.v1";

#[derive(Debug, Clone)]
pub struct ContextIndexRequest {
    pub run_id: Option<String>,
    pub(crate) snapshot: Arc<RepoSnapshot>,
    pub(crate) review_plan: Option<ReviewPlan>,
    pub instructions: Vec<SessionInstruction>,
    pub host_metadata: BTreeMap<String, serde_json::Value>,
    pub cross_repo_contracts: Vec<CrossRepoContractCandidate>,
    pub allowed_cross_repo_resources: BTreeSet<String>,
    pub include_host_context: bool,
    pub semantic: ContextSemanticConfig,
    pub limits: ContextLimits,
}

impl ContextIndexRequest {
    pub(crate) fn for_snapshot(snapshot: Arc<RepoSnapshot>, config: &ContextEngineConfig) -> Self {
        Self {
            run_id: None,
            snapshot,
            review_plan: None,
            instructions: Vec::new(),
            host_metadata: BTreeMap::new(),
            cross_repo_contracts: Vec::new(),
            allowed_cross_repo_resources: BTreeSet::new(),
            include_host_context: config.include_host_context,
            semantic: config.semantic.clone(),
            limits: ContextLimits::from_config(config),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CrossRepoContractCandidate {
    pub resource_id: String,
    pub repository: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextLimits {
    pub max_indexed_files: usize,
    pub max_indexed_bytes: usize,
    pub max_evidence_items: usize,
    pub max_pack_tokens: usize,
    pub max_query_results: usize,
}

impl ContextLimits {
    pub fn from_config(config: &ContextEngineConfig) -> Self {
        Self {
            max_indexed_files: config.max_indexed_files,
            max_indexed_bytes: config.max_indexed_bytes,
            max_evidence_items: config.max_evidence_items,
            max_pack_tokens: config.max_pack_tokens,
            max_query_results: config.max_query_results,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexReport {
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub context_engine_version: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub indexed_bytes: u64,
    pub indexed_changed_files: usize,
    pub rule_count: usize,
    pub diff_hunk_count: usize,
    pub evidence_count: usize,
    pub elapsed_ms: u64,
    pub warnings: Vec<ContextIndexWarning>,
    pub artifacts: Vec<ArtifactId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexWarning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RepoPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextManifestArtifact {
    pub schema_version: String,
    pub context_engine_version: String,
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub path_policy_hash: String,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub indexed_bytes: u64,
    pub evidence_count: usize,
    pub rule_count: usize,
    pub diff_hunk_count: usize,
    pub skips: Vec<ContextIndexSkip>,
    pub warnings: Vec<ContextIndexWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexSkip {
    pub path: RepoPath,
    pub reason: ContextIndexSkipReason,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextIndexSkipReason {
    BinaryFile,
    Unavailable,
    BudgetExceeded,
}

#[derive(Debug, Clone)]
pub struct ContextIndex {
    pub index_id: ContextIndexId,
    pub snapshot_id: SnapshotId,
    pub manifest_hash: String,
    pub evidence: Vec<ContextEvidence>,
    pub file_contents: BTreeMap<RepoPath, String>,
    pub symbol_graph: ContextSymbolGraph,
    pub semantic: ContextSemanticConfig,
    pub semantic_vectors: Option<InMemoryVectorIndex>,
    pub denied_cross_repo_contracts: usize,
    pub skips: Vec<ContextIndexSkip>,
    pub report: ContextIndexReport,
    pub manifest_artifact: ContextManifestArtifact,
}

impl ContextIndex {
    pub async fn build(request: ContextIndexRequest) -> RuntimeResult<Self> {
        let started = std::time::Instant::now();
        let _review_plan = &request.review_plan;
        let snapshot = request.snapshot;
        let mut evidence = Vec::new();
        let mut file_contents = BTreeMap::new();
        let mut symbol_graph = ContextSymbolGraph::default();
        let mut denied_cross_repo_contracts = 0usize;
        let mut skips = Vec::new();
        let mut indexed_files = 0usize;
        let mut indexed_bytes = 0u64;
        let mut indexed_changed_files = 0usize;
        let mut rule_count = 0usize;
        let diff_hunk_count = count_diff_hunks(&snapshot.diff.content);

        for file in &snapshot.manifest.files {
            if indexed_files >= request.limits.max_indexed_files
                || indexed_bytes as usize >= request.limits.max_indexed_bytes
                || evidence.len() >= request.limits.max_evidence_items
            {
                skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::BudgetExceeded,
                });
                continue;
            }
            match file.capture_status {
                SnapshotCaptureStatus::Captured => {
                    indexed_files += 1;
                    indexed_bytes = indexed_bytes.saturating_add(file.size);
                    if file.is_changed {
                        indexed_changed_files += 1;
                    }
                    let kind = evidence_kind_for_file(file);
                    if kind == ContextEvidenceKind::RepositoryRule {
                        rule_count += 1;
                    }
                    evidence.push(file_evidence(&snapshot, file, kind));
                    if let Ok((bytes, _truncated)) =
                        snapshot.read_bounded(file.file_id, request.limits.max_indexed_bytes)
                    {
                        if let Ok(content) = String::from_utf8(bytes) {
                            let parsed_symbols =
                                symbol_graph.add_file(file.rel_path.clone(), &content);
                            if file.is_changed {
                                for symbol in parsed_symbols.definitions.iter().take(
                                    request
                                        .limits
                                        .max_evidence_items
                                        .saturating_sub(evidence.len()),
                                ) {
                                    evidence.push(symbol_evidence(
                                        &snapshot,
                                        file,
                                        symbol,
                                        parsed_symbols.definition_ranges.get(symbol).copied(),
                                    ));
                                    if evidence.len() >= request.limits.max_evidence_items {
                                        break;
                                    }
                                }
                            }
                            file_contents.insert(file.rel_path.clone(), content);
                        }
                    }
                }
                SnapshotCaptureStatus::NotTextCandidate => skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::BinaryFile,
                }),
                SnapshotCaptureStatus::SkippedMemoryLimit
                | SnapshotCaptureStatus::SkippedUnreadable => skips.push(ContextIndexSkip {
                    path: file.rel_path.clone(),
                    reason: ContextIndexSkipReason::Unavailable,
                }),
            }
        }

        if !snapshot.diff.content.is_empty() && evidence.len() < request.limits.max_evidence_items {
            evidence.push(ContextEvidence {
                id: EvidenceId(stable_id(&[
                    &snapshot.snapshot_id.0,
                    "diff",
                    &snapshot.diff.content_hash,
                ])),
                kind: ContextEvidenceKind::Diff,
                source: ContextEvidenceSource::Snapshot,
                trust: ContextTrust::Kernel,
                sensitivity: ContextSensitivity::Private,
                scope: ContextScope::Snapshot,
                path: None,
                revision: None,
                range: None,
                content_hash: Some(snapshot.diff.content_hash.clone()),
                summary: Some("Review diff manifest".to_string()),
                token_estimate: estimate_tokens(snapshot.diff.content.len()),
                provenance: ContextProvenance {
                    provider: "snapshot_diff".to_string(),
                    query: None,
                    tool_call_id: None,
                    snapshot_id: Some(snapshot.snapshot_id.0.clone()),
                    original_url: None,
                },
                created_at_utc: None,
                expires_at_utc: None,
            });
        }

        if request.include_host_context && evidence.len() < request.limits.max_evidence_items {
            for instruction in &request.instructions {
                if evidence.len() >= request.limits.max_evidence_items {
                    break;
                }
                if let Some(item) =
                    host_instruction_evidence(&snapshot, instruction, request.run_id.as_deref())
                {
                    evidence.push(item);
                }
            }
            for (key, value) in &request.host_metadata {
                if evidence.len() >= request.limits.max_evidence_items {
                    break;
                }
                if let Some(item) =
                    host_metadata_evidence(&snapshot, key, value, request.run_id.as_deref())
                {
                    evidence.push(item);
                }
            }
        }
        for candidate in &request.cross_repo_contracts {
            if evidence.len() >= request.limits.max_evidence_items {
                break;
            }
            if request
                .allowed_cross_repo_resources
                .contains(&candidate.resource_id)
            {
                evidence.push(cross_repo_contract_evidence(
                    &snapshot,
                    candidate,
                    request.run_id.as_deref(),
                ));
            } else {
                denied_cross_repo_contracts = denied_cross_repo_contracts.saturating_add(1);
            }
        }

        let semantic_vectors =
            build_semantic_vectors(&request.semantic, &evidence, &file_contents).await?;

        let index_id = ContextIndexId(stable_id(&[
            &snapshot.snapshot_id.0,
            &snapshot.manifest_hash,
            CONTEXT_ENGINE_VERSION,
            &evidence.len().to_string(),
        ]));
        let warnings = Vec::new();
        let report = ContextIndexReport {
            index_id: index_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            context_engine_version: CONTEXT_ENGINE_VERSION.to_string(),
            indexed_files,
            skipped_files: skips.len(),
            indexed_bytes,
            indexed_changed_files,
            rule_count,
            diff_hunk_count,
            evidence_count: evidence.len(),
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            warnings: warnings.clone(),
            artifacts: Vec::new(),
        };
        let manifest_artifact = ContextManifestArtifact {
            schema_version: CONTEXT_MANIFEST_SCHEMA_VERSION.to_string(),
            context_engine_version: CONTEXT_ENGINE_VERSION.to_string(),
            index_id: index_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            path_policy_hash: snapshot.path_policy_hash.clone(),
            indexed_files,
            skipped_files: skips.len(),
            indexed_bytes,
            evidence_count: evidence.len(),
            rule_count,
            diff_hunk_count,
            skips: skips.clone(),
            warnings,
        };
        Ok(Self {
            index_id,
            snapshot_id: snapshot.snapshot_id.clone(),
            manifest_hash: snapshot.manifest_hash.clone(),
            evidence,
            file_contents,
            symbol_graph,
            semantic: request.semantic,
            semantic_vectors,
            denied_cross_repo_contracts,
            skips,
            report,
            manifest_artifact,
        })
    }
}

async fn build_semantic_vectors(
    semantic: &ContextSemanticConfig,
    evidence: &[ContextEvidence],
    file_contents: &BTreeMap<RepoPath, String>,
) -> RuntimeResult<Option<InMemoryVectorIndex>> {
    if semantic.mode == ContextSemanticMode::NoVector {
        return Ok(None);
    }
    let max_inputs = semantic.max_embedding_inputs.min(evidence.len());
    let inputs = evidence
        .iter()
        .take(max_inputs)
        .map(|evidence| {
            let content = evidence
                .path
                .as_ref()
                .and_then(|path| file_contents.get(path))
                .map(String::as_str);
            EmbeddingInput {
                id: evidence.id.0.clone(),
                text: context_embedding_text(evidence, content),
                sensitivity: evidence.sensitivity,
            }
        })
        .collect::<Vec<_>>();
    validate_embedding_batch(
        &ContextEngineConfig {
            semantic: semantic.clone(),
            ..ContextEngineConfig::snapshot_v0()
        },
        &inputs,
    )?;
    let vectors = match semantic.mode {
        ContextSemanticMode::NoVector => Vec::new(),
        ContextSemanticMode::Local => {
            let provider = LocalHashEmbeddingProvider::new(256)?;
            provider.embed(inputs.clone()).await?
        }
        ContextSemanticMode::Hosted => {
            let provider = HostedEmbeddingProvider::from_config(semantic)?;
            provider.embed(inputs.clone()).await?
        }
    };
    let mut index = InMemoryVectorIndex::new();
    if vectors.len() != inputs.len() {
        return Err(RuntimeError::ProviderMessage {
            status: None,
            retryable: false,
            message: "context embedding provider returned an unexpected vector count".to_string(),
        });
    }
    for (input, vector) in inputs.into_iter().zip(vectors) {
        index.put(input.id, vector)?;
    }
    Ok(Some(index))
}

fn cross_repo_contract_evidence(
    snapshot: &RepoSnapshot,
    candidate: &CrossRepoContractCandidate,
    run_id: Option<&str>,
) -> ContextEvidence {
    let text = format!("{}: {}", candidate.repository, candidate.summary);
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "cross_repo_contract",
            &candidate.resource_id,
            &text,
            run_id.unwrap_or("standalone"),
        ])),
        kind: ContextEvidenceKind::CrossRepoContract,
        source: ContextEvidenceSource::External,
        trust: ContextTrust::ToolProvider,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::External,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[&text])),
        summary: Some(format!(
            "cross-repo contract {}: {}",
            candidate.repository,
            concise_text(&candidate.summary)
        )),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "cross_repo_provider".to_string(),
            query: Some(candidate.resource_id.clone()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: candidate.original_url.clone(),
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn host_instruction_evidence(
    snapshot: &RepoSnapshot,
    instruction: &SessionInstruction,
    run_id: Option<&str>,
) -> Option<ContextEvidence> {
    let text = instruction.text.trim();
    if text.is_empty() {
        return None;
    }
    let kind = host_instruction_kind(&instruction.kind);
    Some(ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "host_instruction",
            &instruction.kind,
            text,
            run_id.unwrap_or("standalone"),
        ])),
        kind,
        source: ContextEvidenceSource::Host,
        trust: if instruction.trusted {
            ContextTrust::HostTrusted
        } else {
            ContextTrust::UserUntrusted
        },
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Run,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[text])),
        summary: Some(format!("host {}: {}", instruction.kind, concise_text(text))),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "host_instruction".to_string(),
            query: Some(instruction.kind.clone()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    })
}

fn host_metadata_evidence(
    snapshot: &RepoSnapshot,
    key: &str,
    value: &serde_json::Value,
    run_id: Option<&str>,
) -> Option<ContextEvidence> {
    let text = match value {
        serde_json::Value::String(value) => value.trim().to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => value.to_string(),
        serde_json::Value::Null => String::new(),
    };
    if text.is_empty() {
        return None;
    }
    Some(ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "host_metadata",
            key,
            &text,
            run_id.unwrap_or("standalone"),
        ])),
        kind: host_metadata_kind(key),
        source: ContextEvidenceSource::Host,
        trust: ContextTrust::HostTrusted,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Run,
        path: None,
        revision: None,
        range: None,
        content_hash: Some(stable_id(&[&text])),
        summary: Some(format!("host metadata {key}: {}", concise_text(&text))),
        token_estimate: estimate_tokens(text.len()),
        provenance: ContextProvenance {
            provider: "host_metadata".to_string(),
            query: Some(key.to_string()),
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    })
}

fn host_instruction_kind(kind: &str) -> ContextEvidenceKind {
    let lower = kind.to_ascii_lowercase();
    if lower.contains("ticket")
        || lower.contains("issue")
        || lower.contains("acceptance")
        || lower.contains("requirement")
        || lower.contains("scope")
    {
        ContextEvidenceKind::Ticket
    } else {
        ContextEvidenceKind::OrganizationRule
    }
}

fn host_metadata_kind(key: &str) -> ContextEvidenceKind {
    let lower = key.to_ascii_lowercase();
    if lower.contains("cross_repo")
        || lower.contains("crossrepo")
        || lower.contains("linked_repo")
        || lower.contains("linkedrepo")
        || lower.contains("contract")
        || lower.contains("consumer")
    {
        ContextEvidenceKind::CrossRepoContract
    } else if lower.contains("ticket")
        || lower.contains("issue")
        || lower.contains("acceptance")
        || lower.contains("requirement")
        || lower.contains("label")
        || lower.contains("release")
    {
        ContextEvidenceKind::Ticket
    } else {
        ContextEvidenceKind::OrganizationRule
    }
}

fn concise_text(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut output = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.len() > MAX_CHARS {
        output.truncate(MAX_CHARS);
        output.push_str("...");
    }
    output
}

fn symbol_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    symbol: &str,
    range: Option<super::ContextRange>,
) -> ContextEvidence {
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "symbol",
            &file.rel_path.display(),
            symbol,
        ])),
        kind: ContextEvidenceKind::Symbol,
        source: ContextEvidenceSource::Snapshot,
        trust: ContextTrust::Kernel,
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range,
        content_hash: file.content_hash.clone(),
        summary: Some(format!("symbol {symbol} in {}", file.rel_path.display())),
        token_estimate: estimate_tokens(symbol.len()),
        provenance: ContextProvenance {
            provider: "snapshot_symbol_graph_v1".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn file_evidence(
    snapshot: &RepoSnapshot,
    file: &FileMeta,
    kind: ContextEvidenceKind,
) -> ContextEvidence {
    ContextEvidence {
        id: EvidenceId(stable_id(&[
            &snapshot.snapshot_id.0,
            "file",
            &file.rel_path.display(),
            file.content_hash.as_deref().unwrap_or(""),
        ])),
        kind,
        source: ContextEvidenceSource::Snapshot,
        trust: if kind == ContextEvidenceKind::RepositoryRule {
            ContextTrust::RepositoryUntrusted
        } else {
            ContextTrust::Kernel
        },
        sensitivity: ContextSensitivity::Private,
        scope: ContextScope::Snapshot,
        path: Some(file.rel_path.clone()),
        revision: Some(ContextRevision::head()),
        range: None,
        content_hash: file.content_hash.clone(),
        summary: Some(file_summary(file, kind)),
        token_estimate: estimate_tokens(file.size as usize),
        provenance: ContextProvenance {
            provider: "repo_snapshot".to_string(),
            query: None,
            tool_call_id: None,
            snapshot_id: Some(snapshot.snapshot_id.0.clone()),
            original_url: None,
        },
        created_at_utc: None,
        expires_at_utc: None,
    }
}

fn evidence_kind_for_file(file: &FileMeta) -> ContextEvidenceKind {
    let path = file.rel_path.display();
    let lower = path.to_ascii_lowercase();
    if is_repository_guidance(&path) {
        ContextEvidenceKind::RepositoryRule
    } else if lower.contains("test") || lower.contains("spec") {
        ContextEvidenceKind::Test
    } else if is_config_path(&lower) {
        ContextEvidenceKind::Config
    } else if lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".rst") {
        ContextEvidenceKind::Doc
    } else {
        ContextEvidenceKind::FileSpan
    }
}

fn file_summary(file: &FileMeta, kind: ContextEvidenceKind) -> String {
    let changed = if file.is_changed {
        "changed"
    } else {
        "unchanged"
    };
    format!("{changed} {:?} file {}", kind, file.rel_path.display())
}

fn is_repository_guidance(path: &str) -> bool {
    path == "CONTEXT.md"
        || path == "AGENTS.md"
        || path.ends_with("/AGENTS.md")
        || path == ".cursorrules"
        || path == ".github/copilot-instructions.md"
}

fn is_config_path(lower: &str) -> bool {
    lower.ends_with("cargo.toml")
        || lower.ends_with("package.json")
        || lower.ends_with("tsconfig.json")
        || lower.ends_with("pyproject.toml")
        || lower.ends_with(".yml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".json")
        || lower.ends_with(".toml")
}

fn count_diff_hunks(diff: &str) -> usize {
    diff.lines().filter(|line| line.starts_with("@@")).count()
}

pub(crate) fn estimate_tokens(bytes_or_chars: usize) -> usize {
    bytes_or_chars.div_ceil(4).max(1)
}
