use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeScopeV1 {
    pub(crate) kind: ChangeKind,
    pub(crate) change_id: String,
    pub(crate) source_ref: String,
    pub(crate) target_ref: String,
    pub(crate) base_revision_id: String,
    pub(crate) head_revision_id: String,
    pub(crate) merge_base_revision_id: Option<String>,
    pub(crate) changed_files_manifest_ref: Option<String>,
    pub(crate) diff_manifest_ref: Option<String>,
    #[serde(default)]
    pub(crate) inline_diff: Option<String>,
    pub(crate) snapshot_mode: SnapshotMode,
    pub(crate) rename_detection: RenameDetection,
    pub(crate) changed_files: Vec<ChangedFileEntryV1>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    PullRequest,
    MergeRequest,
    LocalDiff,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotMode {
    WorktreeHead,
    BaseHeadManifests,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RenameDetection {
    None,
    AppManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangedFileEntryV1 {
    pub(crate) status: ChangedFileStatus,
    pub(crate) old_path: Option<PathBuf>,
    pub(crate) new_path: Option<PathBuf>,
    pub(crate) old_content_hash: Option<String>,
    pub(crate) new_content_hash: Option<String>,
    pub(crate) is_binary: bool,
    pub(crate) is_generated: bool,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangedFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelProfileRefV1 {
    pub(crate) id: String,
    pub(crate) provider_kind: ProviderKind,
    #[serde(default)]
    pub(crate) api_protocol: ModelApiProtocol,
    pub(crate) provider_profile_id: String,
    pub(crate) credential_ref: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
    pub(crate) max_input_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) tool_calling_mode: ToolCallingMode,
    pub(crate) temperature: Option<f32>,
    pub(crate) top_p: Option<f32>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    OpenaiCompatible,
    Anthropic,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelApiProtocol {
    Responses,
    /// The Anthropic Messages API (`POST /v1/messages`).
    Messages,
}

impl Default for ModelApiProtocol {
    fn default() -> Self {
        Self::Responses
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolCallingMode {
    Required,
    Auto,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Generalist,
    Security,
    Performance,
    Maintainability,
    Correctness,
    Architecture,
    Validator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PathPolicyV1 {
    pub(crate) allowed_roots: Vec<PathBuf>,
    pub(crate) denied_globs: Vec<String>,
    pub(crate) allowed_globs: Option<Vec<String>>,
    pub(crate) allow_dot_git: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) max_file_bytes: usize,
    pub(crate) max_diff_bytes: usize,
    pub(crate) max_search_results: usize,
    pub(crate) max_directory_entries: usize,
}

impl PathPolicyV1 {
    pub(crate) fn bench(max_file_kb: usize, max_search_matches: usize) -> Self {
        Self {
            allowed_roots: vec![PathBuf::from(".")],
            denied_globs: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
                ".venv".to_string(),
                "dist".to_string(),
                "build".to_string(),
                ".next".to_string(),
            ],
            allowed_globs: None,
            allow_dot_git: false,
            follow_symlinks: false,
            max_file_bytes: max_file_kb * 1024,
            max_diff_bytes: max_file_kb * 1024,
            max_search_results: max_search_matches,
            max_directory_entries: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBudget {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_prompt_tokens: u64,
    pub max_output_tokens: u64,
    #[serde(default)]
    pub budget_source: BudgetSource,
}

impl AgentBudget {
    pub fn planned_baseline() -> Self {
        Self {
            max_turns: 10,
            max_tool_calls: 32,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: BudgetSource::PlannedDefault,
        }
    }

    pub fn planned_high_risk() -> Self {
        Self {
            max_turns: 14,
            max_tool_calls: 64,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: BudgetSource::AdaptiveReview,
        }
    }

    pub fn planned_secondary_lens() -> Self {
        Self {
            max_turns: 6,
            max_tool_calls: 20,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: BudgetSource::AdaptiveReview,
        }
    }

    pub fn planned_high_value_secondary_lens() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 32,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: BudgetSource::AdaptiveReview,
        }
    }

    pub fn planned_challenge() -> Self {
        Self {
            max_turns: 4,
            max_tool_calls: 16,
            max_prompt_tokens: 64_000,
            max_output_tokens: 8_000,
            budget_source: BudgetSource::RunReserve,
        }
    }

    pub fn caller_hard_cap(
        max_turns: usize,
        max_tool_calls: usize,
        max_prompt_tokens: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            max_turns,
            max_tool_calls,
            max_prompt_tokens,
            max_output_tokens,
            budget_source: BudgetSource::CallerHardCap,
        }
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSource {
    CallerHardCap,
    #[default]
    PlannedDefault,
    AdaptiveReview,
    RunReserve,
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCoverage {
    Full,
    Standard,
    #[default]
    Sampled,
    Insufficient,
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    #[default]
    Clean,
    IssueFound,
    NeedsReview,
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeStatus {
    Confirmed,
    Refuted,
    Insufficient,
    #[default]
    NotRun,
    Incomplete,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RedactionMetadataV1 {
    pub(crate) redaction_state: RedactionState,
    pub(crate) redaction_policy_id: String,
    pub(crate) contains_repo_content: bool,
    pub(crate) contains_prompt_content: bool,
    pub(crate) contains_model_output: bool,
    pub(crate) contains_secret_material: bool,
}

#[derive(Debug, Copy, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum RedactionState {
    None,
    Partial,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileReviewV1 {
    pub path: String,
    pub verdict: String,
    #[serde(default)]
    pub coverage: ReviewCoverage,
    #[serde(default)]
    pub review_verdict: ReviewVerdict,
    pub summary: String,
    #[serde(default)]
    pub related_paths: Vec<String>,
    #[serde(default)]
    pub evidence_artifact_ids: Vec<String>,
    pub evidence_count: usize,
    pub session_id: String,
    pub unit_id: String,
}

#[derive(Debug, Copy, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ReviewOutcomeV1 {
    CompletedNoFindings,
    CompletedWithFindings,
    BudgetExhaustedPartial,
    CancelledPartial,
    FailedPartial,
}

#[derive(Debug, Copy, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum Publishability {
    Publishable,
    DiagnosticOnly,
    NotPublishable,
}

#[derive(Debug, Copy, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ArtifactKind {
    FileSlice,
    DiffHunk,
    SearchResults,
    FileList,
    ChangedFileList,
    ImportSummary,
    ToolSummary,
    RedactedView,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvidenceRefV1 {
    pub(crate) evidence_id: String,
    pub(crate) artifact_id: String,
    pub(crate) kind: ArtifactKind,
    pub(crate) revision: EvidenceRevision,
    pub(crate) revision_id: String,
    pub(crate) location: EvidenceLocationV1,
    pub(crate) line_range: Option<LineRangeV1>,
    pub(crate) byte_range: Option<ByteRangeV1>,
    pub(crate) diff_anchor: Option<DiffAnchorV1>,
    pub(crate) content_hash: String,
    pub(crate) redaction: RedactionMetadataV1,
    pub(crate) producing_tool_call_id: String,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum EvidenceRevision {
    Base,
    Head,
    MergeBase,
    Review,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "locationKind", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum EvidenceLocationV1 {
    SinglePath { path: String },
    Rename { old_path: String, new_path: String },
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LineRangeV1 {
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ByteRangeV1 {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiffAnchorV1 {
    pub(crate) hunk_id: String,
    pub(crate) side: DiffSide,
    pub(crate) old_start: Option<usize>,
    pub(crate) old_lines: Option<usize>,
    pub(crate) new_start: Option<usize>,
    pub(crate) new_lines: Option<usize>,
    pub(crate) patch_hash: String,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum DiffSide {
    Base,
    Head,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FindingV1 {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) claim: String,
    pub(crate) severity: FindingSeverity,
    pub(crate) confidence: f32,
    pub(crate) validation_status: ValidationStatus,
    pub(crate) report_status: ReportStatus,
    pub(crate) publishability: FindingPublishability,
    pub(crate) challenge_status: ChallengeStatus,
    pub(crate) evidence: Vec<EvidenceRefV1>,
    pub(crate) file_refs: Vec<EvidenceLocationV1>,
    pub(crate) location_line_range: Option<LineRangeV1>,
    pub(crate) discovered_by: Vec<String>,
    pub(crate) challenged_by: Vec<String>,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum FindingSeverity {
    Blocker,
    High,
    Medium,
    Low,
    Nit,
}

#[derive(Debug, Copy, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ValidationStatus {
    Candidate,
    Challenged,
    Validated,
    Rejected,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportStatus {
    Included,
    Suppressed,
}

#[derive(Debug, Copy, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingPublishability {
    Publishable,
    NotPublishable,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    ListChangedFiles,
    ReadDiff,
    ListFiles,
    ReadFile,
    ReadFileRange,
    ReadBaseFile,
    ReadHeadFile,
    SearchText,
    FindRelatedFiles,
    FindTestsForFile,
    ListImports,
}

impl ToolName {
    pub const REVIEW_READ_ONLY: [Self; 11] = [
        Self::ListChangedFiles,
        Self::ReadDiff,
        Self::ListFiles,
        Self::ReadFile,
        Self::ReadFileRange,
        Self::ReadBaseFile,
        Self::ReadHeadFile,
        Self::SearchText,
        Self::FindRelatedFiles,
        Self::FindTestsForFile,
        Self::ListImports,
    ];

    pub fn review_read_only_tools() -> &'static [Self] {
        &Self::REVIEW_READ_ONLY
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ListChangedFiles => "list_changed_files",
            Self::ReadDiff => "read_diff",
            Self::ListFiles => "list_files",
            Self::ReadFile => "read_file",
            Self::ReadFileRange => "read_file_range",
            Self::ReadBaseFile => "read_base_file",
            Self::ReadHeadFile => "read_head_file",
            Self::SearchText => "search_text",
            Self::FindRelatedFiles => "find_related_files",
            Self::FindTestsForFile => "find_tests_for_file",
            Self::ListImports => "list_imports",
        }
    }
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Input tokens the provider served from its prompt cache (OpenAI
    /// `prompt_tokens_details.cached_tokens`, Anthropic
    /// `cache_read_input_tokens`). Subset of `input_tokens`; billed at a
    /// discount, so this is the visibility needed to judge real prompt cost.
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

#[derive(Debug, Default, Copy, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCounts {
    pub list_changed_files: usize,
    pub read_diff: usize,
    pub list_files: usize,
    pub read_file: usize,
    pub read_file_range: usize,
    pub read_base_file: usize,
    pub read_head_file: usize,
    pub search_text: usize,
    pub find_related_files: usize,
    pub find_tests_for_file: usize,
    pub list_imports: usize,
}

impl ToolCounts {
    pub fn add(&mut self, other: ToolCounts) {
        self.list_changed_files += other.list_changed_files;
        self.read_diff += other.read_diff;
        self.list_files += other.list_files;
        self.read_file += other.read_file;
        self.read_file_range += other.read_file_range;
        self.read_base_file += other.read_base_file;
        self.read_head_file += other.read_head_file;
        self.search_text += other.search_text;
        self.find_related_files += other.find_related_files;
        self.find_tests_for_file += other.find_tests_for_file;
        self.list_imports += other.list_imports;
    }

    pub fn increment(&mut self, tool: ToolName) {
        match tool {
            ToolName::ListChangedFiles => self.list_changed_files += 1,
            ToolName::ReadDiff => self.read_diff += 1,
            ToolName::ListFiles => self.list_files += 1,
            ToolName::ReadFile => self.read_file += 1,
            ToolName::ReadFileRange => self.read_file_range += 1,
            ToolName::ReadBaseFile => self.read_base_file += 1,
            ToolName::ReadHeadFile => self.read_head_file += 1,
            ToolName::SearchText => self.search_text += 1,
            ToolName::FindRelatedFiles => self.find_related_files += 1,
            ToolName::FindTestsForFile => self.find_tests_for_file += 1,
            ToolName::ListImports => self.list_imports += 1,
        }
    }

    pub fn total(self) -> usize {
        self.list_changed_files
            + self.read_diff
            + self.list_files
            + self.read_file
            + self.read_file_range
            + self.read_base_file
            + self.read_head_file
            + self.search_text
            + self.find_related_files
            + self.find_tests_for_file
            + self.list_imports
    }
}
