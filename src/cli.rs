use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::bench::bench_job;
use crate::context_engine::{
    explain_selected_evidence, ContextEmbeddingProviderKind, ContextEngine, ContextEngineConfig,
    ContextIndexRequest, ContextPackPurpose, ContextPackRequest, ContextQuery, ContextQueryKind,
    ContextQueryLimits, ContextSemanticMode, SnapshotContextEngine,
};
use crate::contracts::*;
use crate::reviewer::artifacts::InMemoryRemoteArtifactObjectClient;
use crate::reviewer::canaries::{
    export_canary_evidence_manifest, export_model_provider_canary_evidence,
    export_remote_object_store_canary_evidence, load_canary_evidence_manifest,
    load_model_provider_canary_evidence, load_remote_object_store_canary_evidence,
    run_openai_provider_canaries, run_remote_artifact_object_store_canary,
    run_remote_snapshot_object_store_canary, CanaryEvidenceFreshnessPolicy, CanaryEvidenceManifest,
    EnvCredentialResolver, ModelProviderCanaryEvidence, OpenAiProviderCanaryConfig,
};
use crate::reviewer::snapshots::{HttpRemoteObjectClient, InMemoryRemoteSnapshotObjectClient};
use crate::runtime::bench::run_job_concurrent;
use crate::runtime::repo::RepoSnapshot;
use crate::util::{redact_known_secrets, timestamp_utc, DEFAULT_MODEL};

const CANARY_PROVIDER_EVIDENCE_FILE: &str = "model-provider.json";
const CANARY_SNAPSHOT_EVIDENCE_FILE: &str = "remote-snapshot-object-store.json";
const CANARY_ARTIFACT_EVIDENCE_FILE: &str = "remote-artifact-object-store.json";
const CANARY_PREFLIGHT_FILE: &str = "preflight.json";
const CANARY_WORKFLOW_FILE: &str = "workflow.json";
const CANARY_MANIFEST_FILE: &str = "manifest.json";
const CANARY_STATUS_FILE: &str = "status.json";
const CANARY_PUBLICATION_FILE: &str = "publication.json";
const CANARY_PREFLIGHT_SCHEMA_VERSION: &str = "muzen.canary-preflight.v1";
const CANARY_PUBLICATION_SCHEMA_VERSION: &str = "muzen.canary-publication.v1";
const CANARY_WORKFLOW_SCHEMA_VERSION: &str = "muzen.canary-workflow.v1";
const CANARY_PROOF_SCHEMA_VERSION: &str = "muzen.canary-proof.v1";
const CANARY_EXPECTED_WORKFLOW: &str = "Muzen Canary Evidence";
const CANARY_EXPECTED_JOB: &str = "publish-canary-evidence";

