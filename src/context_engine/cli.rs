use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::context_engine::{
    explain_selected_evidence, ContextEmbeddingProviderKind, ContextEngine, ContextEngineConfig,
    ContextGraphDebugExport, ContextIndex, ContextIndexRequest, ContextPackPurpose,
    ContextPackRequest, ContextQuery, ContextQueryKind, ContextQueryLimits, ContextSemanticMode,
    SnapshotContextEngine,
};
use crate::reviewer_kernel::review_contract::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode,
};
use crate::workspace::RepoSnapshot;

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextArgs {
    #[command(subcommand)]
    pub(crate) command: ContextCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ContextCommand {
    /// Index a local snapshot and print context_manifest JSON.
    Index(ContextSnapshotArgs),
    /// Build a role/purpose-specific context pack for a local snapshot.
    Pack(ContextPackArgs),
    /// Query indexed context evidence for a local snapshot.
    Query(ContextQueryArgs),
    /// Explain why evidence was included or omitted in a context pack JSON file.
    Explain(ContextExplainArgs),
    /// Export bounded Context Graph debug JSON for a local snapshot.
    GraphDebug(ContextGraphDebugArgs),
    /// Run multiple context pack/query eval cases against one indexed snapshot.
    EvalBatch(ContextEvalBatchArgs),
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextSnapshotArgs {
    #[arg(long, default_value = ".")]
    pub(crate) repo: PathBuf,

    #[arg(long = "changed-file", required = true)]
    pub(crate) changed_files: Vec<PathBuf>,

    /// Unified diff for the change under review. Hunks anchor changed-span
    /// detection, graph expansion, and sufficiency coverage.
    #[arg(long = "diff-file")]
    pub(crate) diff_file: Option<PathBuf>,

    #[arg(long, default_value_t = 200)]
    pub(crate) max_file_kb: usize,

    #[arg(long, default_value_t = 120)]
    pub(crate) max_search_matches: usize,

    #[arg(long)]
    pub(crate) local_semantic: bool,

    /// Local transformer embeddings via ONNX Runtime; requires
    /// --onnx-model-dir.
    #[arg(long)]
    pub(crate) local_onnx_semantic: bool,

    /// Directory holding model.onnx/model_quantized.onnx and
    /// tokenizer.json for --local-onnx-semantic.
    #[arg(long)]
    pub(crate) onnx_model_dir: Option<PathBuf>,

    #[arg(long)]
    pub(crate) hosted_semantic: bool,

    #[arg(long)]
    pub(crate) hosted_embedding_base_url: Option<String>,

    #[arg(long)]
    pub(crate) hosted_embedding_model: Option<String>,

    #[arg(long)]
    pub(crate) hosted_embedding_credential_ref: Option<String>,

    #[arg(long, default_value_t = 512)]
    pub(crate) max_embedding_inputs: usize,

    /// Enable the cross-encoder rerank stage over the fused top
    /// candidates (R8). Requires --rerank-base-url.
    #[arg(long)]
    pub(crate) rerank: bool,

    /// Cohere-style /rerank endpoint (Cohere, Jina, or an in-house
    /// vLLM/Infinity server).
    #[arg(long)]
    pub(crate) rerank_base_url: Option<String>,

    #[arg(long)]
    pub(crate) rerank_model: Option<String>,

    /// Credential reference (env:NAME). Omit for unauthenticated
    /// in-house rerankers.
    #[arg(long)]
    pub(crate) rerank_credential_ref: Option<String>,

    /// Fused candidates sent to the reranker.
    #[arg(long, default_value_t = 50)]
    pub(crate) rerank_top_n: usize,

    /// Disable one context signal for evaluation/debugging. Repeatable.
    #[arg(long = "ablate-context-signal", value_enum)]
    pub(crate) ablate_context_signals: Vec<ContextSignalAblationArg>,

    /// Directory for the durable derived-data cache (R9). Re-indexing
    /// an unchanged repo recomputes nothing; only changed files pay.
    #[arg(long)]
    pub(crate) derived_cache_root: Option<PathBuf>,

    /// JSON object of trusted host metadata (PR body, issue text, CI
    /// failure, external contract summary) to index as run-scoped context.
    #[arg(long)]
    pub(crate) host_metadata_json: Option<PathBuf>,

    /// JSON array of SessionInstruction objects to index as run-scoped
    /// host context.
    #[arg(long)]
    pub(crate) host_instruction_json: Option<PathBuf>,

    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextPackArgs {
    #[command(flatten)]
    pub(crate) snapshot: ContextSnapshotArgs,

    #[arg(long, value_enum, default_value_t = ContextPurposeArg::GeneralReview)]
    pub(crate) purpose: ContextPurposeArg,

    #[arg(long, default_value_t = 12_000)]
    pub(crate) max_tokens: usize,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextGraphDebugArgs {
    #[command(flatten)]
    pub(crate) snapshot: ContextSnapshotArgs,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextEvalBatchArgs {
    /// Schema-versioned muzen.context-eval-batch.v1 JSON.
    #[arg(long)]
    pub(crate) cases: PathBuf,

    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextQueryArgs {
    #[command(flatten)]
    pub(crate) snapshot: ContextSnapshotArgs,

    #[arg(long, value_enum, default_value_t = ContextQueryKindArg::SearchText)]
    pub(crate) kind: ContextQueryKindArg,

    #[arg(long)]
    pub(crate) query: Option<String>,

    #[arg(long)]
    pub(crate) path: Option<String>,

    #[arg(long)]
    pub(crate) start_line: Option<usize>,

    #[arg(long)]
    pub(crate) end_line: Option<usize>,

    #[arg(long, default_value_t = 20)]
    pub(crate) max_results: usize,
}

const CONTEXT_EVAL_BATCH_SCHEMA_VERSION: &str = "muzen.context-eval-batch.v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchInput {
    schema_version: String,
    snapshot: ContextEvalBatchSnapshotInput,
    cases: Vec<ContextEvalBatchCaseInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchSnapshotInput {
    repo: PathBuf,
    changed_files: Vec<PathBuf>,
    #[serde(default)]
    diff_file: Option<PathBuf>,
    #[serde(default = "default_context_max_file_kb")]
    max_file_kb: usize,
    #[serde(default = "default_context_max_search_matches")]
    max_search_matches: usize,
    #[serde(default)]
    local_semantic: bool,
    #[serde(default)]
    local_onnx_semantic: bool,
    #[serde(default)]
    onnx_model_dir: Option<PathBuf>,
    #[serde(default)]
    hosted_semantic: bool,
    #[serde(default)]
    hosted_embedding_base_url: Option<String>,
    #[serde(default)]
    hosted_embedding_model: Option<String>,
    #[serde(default)]
    hosted_embedding_credential_ref: Option<String>,
    #[serde(default = "default_context_max_embedding_inputs")]
    max_embedding_inputs: usize,
    #[serde(default)]
    rerank: bool,
    #[serde(default)]
    rerank_base_url: Option<String>,
    #[serde(default)]
    rerank_model: Option<String>,
    #[serde(default)]
    rerank_credential_ref: Option<String>,
    #[serde(default = "default_context_rerank_top_n")]
    rerank_top_n: usize,
    #[serde(default)]
    ablate_context_signals: Vec<String>,
    #[serde(default)]
    derived_cache_root: Option<PathBuf>,
    #[serde(default)]
    host_metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    host_instructions: Vec<crate::reviewer_kernel::kernel_types::SessionInstruction>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchCaseInput {
    id: String,
    command: ContextEvalBatchCaseCommand,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    include_graph_debug: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ContextEvalBatchCaseCommand {
    Pack,
    Query,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchOutput {
    schema_version: String,
    cases: Vec<ContextEvalBatchCaseOutput>,
    performance: ContextEvalBatchPerformance,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchCaseOutput {
    id: String,
    command: ContextEvalBatchCaseCommandOutput,
    result: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_debug: Option<ContextGraphDebugExport>,
    performance: ContextCliTimings,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ContextEvalBatchCaseCommandOutput {
    Pack,
    Query,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextEvalBatchPerformance {
    snapshot_build_ms: f64,
    index_build_ms: f64,
    case_action_ms: f64,
    graph_debug_ms: f64,
    output_serialization_ms: f64,
}

fn default_context_max_file_kb() -> usize {
    200
}

fn default_context_max_search_matches() -> usize {
    120
}

fn default_context_max_embedding_inputs() -> usize {
    512
}

fn default_context_rerank_top_n() -> usize {
    50
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ContextExplainArgs {
    #[arg(long)]
    pub(crate) pack: PathBuf,

    #[arg(long, default_value_t = true)]
    pub(crate) include_omitted: bool,

    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
pub(crate) enum ContextPurposeArg {
    GeneralReview,
    Correctness,
    Security,
    Tests,
    Architecture,
    Performance,
    Validator,
}

impl From<ContextPurposeArg> for ContextPackPurpose {
    fn from(value: ContextPurposeArg) -> Self {
        match value {
            ContextPurposeArg::GeneralReview => Self::GeneralReview,
            ContextPurposeArg::Correctness => Self::Correctness,
            ContextPurposeArg::Security => Self::Security,
            ContextPurposeArg::Tests => Self::Tests,
            ContextPurposeArg::Architecture => Self::Architecture,
            ContextPurposeArg::Performance => Self::Performance,
            ContextPurposeArg::Validator => Self::Validator,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
pub(crate) enum ContextQueryKindArg {
    SearchText,
    ReadSpan,
    RelatedTests,
    RelatedSymbols,
    TicketRequirements,
    HistorySimilar,
    CrossRepoContracts,
    SufficiencyCheck,
}

impl From<ContextQueryKindArg> for ContextQueryKind {
    fn from(value: ContextQueryKindArg) -> Self {
        match value {
            ContextQueryKindArg::SearchText => Self::SearchText,
            ContextQueryKindArg::ReadSpan => Self::ReadSpan,
            ContextQueryKindArg::RelatedTests => Self::RelatedTests,
            ContextQueryKindArg::RelatedSymbols => Self::RelatedSymbols,
            ContextQueryKindArg::TicketRequirements => Self::TicketRequirements,
            ContextQueryKindArg::HistorySimilar => Self::HistorySimilar,
            ContextQueryKindArg::CrossRepoContracts => Self::CrossRepoContracts,
            ContextQueryKindArg::SufficiencyCheck => Self::SufficiencyCheck,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
pub(crate) enum ContextSignalAblationArg {
    Graph,
    CoChange,
    PathProximity,
    LexicalChange,
    TestCoverage,
    SemanticChange,
    PackRepair,
    PackPathDiversity,
    SkeletonReserve,
    RankDiversity,
    TokenEfficiency,
}

pub(crate) fn run_context(args: ContextArgs) -> Result<i32> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for context engine")?;
    runtime.block_on(async move {
        match args.command {
            ContextCommand::Index(args) => {
                let (_engine, _snapshot, _index, manifest, mut timings) =
                    index_context_snapshot(&args).await?;
                timings.action_ms = 0.0;
                write_context_output_with_performance(args.output.as_ref(), &manifest, timings)?;
                Ok(0)
            }
            ContextCommand::Pack(args) => {
                let (engine, snapshot, _index, _manifest, mut timings) =
                    index_context_snapshot(&args.snapshot).await?;
                let action_started = Instant::now();
                let pack = engine
                    .build_pack(
                        ContextPackRequest {
                            run_id: None,
                            snapshot_id: snapshot.snapshot_id.clone(),
                            session_id: None,
                            purpose: args.purpose.into(),
                            max_tokens: args.max_tokens,
                        },
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                timings.action_ms = elapsed_ms(action_started);
                write_context_output_with_performance(args.snapshot.output.as_ref(), &pack, timings)?;
                Ok(0)
            }
            ContextCommand::GraphDebug(args) => {
                let (_engine, _snapshot, index, _manifest, mut timings) =
                    index_context_snapshot(&args.snapshot).await?;
                let action_started = Instant::now();
                let export =
                    ContextGraphDebugExport::collect(&index.graph, &index.graph_expansion);
                timings.action_ms = elapsed_ms(action_started);
                write_context_output_with_performance(
                    args.snapshot.output.as_ref(),
                    &export,
                    timings,
                )?;
                Ok(0)
            }
            ContextCommand::Query(args) => {
                let (engine, snapshot, _index, _manifest, mut timings) =
                    index_context_snapshot(&args.snapshot).await?;
                let arguments = match args.kind {
                    ContextQueryKindArg::SearchText => {
                        serde_json::json!({"query": args.query.unwrap_or_default()})
                    }
                    ContextQueryKindArg::ReadSpan => {
                        serde_json::json!({
                            "path": args.path.unwrap_or_default(),
                            "startLine": args.start_line.unwrap_or(1),
                            "endLine": args.end_line.unwrap_or(args.start_line.unwrap_or(1)),
                        })
                    }
                    ContextQueryKindArg::RelatedTests => {
                        serde_json::json!({"path": args.path.unwrap_or_default()})
                    }
                    ContextQueryKindArg::RelatedSymbols => {
                        serde_json::json!({"path": args.path.unwrap_or_default()})
                    }
                    ContextQueryKindArg::TicketRequirements => {
                        serde_json::json!({"query": args.query.unwrap_or_default()})
                    }
                    ContextQueryKindArg::HistorySimilar => {
                        serde_json::json!({"query": args.query.unwrap_or_default()})
                    }
                    ContextQueryKindArg::CrossRepoContracts => {
                        serde_json::json!({"query": args.query.unwrap_or_default()})
                    }
                    ContextQueryKindArg::SufficiencyCheck => {
                        serde_json::json!({"question": args.query.unwrap_or_default()})
                    }
                };
                let action_started = Instant::now();
                let result = engine
                    .query(
                        ContextQuery {
                            run_id: None,
                            snapshot_id: snapshot.snapshot_id.clone(),
                            session_id: None,
                            purpose: Some(ContextPackPurpose::StandaloneQuery),
                            kind: args.kind.into(),
                            arguments,
                            current_evidence: Vec::new(),
                            limits: ContextQueryLimits {
                                max_results: args.max_results,
                                max_tokens: ContextEngineConfig::snapshot_v0().max_pack_tokens,
                            },
                        },
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                timings.action_ms = elapsed_ms(action_started);
                write_context_output_with_performance(
                    args.snapshot.output.as_ref(),
                    &result,
                    timings,
                )?;
                Ok(0)
            }
            ContextCommand::Explain(args) => {
                let pack =
                    read_context_json_file::<crate::context_engine::ContextPack>(&args.pack)?;
                let explanation = serde_json::json!({
                    "packId": pack.id.0,
                    "purpose": pack.purpose,
                    "included": pack.evidence.iter().map(|evidence| {
                        let selected_candidate = pack
                            .selected_candidates
                            .iter()
                            .find(|candidate| candidate.evidence_id == evidence.id);
                        serde_json::json!({
                            "evidenceId": evidence.id.0,
                            "kind": evidence.kind,
                            "path": evidence.path.as_ref().map(|path| path.display()),
                            "score": selected_candidate.map(|candidate| candidate.score),
                            "rankIndex": selected_candidate.map(|candidate| candidate.rank_index),
                            "tokenEstimate": evidence.token_estimate,
                            "why": explain_selected_evidence(evidence, pack.purpose),
                            "graphPaths": pack.relationships
                                .iter()
                                .filter(|relationship| {
                                    relationship.from == evidence.id || relationship.to == evidence.id
                                })
                                .map(|relationship| {
                                    serde_json::json!({
                                        "kind": relationship.kind,
                                        "confidence": relationship.confidence,
                                        "path": relationship.reason,
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                    }).collect::<Vec<_>>(),
                    "omitted": if args.include_omitted {
                        serde_json::to_value(&pack.omitted_candidates)?
                    } else {
                        serde_json::json!([])
                    },
                    "sufficiency": pack.sufficiency,
                });
                write_context_output(args.output.as_ref(), &explanation)?;
                Ok(0)
            }
            ContextCommand::EvalBatch(args) => {
                let output = run_context_eval_batch(&args).await?;
                write_context_eval_batch_output(args.output.as_ref(), output)?;
                Ok(0)
            }
        }
    })
}

async fn index_context_snapshot(
    args: &ContextSnapshotArgs,
) -> Result<(
    SnapshotContextEngine,
    Arc<RepoSnapshot>,
    Arc<ContextIndex>,
    crate::context_engine::ContextManifestArtifact,
    ContextCliTimings,
)> {
    index_context_snapshot_with_host(args, None, None).await
}

async fn index_context_snapshot_with_host(
    args: &ContextSnapshotArgs,
    host_metadata: Option<BTreeMap<String, serde_json::Value>>,
    host_instructions: Option<Vec<crate::reviewer_kernel::kernel_types::SessionInstruction>>,
) -> Result<(
    SnapshotContextEngine,
    Arc<RepoSnapshot>,
    Arc<ContextIndex>,
    crate::context_engine::ContextManifestArtifact,
    ContextCliTimings,
)> {
    let snapshot_started = Instant::now();
    let snapshot = build_context_snapshot(args)?;
    let snapshot_build_ms = elapsed_ms(snapshot_started);
    let mut engine = SnapshotContextEngine::new(context_engine_config(args)?);
    if let Some(root) = &args.derived_cache_root {
        engine = engine.with_derived_cache_file(root.join("context-derived-cache.json"));
    }
    let index_started = Instant::now();
    engine
        .index_snapshot(
            context_index_request(
                &snapshot,
                engine.config_ref(),
                args,
                host_metadata,
                host_instructions,
            )?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let index_build_ms = elapsed_ms(index_started);
    let index = engine
        .get_index(&snapshot.snapshot_id)
        .ok_or_else(|| anyhow::anyhow!("context index was not stored"))?;
    let timings = ContextCliTimings {
        snapshot_build_ms,
        index_build_ms,
        ..ContextCliTimings::default()
    };
    Ok((
        engine,
        snapshot,
        Arc::clone(&index),
        index.manifest_artifact.clone(),
        timings,
    ))
}

fn context_index_request(
    snapshot: &Arc<RepoSnapshot>,
    config: &ContextEngineConfig,
    args: &ContextSnapshotArgs,
    host_metadata: Option<BTreeMap<String, serde_json::Value>>,
    host_instructions: Option<Vec<crate::reviewer_kernel::kernel_types::SessionInstruction>>,
) -> Result<ContextIndexRequest> {
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(snapshot), config);
    if let Some(metadata) = host_metadata {
        request.host_metadata = metadata;
        request.include_host_context = true;
    } else if let Some(path) = &args.host_metadata_json {
        request.host_metadata =
            read_context_json_file::<BTreeMap<String, serde_json::Value>>(path)?;
        request.include_host_context = true;
    }
    if let Some(instructions) = host_instructions {
        request.instructions = instructions;
        request.include_host_context = true;
    } else if let Some(path) = &args.host_instruction_json {
        request.instructions = read_context_json_file::<
            Vec<crate::reviewer_kernel::kernel_types::SessionInstruction>,
        >(path)?;
        request.include_host_context = true;
    }
    Ok(request)
}

async fn run_context_eval_batch(args: &ContextEvalBatchArgs) -> Result<ContextEvalBatchOutput> {
    let input = read_context_json_file::<ContextEvalBatchInput>(&args.cases)?;
    if input.schema_version != CONTEXT_EVAL_BATCH_SCHEMA_VERSION {
        bail!(
            "{}: expected schemaVersion {CONTEXT_EVAL_BATCH_SCHEMA_VERSION}, got {:?}",
            args.cases.display(),
            input.schema_version
        );
    }
    if input.cases.is_empty() {
        bail!(
            "{}: eval-batch cases must not be empty",
            args.cases.display()
        );
    }
    let snapshot_args = context_eval_batch_snapshot_args(&input.snapshot)?;
    let host_metadata = non_empty_host_metadata(&input.snapshot.host_metadata);
    let host_instructions = non_empty_host_instructions(&input.snapshot.host_instructions);
    let (engine, snapshot, index, _manifest, shared_timings) =
        index_context_snapshot_with_host(&snapshot_args, host_metadata, host_instructions).await?;
    let graph_export_needed = input.cases.iter().any(|case| case.include_graph_debug);
    let graph_debug = if graph_export_needed {
        Some(ContextGraphDebugExport::collect(
            &index.graph,
            &index.graph_expansion,
        ))
    } else {
        None
    };
    let mut outputs = Vec::with_capacity(input.cases.len());
    let mut performance = ContextEvalBatchPerformance {
        snapshot_build_ms: shared_timings.snapshot_build_ms,
        index_build_ms: shared_timings.index_build_ms,
        ..ContextEvalBatchPerformance::default()
    };
    for case in input.cases {
        let action_started = Instant::now();
        let (command, mut result) = run_context_eval_batch_case(&engine, &snapshot, &case).await?;
        let action_ms = elapsed_ms(action_started);
        performance.case_action_ms += action_ms;
        let graph_started = Instant::now();
        let case_graph_debug = if case.include_graph_debug {
            graph_debug.clone()
        } else {
            None
        };
        performance.graph_debug_ms += elapsed_ms(graph_started);
        let case_timings = ContextCliTimings {
            snapshot_build_ms: shared_timings.snapshot_build_ms,
            index_build_ms: shared_timings.index_build_ms,
            action_ms,
            output_serialization_ms: 0.0,
        };
        if let serde_json::Value::Object(object) = &mut result {
            object.insert(
                "performance".to_string(),
                serde_json::to_value(case_timings)?,
            );
        }
        outputs.push(ContextEvalBatchCaseOutput {
            id: case.id,
            command,
            result,
            graph_debug: case_graph_debug,
            performance: case_timings,
        });
    }
    Ok(ContextEvalBatchOutput {
        schema_version: CONTEXT_EVAL_BATCH_SCHEMA_VERSION.to_string(),
        cases: outputs,
        performance,
    })
}

async fn run_context_eval_batch_case(
    engine: &SnapshotContextEngine,
    snapshot: &Arc<RepoSnapshot>,
    case: &ContextEvalBatchCaseInput,
) -> Result<(ContextEvalBatchCaseCommandOutput, serde_json::Value)> {
    match case.command {
        ContextEvalBatchCaseCommand::Pack => {
            let purpose =
                parse_context_pack_purpose(case.purpose.as_deref().unwrap_or("general-review"))?;
            let pack = engine
                .build_pack(
                    ContextPackRequest {
                        run_id: None,
                        snapshot_id: snapshot.snapshot_id.clone(),
                        session_id: None,
                        purpose,
                        max_tokens: case.max_tokens.unwrap_or(12_000),
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok((
                ContextEvalBatchCaseCommandOutput::Pack,
                serde_json::to_value(pack)?,
            ))
        }
        ContextEvalBatchCaseCommand::Query => {
            let kind = parse_context_query_kind(
                case.kind
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("eval-batch case {} missing kind", case.id))?,
            )?;
            let arguments = context_eval_batch_query_arguments(kind, case);
            let result = engine
                .query(
                    ContextQuery {
                        run_id: None,
                        snapshot_id: snapshot.snapshot_id.clone(),
                        session_id: None,
                        purpose: Some(ContextPackPurpose::StandaloneQuery),
                        kind,
                        arguments,
                        current_evidence: Vec::new(),
                        limits: ContextQueryLimits {
                            max_results: case.max_results.unwrap_or(20),
                            max_tokens: ContextEngineConfig::snapshot_v0().max_pack_tokens,
                        },
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok((
                ContextEvalBatchCaseCommandOutput::Query,
                serde_json::to_value(result)?,
            ))
        }
    }
}

fn context_eval_batch_snapshot_args(
    input: &ContextEvalBatchSnapshotInput,
) -> Result<ContextSnapshotArgs> {
    Ok(ContextSnapshotArgs {
        repo: input.repo.clone(),
        changed_files: input.changed_files.clone(),
        diff_file: input.diff_file.clone(),
        max_file_kb: input.max_file_kb,
        max_search_matches: input.max_search_matches,
        local_semantic: input.local_semantic,
        local_onnx_semantic: input.local_onnx_semantic,
        onnx_model_dir: input.onnx_model_dir.clone(),
        hosted_semantic: input.hosted_semantic,
        hosted_embedding_base_url: input.hosted_embedding_base_url.clone(),
        hosted_embedding_model: input.hosted_embedding_model.clone(),
        hosted_embedding_credential_ref: input.hosted_embedding_credential_ref.clone(),
        max_embedding_inputs: input.max_embedding_inputs,
        rerank: input.rerank,
        rerank_base_url: input.rerank_base_url.clone(),
        rerank_model: input.rerank_model.clone(),
        rerank_credential_ref: input.rerank_credential_ref.clone(),
        rerank_top_n: input.rerank_top_n,
        ablate_context_signals: input
            .ablate_context_signals
            .iter()
            .map(|signal| parse_context_signal_ablation(signal))
            .collect::<Result<Vec<_>>>()?,
        derived_cache_root: input.derived_cache_root.clone(),
        host_metadata_json: None,
        host_instruction_json: None,
        output: None,
    })
}

fn non_empty_host_metadata(
    value: &BTreeMap<String, serde_json::Value>,
) -> Option<BTreeMap<String, serde_json::Value>> {
    if value.is_empty() {
        None
    } else {
        Some(value.clone())
    }
}

fn non_empty_host_instructions(
    value: &[crate::reviewer_kernel::kernel_types::SessionInstruction],
) -> Option<Vec<crate::reviewer_kernel::kernel_types::SessionInstruction>> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_vec())
    }
}

fn context_eval_batch_query_arguments(
    kind: ContextQueryKind,
    case: &ContextEvalBatchCaseInput,
) -> serde_json::Value {
    match kind {
        ContextQueryKind::SearchText => {
            serde_json::json!({"query": case.query.clone().unwrap_or_default()})
        }
        ContextQueryKind::ReadSpan => serde_json::json!({
            "path": case.path.clone().unwrap_or_default(),
            "startLine": case.start_line.unwrap_or(1),
            "endLine": case.end_line.unwrap_or(case.start_line.unwrap_or(1)),
        }),
        ContextQueryKind::RelatedTests | ContextQueryKind::RelatedSymbols => {
            serde_json::json!({"path": case.path.clone().unwrap_or_default()})
        }
        ContextQueryKind::TicketRequirements
        | ContextQueryKind::HistorySimilar
        | ContextQueryKind::CrossRepoContracts => {
            serde_json::json!({"query": case.query.clone().unwrap_or_default()})
        }
        ContextQueryKind::SufficiencyCheck => {
            serde_json::json!({"question": case.query.clone().unwrap_or_default()})
        }
        ContextQueryKind::ExplainPack => serde_json::json!({}),
    }
}

fn parse_context_pack_purpose(value: &str) -> Result<ContextPackPurpose> {
    match normalized_context_name(value).as_str() {
        "generalreview" => Ok(ContextPackPurpose::GeneralReview),
        "correctness" => Ok(ContextPackPurpose::Correctness),
        "security" => Ok(ContextPackPurpose::Security),
        "tests" => Ok(ContextPackPurpose::Tests),
        "architecture" => Ok(ContextPackPurpose::Architecture),
        "performance" => Ok(ContextPackPurpose::Performance),
        "validator" => Ok(ContextPackPurpose::Validator),
        "standalonequery" => Ok(ContextPackPurpose::StandaloneQuery),
        _ => bail!("unsupported context pack purpose {value:?}"),
    }
}

fn parse_context_query_kind(value: &str) -> Result<ContextQueryKind> {
    match normalized_context_name(value).as_str() {
        "searchtext" => Ok(ContextQueryKind::SearchText),
        "readspan" => Ok(ContextQueryKind::ReadSpan),
        "explainpack" => Ok(ContextQueryKind::ExplainPack),
        "relatedtests" => Ok(ContextQueryKind::RelatedTests),
        "relatedsymbols" => Ok(ContextQueryKind::RelatedSymbols),
        "ticketrequirements" => Ok(ContextQueryKind::TicketRequirements),
        "historysimilar" => Ok(ContextQueryKind::HistorySimilar),
        "crossrepocontracts" => Ok(ContextQueryKind::CrossRepoContracts),
        "sufficiencycheck" => Ok(ContextQueryKind::SufficiencyCheck),
        _ => bail!("unsupported context query kind {value:?}"),
    }
}

fn parse_context_signal_ablation(value: &str) -> Result<ContextSignalAblationArg> {
    match normalized_context_name(value).as_str() {
        "graph" => Ok(ContextSignalAblationArg::Graph),
        "cochange" => Ok(ContextSignalAblationArg::CoChange),
        "pathproximity" => Ok(ContextSignalAblationArg::PathProximity),
        "lexicalchange" => Ok(ContextSignalAblationArg::LexicalChange),
        "testcoverage" => Ok(ContextSignalAblationArg::TestCoverage),
        "semanticchange" => Ok(ContextSignalAblationArg::SemanticChange),
        "packrepair" => Ok(ContextSignalAblationArg::PackRepair),
        "packpathdiversity" => Ok(ContextSignalAblationArg::PackPathDiversity),
        "skeletonreserve" => Ok(ContextSignalAblationArg::SkeletonReserve),
        "rankdiversity" => Ok(ContextSignalAblationArg::RankDiversity),
        "tokenefficiency" => Ok(ContextSignalAblationArg::TokenEfficiency),
        _ => bail!("unsupported context signal ablation {value:?}"),
    }
}

fn normalized_context_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn context_engine_config(args: &ContextSnapshotArgs) -> Result<ContextEngineConfig> {
    if [
        args.local_semantic,
        args.local_onnx_semantic,
        args.hosted_semantic,
    ]
    .iter()
    .filter(|enabled| **enabled)
    .count()
        > 1
    {
        bail!(
            "--local-semantic, --local-onnx-semantic, and --hosted-semantic are mutually exclusive"
        );
    }
    let mut config = ContextEngineConfig::snapshot_v0();
    if args.local_semantic {
        config.semantic.mode = ContextSemanticMode::Local;
        config.semantic.provider = Some(ContextEmbeddingProviderKind::Local);
        config.semantic.max_embedding_inputs = args.max_embedding_inputs;
    } else if args.local_onnx_semantic {
        let model_dir = args
            .onnx_model_dir
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--local-onnx-semantic requires --onnx-model-dir"))?;
        config.semantic.mode = ContextSemanticMode::LocalOnnx;
        config.semantic.provider = Some(ContextEmbeddingProviderKind::LocalOnnx);
        config.semantic.local_onnx_model_dir = Some(model_dir.display().to_string());
        config.semantic.max_embedding_inputs = args.max_embedding_inputs;
    } else if args.hosted_semantic {
        config.semantic.mode = ContextSemanticMode::Hosted;
        config.semantic.provider = Some(ContextEmbeddingProviderKind::Hosted);
        config.semantic.hosted_base_url = args.hosted_embedding_base_url.clone();
        config.semantic.hosted_model = args.hosted_embedding_model.clone();
        config.semantic.hosted_credential_ref = args.hosted_embedding_credential_ref.clone();
        config.semantic.max_embedding_inputs = args.max_embedding_inputs;
    }
    if args.rerank {
        if args.rerank_base_url.is_none() {
            bail!("--rerank requires --rerank-base-url");
        }
        config.semantic.rerank.enabled = true;
        config.semantic.rerank.base_url = args.rerank_base_url.clone();
        config.semantic.rerank.model = args.rerank_model.clone();
        config.semantic.rerank.credential_ref = args.rerank_credential_ref.clone();
        config.semantic.rerank.top_n = args.rerank_top_n;
    }
    for ablation in &args.ablate_context_signals {
        apply_context_signal_ablation(&mut config, *ablation);
    }
    Ok(config)
}

fn apply_context_signal_ablation(
    config: &mut ContextEngineConfig,
    signal: ContextSignalAblationArg,
) {
    match signal {
        ContextSignalAblationArg::Graph => {
            config.graph_max_hops = 0;
            config.graph_max_candidates_per_anchor = 0;
            config.weight_graph_proximity = 0.0;
        }
        ContextSignalAblationArg::CoChange => {
            config.co_change_commit_limit = 0;
            config.weight_co_change = 0.0;
        }
        ContextSignalAblationArg::PathProximity => {
            config.weight_path_proximity = 0.0;
        }
        ContextSignalAblationArg::LexicalChange => {
            config.weight_lexical_change = 0.0;
        }
        ContextSignalAblationArg::TestCoverage => {
            config.weight_test_coverage = 0.0;
        }
        ContextSignalAblationArg::SemanticChange => {
            config.weight_semantic_change = 0.0;
        }
        ContextSignalAblationArg::PackRepair => {
            config.enable_pack_repair = false;
        }
        ContextSignalAblationArg::PackPathDiversity => {
            config.enable_pack_path_diversity = false;
        }
        ContextSignalAblationArg::SkeletonReserve => {
            config.enable_skeleton_reserve = false;
        }
        ContextSignalAblationArg::RankDiversity => {
            config.enable_rank_diversity = false;
        }
        ContextSignalAblationArg::TokenEfficiency => {
            config.enable_token_efficiency_bonus = false;
        }
    }
}

fn build_context_snapshot(args: &ContextSnapshotArgs) -> Result<Arc<RepoSnapshot>> {
    let inline_diff = args
        .diff_file
        .as_ref()
        .map(|path| {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read diff file {}", path.display()))
        })
        .transpose()?;
    let changed_files = args
        .changed_files
        .iter()
        .map(|path| ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        })
        .collect::<Vec<_>>();
    let change = ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "context-local".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff,
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files,
    };
    RepoSnapshot::build_with_storage(
        &args.repo,
        &PathPolicyV1::bench(args.max_file_kb, args.max_search_matches),
        &change,
        crate::reviewer_kernel::kernel_types::SnapshotStoragePolicy::default(),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextCliTimings {
    snapshot_build_ms: f64,
    index_build_ms: f64,
    action_ms: f64,
    output_serialization_ms: f64,
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn write_context_output<T: Serialize>(output: Option<&PathBuf>, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{json}");
    }
    Ok(())
}

fn write_context_output_with_performance<T: Serialize>(
    output: Option<&PathBuf>,
    value: &T,
    mut timings: ContextCliTimings,
) -> Result<()> {
    let mut value = serde_json::to_value(value)?;
    if let serde_json::Value::Object(object) = &mut value {
        object.insert("performance".to_string(), serde_json::to_value(timings)?);
    }
    let serialization_started = Instant::now();
    let _json_for_timing = serde_json::to_string_pretty(&value)?;
    timings.output_serialization_ms = elapsed_ms(serialization_started);
    if let serde_json::Value::Object(object) = &mut value {
        object.insert("performance".to_string(), serde_json::to_value(timings)?);
    }
    let json = serde_json::to_string_pretty(&value)?;
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{json}");
    }
    Ok(())
}

fn write_context_eval_batch_output(
    output: Option<&PathBuf>,
    mut value: ContextEvalBatchOutput,
) -> Result<()> {
    let serialization_started = Instant::now();
    let _json_for_timing = serde_json::to_string_pretty(&value)?;
    value.performance.output_serialization_ms = elapsed_ms(serialization_started);
    let json = serde_json::to_string_pretty(&value)?;
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{json}");
    }
    Ok(())
}

fn read_context_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("invalid JSON in {}", path.display()))
}

#[cfg(test)]
mod context_cli_tests {
    use super::*;
    use crate::cli::{Cli, Command};

    #[test]
    fn eval_batch_empty_host_context_is_absent() {
        let metadata = BTreeMap::new();
        let instructions = Vec::new();

        assert!(non_empty_host_metadata(&metadata).is_none());
        assert!(non_empty_host_instructions(&instructions).is_none());
    }

    #[test]
    fn eval_batch_non_empty_host_metadata_is_preserved() {
        let metadata = BTreeMap::from([("ticket".to_string(), serde_json::json!("T-123"))]);

        assert_eq!(non_empty_host_metadata(&metadata), Some(metadata));
    }

    #[test]
    fn context_signal_ablation_flags_adjust_snapshot_config() {
        let cli = Cli::try_parse_from([
            "muzen",
            "context",
            "pack",
            "--changed-file",
            "src/lib.rs",
            "--ablate-context-signal",
            "graph",
            "--ablate-context-signal",
            "co-change",
            "--ablate-context-signal",
            "pack-repair",
            "--ablate-context-signal",
            "pack-path-diversity",
            "--ablate-context-signal",
            "skeleton-reserve",
            "--ablate-context-signal",
            "rank-diversity",
            "--ablate-context-signal",
            "token-efficiency",
        ])
        .expect("valid context command");
        let Command::Context(context) = cli.command else {
            panic!("expected context command");
        };
        let ContextCommand::Pack(pack) = context.command else {
            panic!("expected context pack command");
        };

        let config = context_engine_config(&pack.snapshot).expect("context config");

        assert_eq!(config.graph_max_hops, 0);
        assert_eq!(config.graph_max_candidates_per_anchor, 0);
        assert_eq!(config.weight_graph_proximity, 0.0);
        assert_eq!(config.co_change_commit_limit, 0);
        assert_eq!(config.weight_co_change, 0.0);
        assert!(!config.enable_pack_repair);
        assert!(!config.enable_pack_path_diversity);
        assert!(!config.enable_skeleton_reserve);
        assert!(!config.enable_rank_diversity);
        assert!(!config.enable_token_efficiency_bonus);
        assert_eq!(
            config.weight_path_proximity,
            ContextEngineConfig::snapshot_v0().weight_path_proximity
        );
    }

    #[test]
    fn context_graph_debug_command_uses_snapshot_args() {
        let cli = Cli::try_parse_from([
            "muzen",
            "context",
            "graph-debug",
            "--changed-file",
            "src/lib.rs",
            "--output",
            "graph-debug.json",
        ])
        .expect("valid graph debug command");
        let Command::Context(context) = cli.command else {
            panic!("expected context command");
        };
        let ContextCommand::GraphDebug(debug) = context.command else {
            panic!("expected context graph-debug command");
        };

        assert_eq!(
            debug.snapshot.changed_files,
            vec![PathBuf::from("src/lib.rs")]
        );
        assert_eq!(
            debug.snapshot.output.as_deref(),
            Some(Path::new("graph-debug.json"))
        );
    }

    #[test]
    fn context_signal_ablation_zeroes_only_requested_weight() {
        let mut config = ContextEngineConfig::snapshot_v0();

        apply_context_signal_ablation(&mut config, ContextSignalAblationArg::LexicalChange);

        assert_eq!(config.weight_lexical_change, 0.0);
        assert_eq!(
            config.weight_test_coverage,
            ContextEngineConfig::snapshot_v0().weight_test_coverage
        );
        assert!(config.enable_pack_repair);
        assert!(config.enable_pack_path_diversity);
        assert!(config.enable_skeleton_reserve);
        assert!(config.enable_rank_diversity);
        assert!(config.enable_token_efficiency_bonus);
    }
}