#[derive(Parser, Debug)]
#[command(name = "muzen")]
#[command(about = "Rust read-only review-runtime MVP for Heimdaal")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Run a ReviewRunJobV1 from JSON.
    Run(RunArgs),
    /// Convenience benchmark wrapper that builds a ReviewRunJobV1 for a repo.
    Bench(BenchArgs),
    /// Build the benchmark ReviewRunJobV1 JSON without executing it.
    BenchJob(BenchArgs),
    /// Inspect Muzen Context Engine output for a local snapshot.
    Context(ContextArgs),
    /// Validate canary publication configuration without writing evidence.
    CanaryPreflight(CanaryPublishArgs),
    /// Publish provider, remote object-store, aggregate, status, and provenance canary evidence.
    CanaryPublish(CanaryPublishArgs),
    /// Compose and validate aggregate canary evidence for CI or scheduled jobs.
    CanaryManifest(CanaryManifestArgs),
    /// Validate a previously published aggregate canary evidence manifest.
    CanaryVerify(CanaryVerifyArgs),
    /// Print structured status for a previously published canary evidence manifest.
    CanaryStatus(CanaryStatusArgs),
    /// Write GitHub Actions workflow provenance for scheduled canary proof.
    CanaryWorkflowProvenance(CanaryWorkflowProvenanceArgs),
    /// Validate a full scheduled canary evidence directory as a final proof bundle.
    CanaryProof(CanaryProofArgs),
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct RunArgs {
    #[arg(long, default_value = "-")]
    pub(crate) job: PathBuf,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct BenchArgs {
    #[arg(long, default_value = ".")]
    pub(crate) repo: PathBuf,

    #[arg(long, default_value_t = 10)]
    pub(crate) sessions: usize,

    #[arg(long, default_value_t = 10)]
    pub(crate) max_active: usize,

    #[arg(long, default_value_t = 7)]
    pub(crate) max_turns: usize,

    #[arg(long, default_value_t = 14)]
    pub(crate) max_tool_calls: usize,

    #[arg(long, default_value_t = 1000)]
    pub(crate) hold_ms: u64,

    #[arg(long, default_value_t = 200)]
    pub(crate) max_file_kb: usize,

    #[arg(long, default_value_t = 120)]
    pub(crate) max_search_matches: usize,

    #[arg(long, default_value = DEFAULT_MODEL)]
    pub(crate) model: String,

    #[arg(long, default_value_t = 256)]
    pub(crate) max_output_tokens: u32,
}

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
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryManifestArgs {
    /// Schema-versioned ModelProviderCanaryEvidence JSON.
    #[arg(long)]
    pub(crate) provider_evidence: PathBuf,

    /// Schema-versioned RemoteObjectStoreCanaryEvidence JSON. Pass once for snapshot and once for artifact evidence.
    #[arg(long = "remote-object-store-evidence", required = true)]
    pub(crate) remote_object_store_evidence: Vec<PathBuf>,

    /// Write aggregate CanaryEvidenceManifest JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteObjectStoreCanaryDriver {
    /// Use in-memory object clients for deterministic local proof.
    Memory,
    /// Use HTTP PUT/GET/DELETE against the generated object URIs.
    Http,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryPublishArgs {
    /// Directory where child evidence, aggregate manifest, status, and publication provenance are written.
    #[arg(long)]
    pub(crate) output_dir: PathBuf,

    /// Reuse an existing schema-versioned provider evidence file instead of running live provider canaries.
    #[arg(long)]
    pub(crate) provider_evidence: Option<PathBuf>,

    /// Remote base URI for snapshot object-store canaries.
    #[arg(long)]
    pub(crate) snapshot_base_uri: String,

    /// Remote base URI for artifact object-store canaries.
    #[arg(long)]
    pub(crate) artifact_base_uri: String,

    /// Remote object-store driver used for snapshot and artifact canaries.
    #[arg(long, value_enum, default_value_t = RemoteObjectStoreCanaryDriver::Http)]
    pub(crate) object_store_driver: RemoteObjectStoreCanaryDriver,

    /// Environment variable containing an HTTP bearer token for the object-store driver.
    #[arg(long, default_value = "MUZEN_REMOTE_OBJECT_STORE_BEARER_TOKEN")]
    pub(crate) object_store_bearer_token_env: String,

    /// OpenAI-compatible model used by live provider canaries.
    #[arg(long, default_value = DEFAULT_MODEL)]
    pub(crate) model: String,

    /// Override the OpenAI-compatible provider base URL for live provider canaries.
    #[arg(long)]
    pub(crate) provider_base_url: Option<String>,

    /// Maximum provider-canary output tokens.
    #[arg(long, default_value_t = 64)]
    pub(crate) max_output_tokens: u32,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryVerifyArgs {
    /// Schema-versioned CanaryEvidenceManifest JSON.
    #[arg(long)]
    pub(crate) manifest: PathBuf,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryStatusArgs {
    /// Schema-versioned CanaryEvidenceManifest JSON.
    #[arg(long)]
    pub(crate) manifest: PathBuf,

    /// Write structured status JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryWorkflowProvenanceArgs {
    /// Write schema-versioned workflow provenance JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct CanaryProofArgs {
    /// Directory containing workflow, preflight, publication, child evidence, manifest, and status JSON.
    #[arg(long)]
    pub(crate) evidence_dir: PathBuf,

    /// Write structured proof JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,

    /// Expected GitHub Actions workflow name for final scheduled proof.
    #[arg(long, default_value = CANARY_EXPECTED_WORKFLOW)]
    pub(crate) expected_workflow: String,

    /// Expected GitHub Actions job id for final scheduled proof.
    #[arg(long, default_value = CANARY_EXPECTED_JOB)]
    pub(crate) expected_job: String,

    /// Expected GitHub repository for final scheduled proof, in owner/name form.
    #[arg(long)]
    pub(crate) expected_repository: Option<String>,

    /// Expected Git ref for final scheduled proof, such as refs/heads/main.
    #[arg(long)]
    pub(crate) expected_git_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryPublicationPreflightReport {
    pub(crate) schema_version: String,
    pub(crate) generated_at_utc: String,
    pub(crate) config: CanaryPublicationPreflightConfig,
    pub(crate) ok: bool,
    pub(crate) checks: Vec<CanaryPublicationPreflightCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryPublicationPreflightConfig {
    pub(crate) output_dir: String,
    pub(crate) provider_evidence_source: CanaryProviderEvidenceSource,
    pub(crate) provider_evidence_input: Option<String>,
    pub(crate) object_store_driver: RemoteObjectStoreCanaryDriver,
    pub(crate) object_store_bearer_token_env: String,
    pub(crate) snapshot_base_uri: String,
    pub(crate) artifact_base_uri: String,
    pub(crate) provider_base_url: String,
    pub(crate) provider_base_url_source: CanaryProviderBaseUrlSource,
    pub(crate) model: String,
    pub(crate) max_output_tokens: u32,
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CanaryProviderBaseUrlSource {
    ExplicitArgument,
    OpenAiBaseUrlEnv,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryPublicationPreflightCheck {
    pub(crate) name: String,
    pub(crate) status: CanaryPublicationPreflightStatus,
    pub(crate) message: String,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CanaryPublicationPreflightStatus {
    Passed,
    Warning,
    Failed,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CanaryProviderEvidenceSource {
    LiveProviderCanary,
    ReusedEvidenceFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryPublicationFiles {
    pub(crate) provider_evidence: String,
    pub(crate) snapshot_object_store_evidence: String,
    pub(crate) artifact_object_store_evidence: String,
    pub(crate) manifest: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryPublicationReport {
    pub(crate) schema_version: String,
    pub(crate) generated_at_utc: String,
    pub(crate) provider_evidence_source: CanaryProviderEvidenceSource,
    pub(crate) provider_evidence_input: Option<String>,
    pub(crate) object_store_driver: RemoteObjectStoreCanaryDriver,
    pub(crate) provider_base_url: Option<String>,
    pub(crate) model: String,
    pub(crate) max_evidence_age_seconds: u64,
    pub(crate) files: CanaryPublicationFiles,
    pub(crate) status_ok: bool,
    pub(crate) failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryProofFiles {
    pub(crate) preflight: String,
    pub(crate) workflow: String,
    pub(crate) publication: String,
    pub(crate) provider_evidence: String,
    pub(crate) snapshot_object_store_evidence: String,
    pub(crate) artifact_object_store_evidence: String,
    pub(crate) manifest: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryProofWorkflowExpectation {
    pub(crate) event_name: String,
    pub(crate) workflow: String,
    pub(crate) job: String,
    pub(crate) repository: Option<String>,
    pub(crate) git_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryProofFileDigest {
    pub(crate) label: String,
    pub(crate) file: String,
    pub(crate) bytes: u64,
    pub(crate) blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryProofReport {
    pub(crate) schema_version: String,
    pub(crate) checked_at_utc: String,
    pub(crate) evidence_dir: String,
    pub(crate) max_evidence_age_seconds: u64,
    pub(crate) expected_files: CanaryProofFiles,
    pub(crate) workflow_expectation: CanaryProofWorkflowExpectation,
    pub(crate) file_digests: Vec<CanaryProofFileDigest>,
    pub(crate) workflow: Option<CanaryWorkflowProvenance>,
    pub(crate) preflight: Option<CanaryPublicationPreflightReport>,
    pub(crate) publication: Option<CanaryPublicationReport>,
    pub(crate) status: Option<crate::reviewer::canaries::CanaryEvidenceStatusReport>,
    pub(crate) ok: bool,
    pub(crate) failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CanaryWorkflowProvenance {
    pub(crate) schema_version: String,
    pub(crate) generated_at_utc: String,
    pub(crate) event_name: String,
    pub(crate) workflow: String,
    pub(crate) job: String,
    pub(crate) run_id: String,
    pub(crate) run_attempt: String,
    pub(crate) repository: String,
    pub(crate) git_ref: String,
    pub(crate) sha: String,
    pub(crate) actor: String,
    pub(crate) server_url: String,
    pub(crate) run_url: String,
}

pub(crate) fn run_json(args: RunArgs) -> Result<i32> {
    let mut input = String::new();
    if args.job == Path::new("-") {
        std::io::stdin().read_to_string(&mut input)?;
    } else {
        input = fs::read_to_string(&args.job)
            .with_context(|| format!("failed to read job {}", args.job.display()))?;
    }
    let job: ReviewRunJobV1 =
        serde_json::from_str(&input).context("invalid ReviewRunJobV1 JSON")?;
    let report = run_job_concurrent(job)?;
    Ok(if report.completed_sessions == report.sessions {
        0
    } else {
        4
    })
}

pub fn main_entry() {
    let code = match run_main() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", redact_known_secrets(&format!("{error:#}"), &[]));
            4
        }
    };
    std::process::exit(code);
}

pub(crate) fn run_main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_json(args),
        Command::Bench(args) => {
            let hold_ms = args.hold_ms;
            let job = bench_job(&args)?;
            let report = run_job_concurrent(job)?;
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.benchmark_valid {
                bail!(
                    "concurrent benchmark gates failed: {:?}",
                    report.benchmark_failures
                );
            }
            if report.completed_sessions != report.sessions {
                bail!(
                    "only {}/{} sessions completed",
                    report.completed_sessions,
                    report.sessions
                );
            }
            Ok(0)
        }
        Command::BenchJob(args) => {
            let job = bench_job(&args)?;
            println!("{}", serde_json::to_string_pretty(&job)?);
            Ok(0)
        }
        Command::Context(args) => run_context(args),
        Command::CanaryPreflight(args) => run_canary_preflight(args),
        Command::CanaryPublish(args) => run_canary_publish(args),
        Command::CanaryManifest(args) => run_canary_manifest(args),
        Command::CanaryVerify(args) => run_canary_verify(args),
        Command::CanaryStatus(args) => run_canary_status(args),
        Command::CanaryWorkflowProvenance(args) => run_canary_workflow_provenance(args),
        Command::CanaryProof(args) => run_canary_proof(args),
    }
}

pub(crate) fn run_context(args: ContextArgs) -> Result<i32> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for context engine")?;
    runtime.block_on(async move {
        match args.command {
            ContextCommand::Index(args) => {
                let (_engine, _snapshot, manifest) = index_context_snapshot(&args).await?;
                write_context_output(args.output.as_ref(), &manifest)?;
                Ok(0)
            }
            ContextCommand::Pack(args) => {
                let (engine, snapshot, _manifest) = index_context_snapshot(&args.snapshot).await?;
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
                write_context_output(args.snapshot.output.as_ref(), &pack)?;
                Ok(0)
            }
            ContextCommand::Query(args) => {
                let (engine, snapshot, _manifest) = index_context_snapshot(&args.snapshot).await?;
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
                write_context_output(args.snapshot.output.as_ref(), &result)?;
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
        }
    })
}

async fn index_context_snapshot(
    args: &ContextSnapshotArgs,
) -> Result<(
    SnapshotContextEngine,
    Arc<RepoSnapshot>,
    crate::context_engine::ContextManifestArtifact,
)> {
    let snapshot = build_context_snapshot(args)?;
    let mut engine = SnapshotContextEngine::new(context_engine_config(args)?);
    if let Some(root) = &args.derived_cache_root {
        engine = engine.with_derived_cache_file(root.join("context-derived-cache.json"));
    }
    engine
        .index_snapshot(
            context_index_request(&snapshot, engine.config_ref(), args)?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let index = engine
        .get_index(&snapshot.snapshot_id)
        .ok_or_else(|| anyhow::anyhow!("context index was not stored"))?;
    Ok((engine, snapshot, index.manifest_artifact.clone()))
}

fn context_index_request(
    snapshot: &Arc<RepoSnapshot>,
    config: &ContextEngineConfig,
    args: &ContextSnapshotArgs,
) -> Result<ContextIndexRequest> {
    let mut request = ContextIndexRequest::for_snapshot(Arc::clone(snapshot), config);
    if let Some(path) = &args.host_metadata_json {
        request.host_metadata =
            read_context_json_file::<BTreeMap<String, serde_json::Value>>(path)?;
        request.include_host_context = true;
    }
    if let Some(path) = &args.host_instruction_json {
        request.instructions =
            read_context_json_file::<Vec<crate::runtime::contracts::SessionInstruction>>(path)?;
        request.include_host_context = true;
    }
    Ok(request)
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
        crate::runtime::contracts::SnapshotStoragePolicy::default(),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
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

fn read_context_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("invalid JSON in {}", path.display()))
}

pub(crate) fn run_canary_preflight(args: CanaryPublishArgs) -> Result<i32> {
    let report = canary_publication_preflight_report(&args);
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report.ok {
        return Ok(0);
    }
    let failures = report
        .checks
        .iter()
        .filter(|check| check.status == CanaryPublicationPreflightStatus::Failed)
        .map(|check| format!("{}: {}", check.name, check.message))
        .collect::<Vec<_>>();
    bail!(
        "canary publication preflight failed: {}",
        failures.join("; ")
    );
}

pub(crate) fn canary_publication_preflight_report(
    args: &CanaryPublishArgs,
) -> CanaryPublicationPreflightReport {
    canary_publication_preflight_report_with_env(args, &|name| env::var(name).ok())
}

pub(crate) fn canary_publication_preflight_report_with_env(
    args: &CanaryPublishArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> CanaryPublicationPreflightReport {
    let mut checks = Vec::new();
    check_output_dir(args, &mut checks);
    check_provider_canary_config(args, env_lookup, &mut checks);
    check_remote_store_base_uri(
        "snapshotBaseUri",
        &args.snapshot_base_uri,
        args.object_store_driver,
        &mut checks,
    );
    check_remote_store_base_uri(
        "artifactBaseUri",
        &args.artifact_base_uri,
        args.object_store_driver,
        &mut checks,
    );
    check_remote_store_auth(args, env_lookup, &mut checks);
    if args.max_output_tokens == 0 {
        push_preflight_check(
            &mut checks,
            "maxOutputTokens",
            CanaryPublicationPreflightStatus::Failed,
            "provider canary max output tokens must be greater than zero",
        );
    } else if args.max_output_tokens > 64 {
        push_preflight_check(
            &mut checks,
            "maxOutputTokens",
            CanaryPublicationPreflightStatus::Warning,
            "provider canary max output tokens will be clamped to 64",
        );
    } else {
        push_preflight_check(
            &mut checks,
            "maxOutputTokens",
            CanaryPublicationPreflightStatus::Passed,
            "provider canary output envelope is configured",
        );
    }
    if args.max_evidence_age_seconds == 0 {
        push_preflight_check(
            &mut checks,
            "maxEvidenceAgeSeconds",
            CanaryPublicationPreflightStatus::Failed,
            "freshness window must be greater than zero",
        );
    } else {
        push_preflight_check(
            &mut checks,
            "maxEvidenceAgeSeconds",
            CanaryPublicationPreflightStatus::Passed,
            "freshness window is configured",
        );
    }

    CanaryPublicationPreflightReport {
        schema_version: CANARY_PREFLIGHT_SCHEMA_VERSION.to_string(),
        generated_at_utc: timestamp_utc(),
        config: canary_publication_preflight_config(args, env_lookup),
        ok: checks
            .iter()
            .all(|check| check.status != CanaryPublicationPreflightStatus::Failed),
        checks,
    }
}

fn canary_publication_preflight_config(
    args: &CanaryPublishArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> CanaryPublicationPreflightConfig {
    let (provider_base_url, provider_base_url_source) =
        effective_provider_base_url(args, env_lookup);
    CanaryPublicationPreflightConfig {
        output_dir: args.output_dir.display().to_string(),
        provider_evidence_source: if args.provider_evidence.is_some() {
            CanaryProviderEvidenceSource::ReusedEvidenceFile
        } else {
            CanaryProviderEvidenceSource::LiveProviderCanary
        },
        provider_evidence_input: args
            .provider_evidence
            .as_ref()
            .map(|path| path.display().to_string()),
        object_store_driver: args.object_store_driver,
        object_store_bearer_token_env: args.object_store_bearer_token_env.clone(),
        snapshot_base_uri: args.snapshot_base_uri.clone(),
        artifact_base_uri: args.artifact_base_uri.clone(),
        provider_base_url,
        provider_base_url_source,
        model: args.model.clone(),
        max_output_tokens: args.max_output_tokens,
        max_evidence_age_seconds: args.max_evidence_age_seconds,
    }
}

fn check_output_dir(args: &CanaryPublishArgs, checks: &mut Vec<CanaryPublicationPreflightCheck>) {
    if args.output_dir.as_os_str().is_empty() {
        push_preflight_check(
            checks,
            "outputDir",
            CanaryPublicationPreflightStatus::Failed,
            "output directory must not be empty",
        );
        return;
    }
    if args.output_dir.is_file() {
        push_preflight_check(
            checks,
            "outputDir",
            CanaryPublicationPreflightStatus::Failed,
            "output directory path points to a file",
        );
        return;
    }
    push_preflight_check(
        checks,
        "outputDir",
        CanaryPublicationPreflightStatus::Passed,
        "output directory path can be used for evidence publication",
    );
}

fn check_provider_canary_config(
    args: &CanaryPublishArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    checks: &mut Vec<CanaryPublicationPreflightCheck>,
) {
    if let Some(path) = &args.provider_evidence {
        match load_model_provider_canary_evidence(path) {
            Ok(evidence) => match evidence.require_passed() {
                Ok(()) => push_preflight_check(
                    checks,
                    "providerEvidence",
                    CanaryPublicationPreflightStatus::Passed,
                    "reused provider evidence passes its schema gate",
                ),
                Err(error) => push_preflight_check(
                    checks,
                    "providerEvidence",
                    CanaryPublicationPreflightStatus::Failed,
                    format!("reused provider evidence does not pass: {error}"),
                ),
            },
            Err(error) => push_preflight_check(
                checks,
                "providerEvidence",
                CanaryPublicationPreflightStatus::Failed,
                format!("failed to load reused provider evidence: {error}"),
            ),
        }
        return;
    }

    if non_empty_env(env_lookup, "OPENAI_API_KEY").is_some() {
        push_preflight_check(
            checks,
            "providerCredential",
            CanaryPublicationPreflightStatus::Passed,
            "OPENAI_API_KEY is present for live provider canaries",
        );
    } else {
        push_preflight_check(
            checks,
            "providerCredential",
            CanaryPublicationPreflightStatus::Failed,
            "OPENAI_API_KEY is required when provider evidence is not reused",
        );
    }

    if args.model.trim().is_empty() {
        push_preflight_check(
            checks,
            "providerModel",
            CanaryPublicationPreflightStatus::Failed,
            "provider canary model must not be empty",
        );
    } else {
        push_preflight_check(
            checks,
            "providerModel",
            CanaryPublicationPreflightStatus::Passed,
            "provider canary model is configured",
        );
    }

    let (provider_base_url, _) = effective_provider_base_url(args, env_lookup);
    check_http_url("providerBaseUrl", &provider_base_url, checks);
}

fn effective_provider_base_url(
    args: &CanaryPublishArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> (String, CanaryProviderBaseUrlSource) {
    if let Some(base_url) = args
        .provider_base_url
        .as_ref()
        .filter(|base_url| !base_url.trim().is_empty())
    {
        return (
            base_url.clone(),
            CanaryProviderBaseUrlSource::ExplicitArgument,
        );
    }
    if let Some(base_url) = non_empty_env(env_lookup, "OPENAI_BASE_URL") {
        return (base_url, CanaryProviderBaseUrlSource::OpenAiBaseUrlEnv);
    }
    (
        "https://api.openai.com/v1".to_string(),
        CanaryProviderBaseUrlSource::Default,
    )
}

fn check_remote_store_base_uri(
    name: &'static str,
    base_uri: &str,
    driver: RemoteObjectStoreCanaryDriver,
    checks: &mut Vec<CanaryPublicationPreflightCheck>,
) {
    let Some((scheme, _rest)) = base_uri.split_once("://") else {
        push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Failed,
            "remote object-store base URI must include a scheme",
        );
        return;
    };
    if base_uri.trim().is_empty()
        || base_uri.starts_with("file://")
        || base_uri.chars().any(char::is_whitespace)
    {
        push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Failed,
            "remote object-store base URI must be non-empty, non-file, and contain no whitespace",
        );
        return;
    }
    if driver == RemoteObjectStoreCanaryDriver::Http && !matches!(scheme, "http" | "https") {
        push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Failed,
            "HTTP object-store driver requires an http:// or https:// base URI",
        );
        return;
    }
    if driver == RemoteObjectStoreCanaryDriver::Http {
        check_http_url(name, base_uri, checks);
    } else {
        push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Passed,
            "remote object-store base URI is structurally valid for the selected driver",
        );
    }
}

fn check_http_url(
    name: &'static str,
    value: &str,
    checks: &mut Vec<CanaryPublicationPreflightCheck>,
) {
    match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Passed,
            "HTTP URL is structurally valid",
        ),
        Ok(_) => push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Failed,
            "HTTP URL must use http:// or https://",
        ),
        Err(error) => push_preflight_check(
            checks,
            name,
            CanaryPublicationPreflightStatus::Failed,
            format!("invalid HTTP URL: {error}"),
        ),
    }
}

fn check_remote_store_auth(
    args: &CanaryPublishArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    checks: &mut Vec<CanaryPublicationPreflightCheck>,
) {
    if args.object_store_driver != RemoteObjectStoreCanaryDriver::Http {
        push_preflight_check(
            checks,
            "remoteObjectStoreAuth",
            CanaryPublicationPreflightStatus::Passed,
            "selected object-store driver does not use HTTP authorization",
        );
        return;
    }
    if args.object_store_bearer_token_env.trim().is_empty() {
        push_preflight_check(
            checks,
            "remoteObjectStoreAuth",
            CanaryPublicationPreflightStatus::Warning,
            "no bearer-token environment variable configured; assuming signed or anonymous HTTP object URIs",
        );
        return;
    }
    if non_empty_env(env_lookup, &args.object_store_bearer_token_env).is_some() {
        push_preflight_check(
            checks,
            "remoteObjectStoreAuth",
            CanaryPublicationPreflightStatus::Passed,
            format!(
                "{} is present for HTTP object-store canaries",
                args.object_store_bearer_token_env
            ),
        );
    } else {
        push_preflight_check(
            checks,
            "remoteObjectStoreAuth",
            CanaryPublicationPreflightStatus::Warning,
            format!(
                "{} is not set; assuming signed or anonymous HTTP object URIs",
                args.object_store_bearer_token_env
            ),
        );
    }
}

fn non_empty_env(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env_lookup(name).filter(|value| !value.trim().is_empty())
}

fn push_preflight_check(
    checks: &mut Vec<CanaryPublicationPreflightCheck>,
    name: impl Into<String>,
    status: CanaryPublicationPreflightStatus,
    message: impl Into<String>,
) {
    checks.push(CanaryPublicationPreflightCheck {
        name: name.into(),
        status,
        message: message.into(),
    });
}

pub(crate) fn run_canary_publish(args: CanaryPublishArgs) -> Result<i32> {
    fs::create_dir_all(&args.output_dir).with_context(|| {
        format!(
            "failed to create canary evidence directory {}",
            args.output_dir.display()
        )
    })?;

    let provider_path = args.output_dir.join(CANARY_PROVIDER_EVIDENCE_FILE);
    let snapshot_path = args.output_dir.join(CANARY_SNAPSHOT_EVIDENCE_FILE);
    let artifact_path = args.output_dir.join(CANARY_ARTIFACT_EVIDENCE_FILE);
    let manifest_path = args.output_dir.join(CANARY_MANIFEST_FILE);
    let status_path = args.output_dir.join(CANARY_STATUS_FILE);
    let publication_path = args.output_dir.join(CANARY_PUBLICATION_FILE);

    let provider = load_or_run_provider_canary_evidence(&args)?;
    let provider_export = export_model_provider_canary_evidence(&provider_path, &provider)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    eprintln!(
        "wrote provider canary evidence to {} ({} bytes)",
        provider_export.path.display(),
        provider_export.bytes
    );

    let (snapshot, artifact) = run_remote_object_store_canary_evidence(&args)?;
    let snapshot_export = export_remote_object_store_canary_evidence(&snapshot_path, &snapshot)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    eprintln!(
        "wrote snapshot object-store canary evidence to {} ({} bytes)",
        snapshot_export.path.display(),
        snapshot_export.bytes
    );
    let artifact_export = export_remote_object_store_canary_evidence(&artifact_path, &artifact)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    eprintln!(
        "wrote artifact object-store canary evidence to {} ({} bytes)",
        artifact_export.path.display(),
        artifact_export.bytes
    );

    let manifest = CanaryEvidenceManifest::from_evidence(Some(provider), vec![snapshot, artifact]);
    let manifest_export = export_canary_evidence_manifest(&manifest_path, &manifest)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    eprintln!(
        "wrote canary evidence manifest to {} ({} bytes)",
        manifest_export.path.display(),
        manifest_export.bytes
    );

    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    let report = manifest.status_report(&freshness);
    let status_bytes = write_canary_status_report(&status_path, &report)?;
    eprintln!(
        "wrote canary evidence status to {} ({} bytes)",
        status_path.display(),
        status_bytes
    );
    let publication = canary_publication_report(&args, &report);
    let publication_bytes = write_canary_publication_report(&publication_path, &publication)?;
    eprintln!(
        "wrote canary publication report to {} ({} bytes)",
        publication_path.display(),
        publication_bytes
    );
    if report.ok {
        return Ok(0);
    }
    bail!(
        "canary evidence manifest gate failed: {}",
        report.failures().join("; ")
    );
}

fn load_or_run_provider_canary_evidence(
    args: &CanaryPublishArgs,
) -> Result<ModelProviderCanaryEvidence> {
    if let Some(path) = &args.provider_evidence {
        return load_model_provider_canary_evidence(path)
            .map_err(|error| anyhow::anyhow!("{error}"));
    }

    let mut config = OpenAiProviderCanaryConfig::from_env(args.model.clone());
    config.enabled = true;
    if let Some(base_url) = &args.provider_base_url {
        config.base_url = base_url.clone();
    }
    config.model = args.model.clone();
    config.max_output_tokens = args.max_output_tokens.clamp(1, 64);
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for provider canaries")?;
    let reports = tokio_runtime.block_on(run_openai_provider_canaries(
        config,
        Arc::new(EnvCredentialResolver),
    ));
    Ok(ModelProviderCanaryEvidence::from_reports(reports))
}

fn run_remote_object_store_canary_evidence(
    args: &CanaryPublishArgs,
) -> Result<(
    crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence,
    crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence,
)> {
    match args.object_store_driver {
        RemoteObjectStoreCanaryDriver::Memory => {
            let snapshot_client = InMemoryRemoteSnapshotObjectClient::default();
            let artifact_client = InMemoryRemoteArtifactObjectClient::default();
            Ok((
                run_remote_snapshot_object_store_canary(
                    args.snapshot_base_uri.clone(),
                    &snapshot_client,
                ),
                run_remote_artifact_object_store_canary(
                    args.artifact_base_uri.clone(),
                    &artifact_client,
                ),
            ))
        }
        RemoteObjectStoreCanaryDriver::Http => {
            let client = http_remote_object_client(&args.object_store_bearer_token_env)?;
            Ok((
                run_remote_snapshot_object_store_canary(args.snapshot_base_uri.clone(), &client),
                run_remote_artifact_object_store_canary(args.artifact_base_uri.clone(), &client),
            ))
        }
    }
}

fn http_remote_object_client(token_env: &str) -> Result<HttpRemoteObjectClient> {
    let token = if token_env.trim().is_empty() {
        None
    } else {
        env::var(token_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    match token {
        Some(token) => {
            HttpRemoteObjectClient::bearer_token(token).map_err(|error| anyhow::anyhow!("{error}"))
        }
        None => HttpRemoteObjectClient::new().map_err(|error| anyhow::anyhow!("{error}")),
    }
}

pub(crate) fn run_canary_manifest(args: CanaryManifestArgs) -> Result<i32> {
    let provider = load_model_provider_canary_evidence(&args.provider_evidence)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut remote_evidence = Vec::with_capacity(args.remote_object_store_evidence.len());
    for path in &args.remote_object_store_evidence {
        remote_evidence.push(
            load_remote_object_store_canary_evidence(path)
                .map_err(|error| anyhow::anyhow!("{error}"))?,
        );
    }
    let manifest = CanaryEvidenceManifest::from_evidence(Some(provider), remote_evidence);
    if let Some(path) = &args.output {
        let export = export_canary_evidence_manifest(path, &manifest)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        eprintln!(
            "wrote canary evidence manifest to {} ({} bytes)",
            export.path.display(),
            export.bytes
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    }
    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    manifest
        .require_passed_with_freshness(&freshness)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(0)
}

pub(crate) fn run_canary_verify(args: CanaryVerifyArgs) -> Result<i32> {
    let manifest = load_canary_evidence_manifest(&args.manifest)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    manifest
        .require_passed_with_freshness(&freshness)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(0)
}

pub(crate) fn run_canary_status(args: CanaryStatusArgs) -> Result<i32> {
    let manifest = load_canary_evidence_manifest(&args.manifest)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    let report = manifest.status_report(&freshness);
    if let Some(path) = &args.output {
        let status_bytes = write_canary_status_report(path, &report)?;
        eprintln!(
            "wrote canary evidence status to {} ({} bytes)",
            path.display(),
            status_bytes
        );
    } else {
        let status_json = canary_status_report_json(&report)?;
        print!("{}", String::from_utf8(status_json)?);
    }
    if report.ok {
        return Ok(0);
    }
    bail!(
        "canary evidence manifest status failed: {}",
        report.failures().join("; ")
    );
}

pub(crate) fn run_canary_proof(args: CanaryProofArgs) -> Result<i32> {
    let report = canary_proof_report(&args);
    if let Some(path) = &args.output {
        let bytes = write_canary_proof_report(path, &report)?;
        eprintln!(
            "wrote canary proof report to {} ({} bytes)",
            path.display(),
            bytes
        );
    } else {
        let bytes = canary_proof_report_json(&report)?;
        print!("{}", String::from_utf8(bytes)?);
    }
    if report.ok {
        return Ok(0);
    }
    bail!("canary proof failed: {}", report.failures.join("; "));
}

pub(crate) fn run_canary_workflow_provenance(args: CanaryWorkflowProvenanceArgs) -> Result<i32> {
    run_canary_workflow_provenance_with_env(args, &|name| env::var(name).ok())
}

pub(crate) fn run_canary_workflow_provenance_with_env(
    args: CanaryWorkflowProvenanceArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<i32> {
    let provenance = canary_workflow_provenance_from_env(env_lookup);
    if let Some(path) = &args.output {
        let bytes = write_canary_workflow_provenance(path, &provenance)?;
        eprintln!(
            "wrote canary workflow provenance to {} ({} bytes)",
            path.display(),
            bytes
        );
    } else {
        let bytes = canary_workflow_provenance_json(&provenance)?;
        print!("{}", String::from_utf8(bytes)?);
    }
    Ok(0)
}

pub(crate) fn canary_workflow_provenance_from_env(
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> CanaryWorkflowProvenance {
    let server_url = canary_env_value(env_lookup, "GITHUB_SERVER_URL");
    let repository = canary_env_value(env_lookup, "GITHUB_REPOSITORY");
    let run_id = canary_env_value(env_lookup, "GITHUB_RUN_ID");
    let run_url =
        if server_url.trim().is_empty() || repository.trim().is_empty() || run_id.trim().is_empty()
        {
            String::new()
        } else {
            format!(
                "{}/{}/actions/runs/{}",
                server_url.trim_end_matches('/'),
                repository,
                run_id
            )
        };

    CanaryWorkflowProvenance {
        schema_version: CANARY_WORKFLOW_SCHEMA_VERSION.to_string(),
        generated_at_utc: timestamp_utc(),
        event_name: canary_env_value(env_lookup, "GITHUB_EVENT_NAME"),
        workflow: canary_env_value(env_lookup, "GITHUB_WORKFLOW"),
        job: canary_env_value(env_lookup, "GITHUB_JOB"),
        run_id,
        run_attempt: canary_env_value(env_lookup, "GITHUB_RUN_ATTEMPT"),
        repository,
        git_ref: canary_env_value(env_lookup, "GITHUB_REF"),
        sha: canary_env_value(env_lookup, "GITHUB_SHA"),
        actor: canary_env_value(env_lookup, "GITHUB_ACTOR"),
        server_url,
        run_url,
    }
}

fn canary_env_value(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> String {
    env_lookup(name).unwrap_or_default()
}

pub(crate) fn canary_proof_report(args: &CanaryProofArgs) -> CanaryProofReport {
    let expected_files = canary_proof_expected_files();
    let preflight_path = args.evidence_dir.join(&expected_files.preflight);
    let workflow_path = args.evidence_dir.join(&expected_files.workflow);
    let publication_path = args.evidence_dir.join(&expected_files.publication);
    let provider_path = args.evidence_dir.join(&expected_files.provider_evidence);
    let snapshot_path = args
        .evidence_dir
        .join(&expected_files.snapshot_object_store_evidence);
    let artifact_path = args
        .evidence_dir
        .join(&expected_files.artifact_object_store_evidence);
    let manifest_path = args.evidence_dir.join(&expected_files.manifest);
    let status_path = args.evidence_dir.join(&expected_files.status);

    let mut failures = Vec::new();
    if args.max_evidence_age_seconds == 0 {
        failures.push("max evidence age seconds must be greater than zero".to_string());
    }

    let file_digests = canary_proof_file_digests(
        &[
            ("workflow", &expected_files.workflow, &workflow_path),
            ("preflight", &expected_files.preflight, &preflight_path),
            (
                "publication",
                &expected_files.publication,
                &publication_path,
            ),
            (
                "providerEvidence",
                &expected_files.provider_evidence,
                &provider_path,
            ),
            (
                "snapshotObjectStoreEvidence",
                &expected_files.snapshot_object_store_evidence,
                &snapshot_path,
            ),
            (
                "artifactObjectStoreEvidence",
                &expected_files.artifact_object_store_evidence,
                &artifact_path,
            ),
            ("manifest", &expected_files.manifest, &manifest_path),
            ("status", &expected_files.status, &status_path),
        ],
        &mut failures,
    );

    let preflight: Option<CanaryPublicationPreflightReport> =
        read_canary_json_file(&preflight_path, "canary preflight report", &mut failures);
    if let Some(preflight) = &preflight {
        validate_canary_preflight_report(preflight, &mut failures);
        validate_canary_proof_timestamp_freshness(
            "canary preflight report",
            &preflight.generated_at_utc,
            args.max_evidence_age_seconds,
            &mut failures,
        );
    }

    let workflow: Option<CanaryWorkflowProvenance> =
        read_canary_json_file(&workflow_path, "canary workflow provenance", &mut failures);
    if let Some(workflow) = &workflow {
        validate_canary_workflow_provenance(workflow, args, &mut failures);
        validate_canary_proof_timestamp_freshness(
            "canary workflow provenance",
            &workflow.generated_at_utc,
            args.max_evidence_age_seconds,
            &mut failures,
        );
    }

    let publication: Option<CanaryPublicationReport> = read_canary_json_file(
        &publication_path,
        "canary publication report",
        &mut failures,
    );
    if let Some(publication) = &publication {
        validate_canary_publication_proof_report(publication, args, &expected_files, &mut failures);
        validate_canary_proof_timestamp_freshness(
            "canary publication report",
            &publication.generated_at_utc,
            args.max_evidence_age_seconds,
            &mut failures,
        );
    }

    let status: Option<crate::reviewer::canaries::CanaryEvidenceStatusReport> =
        read_canary_json_file(&status_path, "canary status report", &mut failures);
    if let Some(status) = &status {
        validate_canary_status_proof_report(status, args, &mut failures);
        validate_canary_proof_timestamp_freshness(
            "canary status report freshness check",
            &status.freshness_checked_at_utc,
            args.max_evidence_age_seconds,
            &mut failures,
        );
    }

    let manifest = load_canary_evidence_manifest_for_proof(&manifest_path, &mut failures);
    if let Some(manifest) = &manifest {
        let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
        if let Err(error) = manifest.require_passed_with_freshness(&freshness) {
            failures.push(format!(
                "canary manifest failed proof freshness/gate validation: {error}"
            ));
        }
        validate_canary_manifest_status_consistency(manifest, status.as_ref(), &mut failures);
    }

    let provider = load_model_provider_canary_evidence_for_proof(&provider_path, &mut failures);
    if let Some(provider) = &provider {
        validate_model_provider_canary_proof(provider, publication.as_ref(), &mut failures);
    }

    let snapshot = load_remote_object_store_canary_evidence_for_proof(
        &snapshot_path,
        "snapshot",
        &mut failures,
    );
    if let Some(snapshot) = &snapshot {
        validate_remote_object_store_canary_proof(
            snapshot,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Snapshot,
            &mut failures,
        );
    }

    let artifact = load_remote_object_store_canary_evidence_for_proof(
        &artifact_path,
        "artifact",
        &mut failures,
    );
    if let Some(artifact) = &artifact {
        validate_remote_object_store_canary_proof(
            artifact,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Artifact,
            &mut failures,
        );
    }

    validate_canary_child_files_match_manifest(
        manifest.as_ref(),
        provider.as_ref(),
        snapshot.as_ref(),
        artifact.as_ref(),
        &mut failures,
    );
    validate_canary_preflight_consistency(
        preflight.as_ref(),
        CanaryPreflightConsistencyContext {
            publication: publication.as_ref(),
            status: status.as_ref(),
            provider: provider.as_ref(),
            snapshot: snapshot.as_ref(),
            artifact: artifact.as_ref(),
            max_evidence_age_seconds: args.max_evidence_age_seconds,
        },
        &mut failures,
    );

    CanaryProofReport {
        schema_version: CANARY_PROOF_SCHEMA_VERSION.to_string(),
        checked_at_utc: timestamp_utc(),
        evidence_dir: args.evidence_dir.display().to_string(),
        max_evidence_age_seconds: args.max_evidence_age_seconds,
        expected_files,
        workflow_expectation: CanaryProofWorkflowExpectation {
            event_name: "schedule".to_string(),
            workflow: args.expected_workflow.clone(),
            job: args.expected_job.clone(),
            repository: args.expected_repository.clone(),
            git_ref: args.expected_git_ref.clone(),
        },
        file_digests,
        workflow,
        preflight,
        publication,
        status,
        ok: failures.is_empty(),
        failures,
    }
}

fn canary_status_report_json(
    report: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
) -> Result<Vec<u8>> {
    let mut status_json = serde_json::to_vec_pretty(report)?;
    status_json.push(b'\n');
    Ok(status_json)
}

fn write_canary_status_report(
    path: &Path,
    report: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
) -> Result<usize> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create canary status directory {}",
                parent.display()
            )
        })?;
    }
    let status_json = canary_status_report_json(report)?;
    fs::write(path, &status_json)
        .with_context(|| format!("failed to write canary status {}", path.display()))?;
    Ok(status_json.len())
}

fn canary_workflow_provenance_json(report: &CanaryWorkflowProvenance) -> Result<Vec<u8>> {
    let mut json = serde_json::to_vec_pretty(report)?;
    json.push(b'\n');
    Ok(json)
}

fn write_canary_workflow_provenance(
    path: &Path,
    report: &CanaryWorkflowProvenance,
) -> Result<usize> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create canary workflow provenance directory {}",
                parent.display()
            )
        })?;
    }
    let json = canary_workflow_provenance_json(report)?;
    fs::write(path, &json).with_context(|| {
        format!(
            "failed to write canary workflow provenance {}",
            path.display()
        )
    })?;
    Ok(json.len())
}

pub(crate) fn canary_publication_report(
    args: &CanaryPublishArgs,
    status: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
) -> CanaryPublicationReport {
    CanaryPublicationReport {
        schema_version: CANARY_PUBLICATION_SCHEMA_VERSION.to_string(),
        generated_at_utc: timestamp_utc(),
        provider_evidence_source: if args.provider_evidence.is_some() {
            CanaryProviderEvidenceSource::ReusedEvidenceFile
        } else {
            CanaryProviderEvidenceSource::LiveProviderCanary
        },
        provider_evidence_input: args
            .provider_evidence
            .as_ref()
            .map(|path| path.display().to_string()),
        object_store_driver: args.object_store_driver,
        provider_base_url: args.provider_base_url.clone(),
        model: args.model.clone(),
        max_evidence_age_seconds: args.max_evidence_age_seconds,
        files: CanaryPublicationFiles {
            provider_evidence: CANARY_PROVIDER_EVIDENCE_FILE.to_string(),
            snapshot_object_store_evidence: CANARY_SNAPSHOT_EVIDENCE_FILE.to_string(),
            artifact_object_store_evidence: CANARY_ARTIFACT_EVIDENCE_FILE.to_string(),
            manifest: CANARY_MANIFEST_FILE.to_string(),
            status: CANARY_STATUS_FILE.to_string(),
        },
        status_ok: status.ok,
        failures: status.failures(),
    }
}

fn canary_publication_report_json(report: &CanaryPublicationReport) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_canary_publication_report(path: &Path, report: &CanaryPublicationReport) -> Result<usize> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create canary publication directory {}",
                parent.display()
            )
        })?;
    }
    let bytes = canary_publication_report_json(report)?;
    fs::write(path, &bytes)
        .with_context(|| format!("failed to write canary publication {}", path.display()))?;
    Ok(bytes.len())
}

fn canary_proof_expected_files() -> CanaryProofFiles {
    CanaryProofFiles {
        preflight: CANARY_PREFLIGHT_FILE.to_string(),
        workflow: CANARY_WORKFLOW_FILE.to_string(),
        publication: CANARY_PUBLICATION_FILE.to_string(),
        provider_evidence: CANARY_PROVIDER_EVIDENCE_FILE.to_string(),
        snapshot_object_store_evidence: CANARY_SNAPSHOT_EVIDENCE_FILE.to_string(),
        artifact_object_store_evidence: CANARY_ARTIFACT_EVIDENCE_FILE.to_string(),
        manifest: CANARY_MANIFEST_FILE.to_string(),
        status: CANARY_STATUS_FILE.to_string(),
    }
}

fn canary_proof_file_digests(
    files: &[(&str, &str, &Path)],
    failures: &mut Vec<String>,
) -> Vec<CanaryProofFileDigest> {
    files
        .iter()
        .filter_map(|(label, file, path)| {
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    failures.push(format!(
                        "failed to read canary proof file digest {label} {}: {error}",
                        path.display()
                    ));
                    return None;
                }
            };
            Some(CanaryProofFileDigest {
                label: (*label).to_string(),
                file: (*file).to_string(),
                bytes: bytes.len() as u64,
                blake3: blake3::hash(&bytes).to_hex().to_string(),
            })
        })
        .collect()
}

fn read_canary_json_file<T: DeserializeOwned>(
    path: &Path,
    label: &str,
    failures: &mut Vec<String>,
) -> Option<T> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            failures.push(format!(
                "failed to read {label} {}: {error}",
                path.display()
            ));
            return None;
        }
    };
    match serde_json::from_str(&contents) {
        Ok(value) => Some(value),
        Err(error) => {
            failures.push(format!("invalid {label} {}: {error}", path.display()));
            None
        }
    }
}

fn load_canary_evidence_manifest_for_proof(
    path: &Path,
    failures: &mut Vec<String>,
) -> Option<CanaryEvidenceManifest> {
    match load_canary_evidence_manifest(path) {
        Ok(manifest) => Some(manifest),
        Err(error) => {
            failures.push(format!(
                "failed to load canary manifest {}: {error}",
                path.display()
            ));
            None
        }
    }
}

fn load_model_provider_canary_evidence_for_proof(
    path: &Path,
    failures: &mut Vec<String>,
) -> Option<ModelProviderCanaryEvidence> {
    match load_model_provider_canary_evidence(path) {
        Ok(evidence) => Some(evidence),
        Err(error) => {
            failures.push(format!(
                "failed to load model provider evidence {}: {error}",
                path.display()
            ));
            None
        }
    }
}

fn load_remote_object_store_canary_evidence_for_proof(
    path: &Path,
    label: &str,
    failures: &mut Vec<String>,
) -> Option<crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence> {
    match load_remote_object_store_canary_evidence(path) {
        Ok(evidence) => Some(evidence),
        Err(error) => {
            failures.push(format!(
                "failed to load {label} remote object-store evidence {}: {error}",
                path.display()
            ));
            None
        }
    }
}

fn validate_canary_proof_timestamp_freshness(
    label: &str,
    generated_at_utc: &str,
    max_evidence_age_seconds: u64,
    failures: &mut Vec<String>,
) {
    let now_utc = timestamp_utc();
    let now_seconds = match parse_canary_proof_timestamp_seconds(&now_utc) {
        Ok(seconds) => seconds,
        Err(error) => {
            failures.push(format!("invalid canary proof reference time: {error}"));
            return;
        }
    };
    let generated_seconds = match parse_canary_proof_timestamp_seconds(generated_at_utc) {
        Ok(seconds) => seconds,
        Err(error) => {
            failures.push(format!("invalid {label} timestamp: {error}"));
            return;
        }
    };
    if generated_seconds > now_seconds {
        failures.push(format!(
            "{label} timestamp {generated_at_utc} is in the future relative to {now_utc}"
        ));
        return;
    }
    let age_seconds = now_seconds.saturating_sub(generated_seconds);
    if age_seconds > max_evidence_age_seconds {
        failures.push(format!(
            "{label} is stale: age {age_seconds}s exceeds max {max_evidence_age_seconds}s"
        ));
    }
}

fn parse_canary_proof_timestamp_seconds(value: &str) -> Result<u64, String> {
    let without_z = value
        .strip_suffix('Z')
        .ok_or_else(|| format!("timestamp {value} must end with Z"))?;
    let seconds = without_z
        .split_once('.')
        .map(|(seconds, _nanos)| seconds)
        .unwrap_or(without_z);
    seconds
        .parse::<u64>()
        .map_err(|error| format!("timestamp {value} has invalid seconds: {error}"))
}

fn validate_canary_workflow_provenance(
    workflow: &CanaryWorkflowProvenance,
    args: &CanaryProofArgs,
    failures: &mut Vec<String>,
) {
    if workflow.schema_version != CANARY_WORKFLOW_SCHEMA_VERSION {
        failures.push(format!(
            "unsupported canary workflow schema {}",
            workflow.schema_version
        ));
    }
    if workflow.event_name != "schedule" {
        failures.push(format!(
            "canary workflow event must be schedule for final proof, got {}",
            workflow.event_name
        ));
    }
    if workflow.workflow != args.expected_workflow {
        failures.push(format!(
            "canary workflow name must be {}, got {}",
            args.expected_workflow, workflow.workflow
        ));
    }
    if workflow.job != args.expected_job {
        failures.push(format!(
            "canary workflow job must be {}, got {}",
            args.expected_job, workflow.job
        ));
    }
    if let Some(expected_repository) = &args.expected_repository {
        if expected_repository.trim().is_empty() {
            failures.push("expected canary workflow repository must not be empty".to_string());
        } else if workflow.repository != *expected_repository {
            failures.push(format!(
                "canary workflow repository must be {}, got {}",
                expected_repository, workflow.repository
            ));
        }
    }
    if let Some(expected_git_ref) = &args.expected_git_ref {
        if expected_git_ref.trim().is_empty() {
            failures.push("expected canary workflow git ref must not be empty".to_string());
        } else if workflow.git_ref != *expected_git_ref {
            failures.push(format!(
                "canary workflow git ref must be {}, got {}",
                expected_git_ref, workflow.git_ref
            ));
        }
    }
    validate_non_empty_workflow_field("workflow", &workflow.workflow, failures);
    validate_non_empty_workflow_field("job", &workflow.job, failures);
    validate_numeric_workflow_field("runId", &workflow.run_id, failures);
    validate_numeric_workflow_field("runAttempt", &workflow.run_attempt, failures);
    validate_non_empty_workflow_field("repository", &workflow.repository, failures);
    validate_non_empty_workflow_field("gitRef", &workflow.git_ref, failures);
    validate_non_empty_workflow_field("actor", &workflow.actor, failures);
    if workflow.sha.len() != 40 || !workflow.sha.chars().all(|ch| ch.is_ascii_hexdigit()) {
        failures.push(format!(
            "canary workflow sha must be a 40-character hex commit, got {}",
            workflow.sha
        ));
    }
    if !is_http_or_https_url(&workflow.server_url) {
        failures.push(format!(
            "canary workflow server URL must be http:// or https://, got {}",
            workflow.server_url
        ));
    }
    if !is_http_or_https_url(&workflow.run_url) {
        failures.push(format!(
            "canary workflow run URL must be http:// or https://, got {}",
            workflow.run_url
        ));
    }
    let expected_prefix = format!("{}/", workflow.server_url.trim_end_matches('/'));
    if !workflow.run_url.starts_with(&expected_prefix) {
        failures.push(format!(
            "canary workflow run URL {} must start with server URL {}",
            workflow.run_url, expected_prefix
        ));
    }
    if !workflow.repository.trim().is_empty() && !workflow.run_id.trim().is_empty() {
        let expected_run_url = format!(
            "{}/{}/actions/runs/{}",
            workflow.server_url.trim_end_matches('/'),
            workflow.repository,
            workflow.run_id
        );
        if workflow.run_url != expected_run_url {
            failures.push(format!(
                "canary workflow run URL must be {}, got {}",
                expected_run_url, workflow.run_url
            ));
        }
    }
}

fn validate_non_empty_workflow_field(name: &str, value: &str, failures: &mut Vec<String>) {
    if value.trim().is_empty() {
        failures.push(format!("canary workflow {name} must not be empty"));
    }
}

fn validate_numeric_workflow_field(name: &str, value: &str, failures: &mut Vec<String>) {
    if value.trim().is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
        failures.push(format!(
            "canary workflow {name} must be numeric, got {value}"
        ));
    }
}

fn validate_canary_preflight_report(
    preflight: &CanaryPublicationPreflightReport,
    failures: &mut Vec<String>,
) {
    if preflight.schema_version != CANARY_PREFLIGHT_SCHEMA_VERSION {
        failures.push(format!(
            "unsupported canary preflight schema {}",
            preflight.schema_version
        ));
    }
    if !preflight.ok {
        failures.push("canary preflight report is not ok".to_string());
    }
    if preflight.config.provider_evidence_source != CanaryProviderEvidenceSource::LiveProviderCanary
    {
        failures.push(format!(
            "canary preflight provider evidence source must be live_provider_canary for scheduled proof, got {:?}",
            preflight.config.provider_evidence_source
        ));
    }
    if preflight.config.provider_evidence_input.is_some() {
        failures.push(
            "canary preflight provider evidence input must be absent for scheduled live proof"
                .to_string(),
        );
    }
    if preflight.config.object_store_driver != RemoteObjectStoreCanaryDriver::Http {
        failures.push(format!(
            "canary preflight object-store driver must be http for scheduled proof, got {:?}",
            preflight.config.object_store_driver
        ));
    }
    if preflight.config.output_dir.trim().is_empty() {
        failures.push("canary preflight output directory must not be empty".to_string());
    }
    if !is_http_or_https_url(&preflight.config.provider_base_url) {
        failures.push(format!(
            "canary preflight provider base URL must be http:// or https://, got {}",
            preflight.config.provider_base_url
        ));
    }
    if !is_http_or_https_url(&preflight.config.snapshot_base_uri) {
        failures.push(format!(
            "canary preflight snapshot base URI must be http:// or https://, got {}",
            preflight.config.snapshot_base_uri
        ));
    }
    if !is_http_or_https_url(&preflight.config.artifact_base_uri) {
        failures.push(format!(
            "canary preflight artifact base URI must be http:// or https://, got {}",
            preflight.config.artifact_base_uri
        ));
    }
    if preflight.config.model.trim().is_empty() {
        failures.push("canary preflight model must not be empty".to_string());
    }
    if preflight.config.max_output_tokens == 0 || preflight.config.max_output_tokens > 64 {
        failures.push(format!(
            "canary preflight max output tokens must be in 1..=64, got {}",
            preflight.config.max_output_tokens
        ));
    }
    if preflight.config.max_evidence_age_seconds == 0 {
        failures.push("canary preflight freshness window must be greater than zero".to_string());
    }
    for check in preflight
        .checks
        .iter()
        .filter(|check| check.status == CanaryPublicationPreflightStatus::Failed)
    {
        failures.push(format!(
            "canary preflight check {} failed: {}",
            check.name, check.message
        ));
    }
    validate_absent_preflight_check(preflight, "providerEvidence", failures);
    validate_required_preflight_check(
        preflight,
        "outputDir",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "providerCredential",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "providerModel",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "providerBaseUrl",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "snapshotBaseUri",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "artifactBaseUri",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "remoteObjectStoreAuth",
        &[
            CanaryPublicationPreflightStatus::Passed,
            CanaryPublicationPreflightStatus::Warning,
        ],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "maxOutputTokens",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
    validate_required_preflight_check(
        preflight,
        "maxEvidenceAgeSeconds",
        &[CanaryPublicationPreflightStatus::Passed],
        failures,
    );
}

fn validate_absent_preflight_check(
    preflight: &CanaryPublicationPreflightReport,
    name: &str,
    failures: &mut Vec<String>,
) {
    if preflight.checks.iter().any(|check| check.name == name) {
        failures.push(format!(
            "canary preflight check {name} must be absent for scheduled live proof"
        ));
    }
}

fn validate_required_preflight_check(
    preflight: &CanaryPublicationPreflightReport,
    name: &str,
    allowed_statuses: &[CanaryPublicationPreflightStatus],
    failures: &mut Vec<String>,
) {
    let matching = preflight
        .checks
        .iter()
        .filter(|check| check.name == name)
        .collect::<Vec<_>>();
    let [check] = matching.as_slice() else {
        failures.push(format!(
            "canary preflight must contain exactly one {name} check, got {}",
            matching.len()
        ));
        return;
    };
    if !allowed_statuses.contains(&check.status) {
        failures.push(format!(
            "canary preflight check {name} must have status {:?}, got {:?}: {}",
            allowed_statuses, check.status, check.message
        ));
    }
}

struct CanaryPreflightConsistencyContext<'a> {
    publication: Option<&'a CanaryPublicationReport>,
    status: Option<&'a crate::reviewer::canaries::CanaryEvidenceStatusReport>,
    provider: Option<&'a ModelProviderCanaryEvidence>,
    snapshot: Option<&'a crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence>,
    artifact: Option<&'a crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence>,
    max_evidence_age_seconds: u64,
}

fn validate_canary_preflight_consistency(
    preflight: Option<&CanaryPublicationPreflightReport>,
    context: CanaryPreflightConsistencyContext<'_>,
    failures: &mut Vec<String>,
) {
    let Some(preflight) = preflight else {
        return;
    };
    let config = &preflight.config;
    if config.max_evidence_age_seconds != context.max_evidence_age_seconds {
        failures.push(format!(
            "preflight freshness window {} does not match proof freshness window {}",
            config.max_evidence_age_seconds, context.max_evidence_age_seconds
        ));
    }
    if let Some(publication) = context.publication {
        if config.provider_evidence_source != publication.provider_evidence_source {
            failures.push(format!(
                "preflight provider evidence source {:?} does not match publication source {:?}",
                config.provider_evidence_source, publication.provider_evidence_source
            ));
        }
        if config.provider_evidence_input != publication.provider_evidence_input {
            failures.push(format!(
                "preflight provider evidence input {:?} does not match publication input {:?}",
                config.provider_evidence_input, publication.provider_evidence_input
            ));
        }
        if config.object_store_driver != publication.object_store_driver {
            failures.push(format!(
                "preflight object-store driver {:?} does not match publication driver {:?}",
                config.object_store_driver, publication.object_store_driver
            ));
        }
        if config.model != publication.model {
            failures.push(format!(
                "preflight model {} does not match publication model {}",
                config.model, publication.model
            ));
        }
        if config.max_evidence_age_seconds != publication.max_evidence_age_seconds {
            failures.push(format!(
                "preflight freshness window {} does not match publication freshness window {}",
                config.max_evidence_age_seconds, publication.max_evidence_age_seconds
            ));
        }
        if let Some(publication_base_url) = publication.provider_base_url.as_deref() {
            if config.provider_base_url != publication_base_url {
                failures.push(format!(
                    "preflight provider base URL {} does not match publication provider base URL {}",
                    config.provider_base_url, publication_base_url
                ));
            }
        }
    }
    if let Some(provider) = context.provider {
        for report in &provider.reports {
            if report.model != config.model {
                failures.push(format!(
                    "provider report for {:?} uses model {}, expected preflight model {}",
                    report.protocol, report.model, config.model
                ));
            }
            if report.base_url != config.provider_base_url {
                failures.push(format!(
                    "provider report for {:?} uses base URL {}, expected preflight base URL {}",
                    report.protocol, report.base_url, config.provider_base_url
                ));
            }
        }
    }
    if let Some(snapshot) = context.snapshot {
        validate_preflight_remote_base_uri(
            "snapshot",
            &config.snapshot_base_uri,
            &snapshot.base_uri,
            failures,
        );
    }
    if let Some(artifact) = context.artifact {
        validate_preflight_remote_base_uri(
            "artifact",
            &config.artifact_base_uri,
            &artifact.base_uri,
            failures,
        );
    }
    if let Some(status) = context.status {
        validate_preflight_status_remote_base_uri(
            status,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Snapshot,
            &config.snapshot_base_uri,
            failures,
        );
        validate_preflight_status_remote_base_uri(
            status,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Artifact,
            &config.artifact_base_uri,
            failures,
        );
    }
}

fn validate_preflight_remote_base_uri(
    label: &str,
    preflight_base_uri: &str,
    evidence_base_uri: &str,
    failures: &mut Vec<String>,
) {
    if preflight_base_uri != evidence_base_uri {
        failures.push(format!(
            "preflight {label} base URI {preflight_base_uri} does not match {label} evidence base URI {evidence_base_uri}"
        ));
    }
}

fn validate_preflight_status_remote_base_uri(
    status: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
    target: crate::reviewer::canaries::RemoteObjectStoreCanaryTarget,
    preflight_base_uri: &str,
    failures: &mut Vec<String>,
) {
    let target_name = canary_remote_target_name(target);
    let Some(remote_status) = status
        .evidence
        .remote_object_stores
        .iter()
        .find(|remote_status| remote_status.target == target)
    else {
        return;
    };
    if let Some(status_base_uri) = remote_status.base_uri.as_deref() {
        if preflight_base_uri != status_base_uri {
            failures.push(format!(
                "preflight {target_name} base URI {preflight_base_uri} does not match status base URI {status_base_uri}"
            ));
        }
    }
}

fn validate_canary_publication_proof_report(
    publication: &CanaryPublicationReport,
    args: &CanaryProofArgs,
    expected_files: &CanaryProofFiles,
    failures: &mut Vec<String>,
) {
    if publication.schema_version != CANARY_PUBLICATION_SCHEMA_VERSION {
        failures.push(format!(
            "unsupported canary publication schema {}",
            publication.schema_version
        ));
    }
    if publication.provider_evidence_source != CanaryProviderEvidenceSource::LiveProviderCanary {
        failures.push(format!(
            "provider evidence source must be live_provider_canary for scheduled proof, got {:?}",
            publication.provider_evidence_source
        ));
    }
    if publication.provider_evidence_input.is_some() {
        failures
            .push("provider evidence input must be absent for scheduled live proof".to_string());
    }
    if publication.object_store_driver != RemoteObjectStoreCanaryDriver::Http {
        failures.push(format!(
            "object-store canary driver must be http for scheduled proof, got {:?}",
            publication.object_store_driver
        ));
    }
    if publication.max_evidence_age_seconds != args.max_evidence_age_seconds {
        failures.push(format!(
            "publication freshness window {} does not match proof freshness window {}",
            publication.max_evidence_age_seconds, args.max_evidence_age_seconds
        ));
    }
    if !publication.status_ok {
        failures.push("canary publication status_ok is false".to_string());
    }
    if !publication.failures.is_empty() {
        failures.push(format!(
            "canary publication contains status failures: {}",
            publication.failures.join("; ")
        ));
    }
    validate_canary_publication_file(
        "provider evidence",
        &publication.files.provider_evidence,
        &expected_files.provider_evidence,
        failures,
    );
    validate_canary_publication_file(
        "snapshot object-store evidence",
        &publication.files.snapshot_object_store_evidence,
        &expected_files.snapshot_object_store_evidence,
        failures,
    );
    validate_canary_publication_file(
        "artifact object-store evidence",
        &publication.files.artifact_object_store_evidence,
        &expected_files.artifact_object_store_evidence,
        failures,
    );
    validate_canary_publication_file(
        "manifest",
        &publication.files.manifest,
        &expected_files.manifest,
        failures,
    );
    validate_canary_publication_file(
        "status",
        &publication.files.status,
        &expected_files.status,
        failures,
    );
}

fn validate_canary_publication_file(
    label: &str,
    actual: &str,
    expected: &str,
    failures: &mut Vec<String>,
) {
    if actual != expected {
        failures.push(format!(
            "publication {label} file must be {expected}, got {actual}"
        ));
    }
}

fn validate_canary_status_proof_report(
    status: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
    args: &CanaryProofArgs,
    failures: &mut Vec<String>,
) {
    if status.max_evidence_age_seconds != args.max_evidence_age_seconds {
        failures.push(format!(
            "status freshness window {} does not match proof freshness window {}",
            status.max_evidence_age_seconds, args.max_evidence_age_seconds
        ));
    }
    if !status.ok {
        failures.push("canary status report is not ok".to_string());
    }
    let status_failures = status.failures();
    if !status_failures.is_empty() {
        failures.push(format!(
            "canary status report contains failures: {}",
            status_failures.join("; ")
        ));
    }
    if !status.evidence.model_provider.present {
        failures.push("status report is missing model provider evidence".to_string());
    }
    if status.evidence.model_provider.passed_protocols
        != status.evidence.model_provider.required_protocols
    {
        failures.push(format!(
            "status report model provider passed protocols {:?} do not match required protocols {:?}",
            status.evidence.model_provider.passed_protocols,
            status.evidence.model_provider.required_protocols
        ));
    }
    validate_canary_remote_status(
        status,
        crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Snapshot,
        failures,
    );
    validate_canary_remote_status(
        status,
        crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Artifact,
        failures,
    );
}

fn validate_canary_remote_status(
    status: &crate::reviewer::canaries::CanaryEvidenceStatusReport,
    target: crate::reviewer::canaries::RemoteObjectStoreCanaryTarget,
    failures: &mut Vec<String>,
) {
    let target_name = canary_remote_target_name(target);
    let Some(remote_status) = status
        .evidence
        .remote_object_stores
        .iter()
        .find(|remote_status| remote_status.target == target)
    else {
        failures.push(format!(
            "status report is missing {target_name} remote object-store evidence summary"
        ));
        return;
    };
    if remote_status.evidence_count != 1 {
        failures.push(format!(
            "status report must contain exactly one {target_name} remote object-store evidence entry, got {}",
            remote_status.evidence_count
        ));
    }
    if !remote_status
        .gate
        .as_ref()
        .is_some_and(|gate| gate.valid && gate.failures.is_empty())
    {
        failures.push(format!(
            "status report {target_name} remote object-store gate is not valid"
        ));
    }
    match remote_status.base_uri.as_deref() {
        Some(base_uri) if is_http_or_https_url(base_uri) => {}
        Some(base_uri) => failures.push(format!(
            "status report {target_name} remote object-store base URI must be http:// or https://, got {base_uri}"
        )),
        None => failures.push(format!(
            "status report {target_name} remote object-store base URI is missing"
        )),
    }
}

fn validate_canary_manifest_status_consistency(
    manifest: &CanaryEvidenceManifest,
    status: Option<&crate::reviewer::canaries::CanaryEvidenceStatusReport>,
    failures: &mut Vec<String>,
) {
    let Some(status) = status else {
        return;
    };
    if status.manifest_schema_version != manifest.schema_version {
        failures.push(format!(
            "status manifest schema {} does not match manifest schema {}",
            status.manifest_schema_version, manifest.schema_version
        ));
    }
    if status.generated_at_utc != manifest.generated_at_utc {
        failures.push(format!(
            "status generated_at_utc {} does not match manifest generated_at_utc {}",
            status.generated_at_utc, manifest.generated_at_utc
        ));
    }
    if status.gate != manifest.gate {
        failures.push("status gate does not match manifest gate".to_string());
    }
    let recomputed = manifest.status_report(&CanaryEvidenceFreshnessPolicy::at(
        &status.freshness_checked_at_utc,
        status.max_evidence_age_seconds,
    ));
    if status.evidence != recomputed.evidence {
        failures.push("status evidence summary does not match manifest evidence".to_string());
    }
    if status.validation_failures != recomputed.validation_failures {
        failures.push("status validation failures do not match manifest validation".to_string());
    }
}

fn validate_model_provider_canary_proof(
    provider: &ModelProviderCanaryEvidence,
    publication: Option<&CanaryPublicationReport>,
    failures: &mut Vec<String>,
) {
    if let Err(error) = provider.require_passed() {
        failures.push(format!(
            "model provider evidence failed proof validation: {error}"
        ));
    }
    let Some(publication) = publication else {
        return;
    };
    for report in &provider.reports {
        if report.model != publication.model {
            failures.push(format!(
                "model provider report for {:?} uses model {}, expected {}",
                report.protocol, report.model, publication.model
            ));
        }
        if let Some(provider_base_url) = publication.provider_base_url.as_deref() {
            if report.base_url != provider_base_url {
                failures.push(format!(
                    "model provider report for {:?} uses base URL {}, expected {}",
                    report.protocol, report.base_url, provider_base_url
                ));
            }
        }
    }
}

fn validate_remote_object_store_canary_proof(
    evidence: &crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence,
    expected_target: crate::reviewer::canaries::RemoteObjectStoreCanaryTarget,
    failures: &mut Vec<String>,
) {
    let expected_name = canary_remote_target_name(expected_target);
    if evidence.target != expected_target {
        failures.push(format!(
            "{expected_name} remote object-store evidence has target {:?}",
            evidence.target
        ));
    }
    if let Err(error) = evidence.require_passed() {
        failures.push(format!(
            "{expected_name} remote object-store evidence failed proof validation: {error}"
        ));
    }
    if !is_http_or_https_url(&evidence.base_uri) {
        failures.push(format!(
            "{expected_name} remote object-store evidence base URI must be http:// or https://, got {}",
            evidence.base_uri
        ));
    }
}

fn validate_canary_child_files_match_manifest(
    manifest: Option<&CanaryEvidenceManifest>,
    provider: Option<&ModelProviderCanaryEvidence>,
    snapshot: Option<&crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence>,
    artifact: Option<&crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence>,
    failures: &mut Vec<String>,
) {
    let Some(manifest) = manifest else {
        return;
    };
    if let Some(provider) = provider {
        match manifest.model_provider.as_ref() {
            Some(manifest_provider) if manifest_provider == provider => {}
            Some(_) => failures.push(
                "manifest model provider evidence does not match model-provider.json".to_string(),
            ),
            None => failures.push("manifest is missing model provider evidence".to_string()),
        }
    }
    if let Some(snapshot) = snapshot {
        validate_manifest_remote_evidence_match(
            manifest,
            snapshot,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Snapshot,
            failures,
        );
    }
    if let Some(artifact) = artifact {
        validate_manifest_remote_evidence_match(
            manifest,
            artifact,
            crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Artifact,
            failures,
        );
    }
}

fn validate_manifest_remote_evidence_match(
    manifest: &CanaryEvidenceManifest,
    evidence: &crate::reviewer::canaries::RemoteObjectStoreCanaryEvidence,
    target: crate::reviewer::canaries::RemoteObjectStoreCanaryTarget,
    failures: &mut Vec<String>,
) {
    let target_name = canary_remote_target_name(target);
    let matching = manifest
        .remote_object_stores
        .iter()
        .filter(|manifest_evidence| manifest_evidence.target == target)
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [manifest_evidence] if *manifest_evidence == evidence => {}
        [_] => failures.push(format!(
            "manifest {target_name} remote object-store evidence does not match child evidence file"
        )),
        [] => failures.push(format!(
            "manifest is missing {target_name} remote object-store evidence"
        )),
        matching => failures.push(format!(
            "manifest contains {} {target_name} remote object-store evidence entries",
            matching.len()
        )),
    }
}

fn canary_remote_target_name(
    target: crate::reviewer::canaries::RemoteObjectStoreCanaryTarget,
) -> &'static str {
    match target {
        crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Snapshot => "snapshot",
        crate::reviewer::canaries::RemoteObjectStoreCanaryTarget::Artifact => "artifact",
    }
}

fn is_http_or_https_url(value: &str) -> bool {
    reqwest::Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn canary_proof_report_json(report: &CanaryProofReport) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_canary_proof_report(path: &Path, report: &CanaryProofReport) -> Result<usize> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create canary proof directory {}",
                parent.display()
            )
        })?;
    }
    let bytes = canary_proof_report_json(report)?;
    fs::write(path, &bytes)
        .with_context(|| format!("failed to write canary proof {}", path.display()))?;
    Ok(bytes.len())
}

#[cfg(test)]
mod context_cli_tests {
    use super::*;

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
        assert_eq!(
            config.weight_path_proximity,
            ContextEngineConfig::snapshot_v0().weight_path_proximity
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
    }
}
