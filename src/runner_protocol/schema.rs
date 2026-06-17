use super::schema_types::{
    RunnerCapabilities, RunnerCheckResult, RunnerHandshakeResult, RunnerMessageDirection,
    RunnerMethodSchema, RunnerMethodStatus, RunnerPayloadFieldSchema, RunnerPayloadRef,
    RunnerPayloadSchema, RunnerPayloadShape, RunnerProtocolSchema,
};
use super::{RUNNER_NAME, RUNNER_PROTOCOL_VERSION};

pub fn runner_handshake() -> RunnerHandshakeResult {
    RunnerHandshakeResult {
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        runner_name: RUNNER_NAME.to_string(),
        runner_version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: RunnerCapabilities {
            supported_methods: supported_methods(),
            planned_methods: Vec::new(),
            transports: vec!["stdio-jsonl".to_string()],
        },
    }
}

pub fn runner_check() -> RunnerCheckResult {
    RunnerCheckResult {
        ok: true,
        protocol_version: RUNNER_PROTOCOL_VERSION.to_string(),
        runner_name: RUNNER_NAME.to_string(),
        runner_version: env!("CARGO_PKG_VERSION").to_string(),
        rust_package: env!("CARGO_PKG_NAME").to_string(),
    }
}

pub fn protocol_schema() -> RunnerProtocolSchema {
    RunnerProtocolSchema {
        schema_version: RUNNER_PROTOCOL_VERSION.to_string(),
        transport: "newline-delimited JSON-RPC 2.0 over stdio".to_string(),
        requests: method_schemas(SDK_TO_RUNNER_METHODS),
        callbacks: method_schemas(RUNNER_TO_SDK_CALLBACKS),
        notifications: method_schemas(RUNNER_TO_SDK_NOTIFICATIONS),
        definitions: payload_definitions(),
    }
}

#[derive(Debug, Copy, Clone)]
struct MethodSpec {
    method: &'static str,
    direction: RunnerMessageDirection,
    summary: &'static str,
    params: Option<&'static str>,
    result: Option<&'static str>,
}

const SDK_TO_RUNNER_METHODS: &[MethodSpec] = &[
    sdk_to_runner(
        "runner.handshake",
        "Negotiate protocol version and capabilities.",
        Some("RunnerHandshakeParams"),
        Some("RunnerHandshakeResult"),
    ),
    sdk_to_runner(
        "runner.check",
        "Return local runner diagnostics.",
        None,
        Some("RunnerCheckResult"),
    ),
    sdk_to_runner(
        "runner.schema.export",
        "Return protocol method metadata for SDK validation.",
        None,
        Some("RunnerProtocolSchema"),
    ),
    sdk_to_runner(
        "run.start",
        "Start a review run.",
        Some("RunStartParams"),
        Some("RunnerRunResult"),
    ),
    sdk_to_runner(
        "run.cancel",
        "Cancel an active review run.",
        Some("RunLookupParams"),
        Some("RunCancelResult"),
    ),
    sdk_to_runner(
        "run.status",
        "Read active run status.",
        Some("RunLookupParams"),
        Some("RunStatusResult"),
    ),
    sdk_to_runner(
        "run.result",
        "Read final run report.",
        Some("RunLookupParams"),
        Some("RunnerRunResult"),
    ),
    sdk_to_runner(
        "artifact.read",
        "Read one redacted or raw artifact.",
        Some("ArtifactReadParams"),
        Some("RunnerArtifactReadResult"),
    ),
    sdk_to_runner(
        "artifact.export",
        "Export artifacts using a policy.",
        Some("ArtifactExportParams"),
        Some("RunnerArtifactExportResult"),
    ),
    sdk_to_runner(
        "snapshot.readText",
        "Read captured snapshot text.",
        Some("SnapshotReadTextParams"),
        Some("RunnerSnapshotTextResult"),
    ),
    sdk_to_runner(
        "webhook.github.handle",
        "Verify and schedule a GitHub webhook delivery.",
        Some("WebhookHandleParams"),
        Some("ReviewHttpResponse"),
    ),
    sdk_to_runner(
        "webhook.gitlab.handle",
        "Verify and schedule a GitLab webhook delivery.",
        Some("WebhookHandleParams"),
        Some("ReviewHttpResponse"),
    ),
    sdk_to_runner(
        "worker.runOnce",
        "Claim and execute ready durable review sessions through the Rust worker.",
        Some("WorkerRunOnceParams"),
        Some("WorkerRunOnceResult"),
    ),
    sdk_to_runner(
        "context.index",
        "Index a repository snapshot outside a review run.",
        Some("ContextIndexParams"),
        Some("ContextManifest"),
    ),
    sdk_to_runner(
        "context.pack",
        "Build a role-scoped context pack from a previously indexed snapshot.",
        Some("ContextPackRequest"),
        Some("ContextPack"),
    ),
    sdk_to_runner(
        "context.query",
        "Query an indexed snapshot or context pack outside agent execution.",
        Some("ContextQuery"),
        Some("ContextQueryResult"),
    ),
    sdk_to_runner(
        "context.feedback",
        "Record feedback as a proposed context learning for an indexed snapshot.",
        Some("ContextFeedback"),
        Some("ContextFeedbackReceipt"),
    ),
    sdk_to_runner(
        "context.learning.approve",
        "Approve or reject a proposed context learning for an indexed snapshot.",
        Some("ContextLearningApprovalParams"),
        Some("ContextLearningApprovalReceipt"),
    ),
];

const RUNNER_TO_SDK_CALLBACKS: &[MethodSpec] = &[
    runner_to_sdk(
        "source.materialize",
        "Ask the SDK source provider to materialize a review source.",
        Some("SourceMaterializeParams"),
        Some("SourceMaterializeResult"),
    ),
    runner_to_sdk(
        "run.heartbeat",
        "Ask the SDK host to renew an active run lease.",
        Some("RunHeartbeatParams"),
        Some("RunHeartbeatResult"),
    ),
    runner_to_sdk(
        "model.complete",
        "Ask the SDK model adapter for one model turn.",
        Some("RunnerModelCompleteParams"),
        Some("RunnerModelCompleteResult"),
    ),
    runner_to_sdk(
        "secret.resolve",
        "Ask the SDK host to resolve one secret reference.",
        Some("RunnerSecretResolveParams"),
        Some("RunnerSecretResolveResult"),
    ),
    runner_to_sdk(
        "tool.execute",
        "Ask the SDK to execute a host custom tool.",
        Some("RunnerToolExecuteParams"),
        Some("RunnerToolExecuteResult"),
    ),
];

const RUNNER_TO_SDK_NOTIFICATIONS: &[MethodSpec] = &[
    runner_to_sdk(
        "event.review",
        "Emit one host-facing review event.",
        Some("ReviewEventRecord"),
        None,
    ),
    runner_to_sdk(
        "event.runtime",
        "Emit one advanced runtime event.",
        Some("RuntimeEventRecord"),
        None,
    ),
    runner_to_sdk(
        "run.finished",
        "Notify that a run reached a terminal state.",
        Some("RunnerRunResult"),
        None,
    ),
    runner_to_sdk(
        "run.failed",
        "Notify that a run failed before producing a report.",
        Some("RunFailedNotification"),
        None,
    ),
];

const fn sdk_to_runner(
    method: &'static str,
    summary: &'static str,
    params: Option<&'static str>,
    result: Option<&'static str>,
) -> MethodSpec {
    MethodSpec {
        method,
        direction: RunnerMessageDirection::SdkToRunner,
        summary,
        params,
        result,
    }
}

const fn runner_to_sdk(
    method: &'static str,
    summary: &'static str,
    params: Option<&'static str>,
    result: Option<&'static str>,
) -> MethodSpec {
    MethodSpec {
        method,
        direction: RunnerMessageDirection::RunnerToSdk,
        summary,
        params,
        result,
    }
}

fn supported_methods() -> Vec<String> {
    base_sdk_methods()
        .chain(context_sdk_methods())
        .chain(host_sdk_methods())
        .chain(RUNNER_TO_SDK_CALLBACKS.iter())
        .chain(RUNNER_TO_SDK_NOTIFICATIONS.iter())
        .map(|spec| spec.method.to_string())
        .collect()
}

fn base_sdk_methods() -> impl Iterator<Item = &'static MethodSpec> {
    SDK_TO_RUNNER_METHODS
        .iter()
        .filter(|spec| !is_context_method(spec.method) && !is_host_method(spec.method))
}

fn context_sdk_methods() -> impl Iterator<Item = &'static MethodSpec> {
    SDK_TO_RUNNER_METHODS
        .iter()
        .filter(|spec| is_context_method(spec.method))
}

fn host_sdk_methods() -> impl Iterator<Item = &'static MethodSpec> {
    SDK_TO_RUNNER_METHODS
        .iter()
        .filter(|spec| is_host_method(spec.method))
}

fn is_context_method(method: &str) -> bool {
    method.starts_with("context.")
}

fn is_host_method(method: &str) -> bool {
    method.starts_with("webhook.") || method == "worker.runOnce"
}

fn method_schemas(specs: &[MethodSpec]) -> Vec<RunnerMethodSchema> {
    specs.iter().map(method_schema).collect()
}

fn method_schema(spec: &MethodSpec) -> RunnerMethodSchema {
    RunnerMethodSchema {
        method: spec.method.to_string(),
        direction: spec.direction,
        status: RunnerMethodStatus::Implemented,
        summary: spec.summary.to_string(),
        params: spec.params.map(payload_ref),
        result: spec.result.map(payload_ref),
    }
}

fn payload_definitions() -> Vec<RunnerPayloadSchema> {
    vec![
        object(
            "RunnerHandshakeParams",
            vec![
                required("protocolVersion", "string"),
                optional("clientName", "string"),
                optional("clientVersion", "string"),
            ],
        ),
        object(
            "RunnerHandshakeResult",
            vec![
                required("protocolVersion", "string"),
                required("runnerName", "string"),
                required("runnerVersion", "string"),
                required("capabilities", "RunnerCapabilities"),
            ],
        ),
        object(
            "RunnerCapabilities",
            vec![
                required("supportedMethods", "string[]"),
                required("plannedMethods", "string[]"),
                required("transports", "string[]"),
            ],
        ),
        object(
            "RunnerCheckResult",
            vec![
                required("ok", "boolean"),
                required("protocolVersion", "string"),
                required("runnerName", "string"),
                required("runnerVersion", "string"),
                required("rustPackage", "string"),
            ],
        ),
        object(
            "RunnerProtocolSchema",
            vec![
                required("schemaVersion", "string"),
                required("transport", "string"),
                required("requests", "RunnerMethodSchema[]"),
                required("callbacks", "RunnerMethodSchema[]"),
                required("notifications", "RunnerMethodSchema[]"),
                required("definitions", "RunnerPayloadSchema[]"),
            ],
        ),
        object(
            "RunnerMethodSchema",
            vec![
                required("method", "string"),
                required("direction", "RunnerMessageDirection"),
                required("status", "RunnerMethodStatus"),
                required("summary", "string"),
                optional("params", "RunnerPayloadRef"),
                optional("result", "RunnerPayloadRef"),
            ],
        ),
        object("RunnerPayloadRef", vec![required("name", "string")]),
        object(
            "RunnerPayloadSchema",
            vec![
                required("name", "string"),
                required("shape", "RunnerPayloadShape"),
                defaulted("fields", "RunnerPayloadFieldSchema[]", "[]"),
                defaulted("values", "string[]", "[]"),
            ],
        ),
        object(
            "RunnerPayloadFieldSchema",
            vec![
                required("name", "string"),
                required("type", "string"),
                required("required", "boolean"),
                optional("default", "string"),
            ],
        ),
        enum_definition(
            "RunnerMessageDirection",
            vec!["sdk_to_runner", "runner_to_sdk"],
        ),
        enum_definition("RunnerMethodStatus", vec!["implemented", "reserved"]),
        enum_definition("RunnerPayloadShape", vec!["object", "enum"]),
        enum_definition(
            "RunnerFailureKind",
            vec![
                "source_unavailable",
                "auth_failed",
                "tool_failed",
                "model_failed",
                "budget_exhausted",
                "cancelled",
                "policy_denied",
                "internal_error",
            ],
        ),
        enum_definition(
            "RunnerRetryHint",
            vec![
                "retryable",
                "not_retryable",
                "retry_after",
                "requires_user_action",
            ],
        ),
        object(
            "RunStartParams",
            vec![
                optional("protocolVersion", "string"),
                optional("runId", "string"),
                optional("repo", "string"),
                optional("source", "ReviewSource"),
                optional("sourceProvider", "RunSourceProviderParams"),
                defaulted("changedFiles", "string[]", "[]"),
                defaulted("metadata", "json", "{}"),
                optional("change", "RunChangeParams"),
                defaulted("instructions", "RunInstructionParams[]", "[]"),
                defaulted("sessions", "RunSessionParams[]", "[]"),
                optional("limits", "RunLimitParams"),
                optional("model", "RunModelParams"),
                defaulted("tools", "RunToolParams[]", "[]"),
                optional("heartbeat", "RunHeartbeatConfigParams"),
                optional("contextEngine", "ContextEngineConfig"),
            ],
        ),
        object(
            "ContextEngineConfig",
            vec![
                required("mode", "ContextEngineMode"),
                optional("semantic", "ContextSemanticConfig"),
                required("maxIndexedFiles", "integer"),
                required("maxIndexedBytes", "integer"),
                required("maxEvidenceItems", "integer"),
                required("maxPackTokens", "integer"),
                required("maxQueryResults", "integer"),
                required("includeRepositoryGuidance", "boolean"),
                required("includeHostContext", "boolean"),
                required("strictEvidenceRequired", "boolean"),
            ],
        ),
        enum_definition("ContextEngineMode", vec!["disabled", "snapshot_v0"]),
        object(
            "ContextSemanticConfig",
            vec![
                required("mode", "ContextSemanticMode"),
                optional("provider", "ContextEmbeddingProviderKind"),
                optional("hostedBaseUrl", "string"),
                optional("hostedModel", "string"),
                optional("hostedCredentialRef", "string"),
                defaulted("allowRestrictedHostedInputs", "boolean", "false"),
                required("maxEmbeddingInputs", "integer"),
            ],
        ),
        enum_definition(
            "ContextSemanticMode",
            vec!["no_vector", "local", "hosted"],
        ),
        enum_definition("ContextEmbeddingProviderKind", vec!["local", "hosted"]),
        object(
            "RunHeartbeatConfigParams",
            vec![
                defaulted("callback", "boolean", "false"),
                optional("intervalMs", "integer"),
                optional("leaseSeconds", "integer"),
            ],
        ),
        object(
            "ReviewSource",
            vec![
                required(
                    "type",
                    "local | raw_snapshot | github_pull_request | gitlab_merge_request | perforce_changelist | custom",
                ),
                optional("repo", "string"),
                optional("root", "string"),
                defaulted("changedFiles", "string[]", "[]"),
                optional("owner", "string"),
                optional("number", "integer"),
                optional("server", "string"),
                optional("changelist", "string"),
                optional("client", "string"),
                defaulted("depotPaths", "string[]", "[]"),
                optional("provider", "string"),
                optional("id", "string"),
            ],
        ),
        object(
            "RunSourceProviderParams",
            vec![
                optional("baseUrl", "string"),
                defaulted("callback", "boolean", "false"),
            ],
        ),
        object(
            "RunChangeParams",
            vec![
                required("kind", "string"),
                optional("baseRevision", "string"),
                optional("startRevision", "string"),
                optional("headRevision", "string"),
                defaulted("changedFiles", "RunChangeFileParams[]", "[]"),
                optional("diff", "string"),
                optional("reviewTarget", "string"),
            ],
        ),
        object(
            "RunChangeFileParams",
            vec![required("path", "string"), optional("status", "string")],
        ),
        object(
            "RunInstructionParams",
            vec![
                required("kind", "string"),
                required("text", "string"),
                defaulted("trusted", "boolean", "false"),
            ],
        ),
        object(
            "RunModelParams",
            vec![
                defaulted("callback", "boolean", "false"),
                optional("defaultModelProfileId", "string"),
                defaulted("modelProfiles", "RunModelProfileParams[]", "[]"),
            ],
        ),
        object(
            "RunModelProfileParams",
            vec![
                required("id", "string"),
                required("provider", "string"),
                required("model", "string"),
                optional("credential", "RunModelCredentialParams"),
                optional("baseUrl", "string"),
                optional("apiProtocol", "string"),
                optional("maxInputTokens", "integer"),
                optional("maxOutputTokens", "integer"),
                optional("temperature", "number"),
                optional("topP", "number"),
            ],
        ),
        object(
            "RunModelCredentialParams",
            vec![optional("env", "string"), optional("secretRef", "string")],
        ),
        object(
            "RunToolParams",
            vec![
                required("id", "string"),
                required("description", "string"),
                required("parameters", "json"),
                defaulted("effects", "string[]", "[]"),
                defaulted("cacheable", "boolean", "false"),
                defaulted("providerResources", "string[]", "[]"),
            ],
        ),
        object(
            "RunSessionParams",
            vec![
                required("id", "string"),
                defaulted("role", "Role", "generalist"),
                required("objective", "string"),
                optional("cwd", "string"),
                optional("modelProfileId", "string"),
                optional("responseFormat", "ModelResponseFormat"),
                defaulted("instructions", "RunInstructionParams[]", "[]"),
                defaulted("toolGrants", "string[]", "[]"),
                optional("budget", "RunAgentBudgetParams"),
            ],
        ),
        object(
            "ModelResponseFormat",
            vec![
                required("name", "string"),
                required("schema", "json"),
                defaulted("strict", "boolean", "true"),
            ],
        ),
        enum_definition(
            "Role",
            vec![
                "generalist",
                "security",
                "performance",
                "maintainability",
                "correctness",
                "architecture",
                "validator",
            ],
        ),
        object(
            "RunAgentBudgetParams",
            vec![
                required("maxTurns", "integer"),
                required("maxToolCalls", "integer"),
                required("maxPromptTokens", "integer"),
                required("maxOutputTokens", "integer"),
            ],
        ),
        object(
            "RunnerSecretResolveParams",
            vec![
                required("protocolVersion", "string"),
                required("ref", "string"),
            ],
        ),
        object(
            "RunnerSecretResolveResult",
            vec![required("value", "string")],
        ),
        object(
            "RunLimitParams",
            vec![
                optional("maxActiveSessions", "integer"),
                optional("maxChildSessions", "integer"),
                optional("maxFileBytes", "integer"),
                optional("maxSearchMatches", "integer"),
                optional("orchestratorModelProfileId", "string"),
                optional("searchModelProfileId", "string"),
                optional("exploreModelProfileId", "string"),
                optional("validatorModelProfileId", "string"),
            ],
        ),
        object("RunLookupParams", vec![required("runId", "string")]),
        object(
            "ArtifactReadParams",
            vec![
                required("runId", "string"),
                required("artifactId", "string"),
                defaulted("view", "RunnerArtifactView", "redacted"),
            ],
        ),
        object(
            "ArtifactExportParams",
            vec![
                required("runId", "string"),
                defaulted("artifactIds", "string[]", "[]"),
                defaulted("view", "RunnerArtifactView", "redacted"),
                optional("maxArtifacts", "integer"),
                optional("maxBytes", "integer"),
            ],
        ),
        enum_definition("RunnerArtifactView", vec!["redacted", "raw"]),
        object(
            "SnapshotReadTextParams",
            vec![
                required("runId", "string"),
                optional("snapshotId", "string"),
                required("path", "string"),
                optional("maxBytes", "integer"),
            ],
        ),
        object(
            "WebhookHandleParams",
            vec![
                optional("projectId", "string"),
                defaulted("headers", "object<string,string>", "{}"),
                required("body", "string"),
                optional("secret", "string"),
                defaulted("options", "WebhookReviewOptions", "{}"),
            ],
        ),
        object(
            "WebhookReviewOptions",
            vec![defaulted("reviewOptions", "ReviewOptions", "{}")],
        ),
        object(
            "WorkerRunOnceParams",
            vec![
                optional("workerId", "string"),
                defaulted("maxSessions", "integer", "1"),
                defaulted("hostConfig", "HostConfiguration", "{}"),
            ],
        ),
        object(
            "HostConfiguration",
            vec![defaulted(
                "scheduling",
                "HostSchedulingConfiguration",
                "default",
            )],
        ),
        object(
            "HostSchedulingConfiguration",
            vec![
                defaulted("leaseSeconds", "integer", "60"),
                defaulted("defaultRetryPolicy", "ReviewRetryPolicy", "default"),
                defaulted("concurrency", "ReviewWorkerConcurrencyLimits", "default"),
                defaulted("fairness", "SchedulingFairnessStrategy", "fifo"),
            ],
        ),
        enum_definition(
            "SchedulingFairnessStrategy",
            vec!["fifo", "round_robin_by_project"],
        ),
        object(
            "ReviewRetryPolicy",
            vec![
                required("maxAttempts", "integer"),
                required("initialBackoffSeconds", "integer"),
                required("maxBackoffSeconds", "integer"),
            ],
        ),
        object(
            "ReviewWorkerConcurrencyLimits",
            vec![
                optional("maxRunningGlobal", "integer"),
                optional("maxRunningPerProject", "integer"),
                optional("maxRunningPerUser", "integer"),
                optional("maxRunningPerModelProfile", "integer"),
                optional("maxRunningPerProviderProfile", "integer"),
            ],
        ),
        object(
            "WorkerRunOnceResult",
            vec![
                required("workerId", "string"),
                required("claimed", "integer"),
                required("completed", "integer"),
                required("retried", "integer"),
                required("failed", "integer"),
                required("skipped", "integer"),
            ],
        ),
        object(
            "SourceMaterializeParams",
            vec![
                required("protocolVersion", "string"),
                required("source", "ReviewSource"),
                defaulted("changedFiles", "string[]", "[]"),
            ],
        ),
        object(
            "SourceMaterializeResult",
            vec![
                required("root", "string"),
                defaulted("changedFiles", "string[]", "[]"),
            ],
        ),
        object(
            "RunHeartbeatParams",
            vec![
                required("protocolVersion", "string"),
                required("runId", "string"),
                required("sequence", "integer"),
                required("elapsedMs", "integer"),
                optional("leaseSeconds", "integer"),
            ],
        ),
        object(
            "RunHeartbeatResult",
            vec![defaulted("continueRun", "boolean", "true")],
        ),
        object(
            "RunStatusResult",
            vec![required("runId", "string"), required("status", "string")],
        ),
        object(
            "RunCancelResult",
            vec![
                required("runId", "string"),
                required("status", "string"),
                required("cancelled", "boolean"),
                required("reason", "string"),
            ],
        ),
        object(
            "RunnerRunResult",
            vec![
                required("protocolVersion", "string"),
                required("runId", "string"),
                required("status", "string"),
                required("summary", "RunnerRunSummary"),
                defaulted("fileReviews", "RunnerFileReview[]", "[]"),
                required("findings", "RunnerFinding[]"),
                required("snapshots", "RunnerSnapshotSummary[]"),
                defaulted("metadata", "json", "{}"),
            ],
        ),
        object(
            "RunnerRunSummary",
            vec![
                required("sessions", "integer"),
                required("completedSessions", "integer"),
                required("modelCalls", "integer"),
                required("toolCalls", "integer"),
                required("findings", "integer"),
                required("publishableFindings", "integer"),
                required("elapsedMs", "integer"),
                required("inputTokens", "integer"),
                required("outputTokens", "integer"),
                required("totalTokens", "integer"),
                defaulted("cachedInputTokens", "integer", "0"),
                required("artifacts", "integer"),
                required("artifactBytes", "integer"),
                required("snapshotCount", "integer"),
                required("qualityDiagnostics", "RunnerReviewQualityDiagnostics"),
            ],
        ),
        object(
            "RunnerReviewQualityDiagnostics",
            vec![
                required("contractRiskUnits", "integer"),
                required("contractSeedCount", "integer"),
                required("contractPackCount", "integer"),
                defaulted("omittedContractPackCandidates", "string[]", "[]"),
                defaulted("selectedContractPacks", "string[]", "[]"),
                required("contractEvidenceFailures", "integer"),
                defaulted("coverageCounts", "Record<string, integer>", "{}"),
                defaulted(
                    "coverageCountsByLens",
                    "Record<string, Record<string, integer>>",
                    "{}",
                ),
                defaulted("highRiskFilesBelowTarget", "string[]", "[]"),
                defaulted("challengeStatusCounts", "Record<string, integer>", "{}"),
                defaulted("sessionsRun", "integer", "0"),
                defaulted("budgetsUsed", "Record<string, integer>", "{}"),
                defaulted("explicitCallerCapSessions", "integer", "0"),
                required("candidateFindings", "integer"),
                required("rescuedCandidates", "integer"),
                required("rejectedCandidates", "integer"),
                defaulted("rejectionReasons", "Record<string, integer>", "{}"),
            ],
        ),
        object(
            "RunnerFileReview",
            vec![
                required("path", "string"),
                required("verdict", "string"),
                defaulted("coverage", "string", "sampled"),
                defaulted("reviewVerdict", "string", "clean"),
                required("summary", "string"),
                defaulted("relatedPaths", "string[]", "[]"),
                defaulted("evidenceArtifactIds", "string[]", "[]"),
                required("evidenceCount", "integer"),
                required("sessionId", "string"),
                required("unitId", "string"),
            ],
        ),
        object(
            "RunnerFinding",
            vec![
                required("id", "string"),
                required("title", "string"),
                required("claim", "string"),
                required("evidenceCount", "integer"),
                required("publishable", "boolean"),
                optional("severity", "string"),
                optional("confidence", "number"),
                optional("validationStatus", "string"),
                optional("challengeStatus", "string"),
                defaulted("evidence", "RunnerFindingEvidence[]", "[]"),
                defaulted("discoveredBy", "string[]", "[]"),
                defaulted("validatedBy", "string[]", "[]"),
                defaulted("challengedBy", "string[]", "[]"),
                optional("location", "RunnerFindingLocation"),
                defaulted("relatedPaths", "string[]", "[]"),
            ],
        ),
        object(
            "RunnerFindingEvidence",
            vec![
                required("evidenceId", "string"),
                required("artifactId", "string"),
                required("kind", "string"),
                required("contentHash", "string"),
                required("producingToolCallId", "string"),
            ],
        ),
        object(
            "RunnerFindingLocation",
            vec![
                required("path", "string"),
                optional("revision", "string"),
                optional("startLine", "integer"),
                optional("endLine", "integer"),
                optional("startColumn", "integer"),
                optional("endColumn", "integer"),
                optional("side", "string"),
                optional("providerAnchor", "json"),
            ],
        ),
        object(
            "RunnerSnapshotSummary",
            vec![
                required("snapshotId", "string"),
                required("files", "integer"),
                required("changedFiles", "integer"),
                required("capturedFiles", "integer"),
                required("capturedBytes", "integer"),
            ],
        ),
        object(
            "RunnerArtifact",
            vec![
                required("artifactId", "string"),
                required("bytes", "integer"),
                required("contentHash", "string"),
                required("content", "string"),
            ],
        ),
        object(
            "RunnerArtifactReadResult",
            vec![
                required("runId", "string"),
                required("view", "RunnerArtifactView"),
                required("artifact", "RunnerArtifact"),
            ],
        ),
        object(
            "RunnerArtifactExportResult",
            vec![
                required("runId", "string"),
                required("view", "RunnerArtifactView"),
                required("artifactCount", "integer"),
                required("totalBytes", "integer"),
                required("artifacts", "RunnerArtifact[]"),
            ],
        ),
        object(
            "RunnerSnapshotTextResult",
            vec![
                required("runId", "string"),
                required("snapshotId", "string"),
                required("path", "string"),
                required("contentHash", "string"),
                required("bytes", "integer"),
                required("truncated", "boolean"),
                required("content", "string"),
            ],
        ),
        object(
            "ContextIndexParams",
            vec![
                required("repo", "string"),
                defaulted("changedFiles", "string[]", "[]"),
                defaulted("hostMetadata", "object<string,json>", "{}"),
                defaulted("crossRepoContracts", "CrossRepoContractCandidate[]", "[]"),
                defaulted("allowedCrossRepoResources", "string[]", "[]"),
                optional("config", "ContextEngineConfig"),
            ],
        ),
        object(
            "CrossRepoContractCandidate",
            vec![
                required("resourceId", "string"),
                required("repository", "string"),
                required("summary", "string"),
                optional("originalUrl", "string"),
            ],
        ),
        object(
            "ContextManifest",
            vec![
                required("schemaVersion", "string"),
                required("engineVersion", "string"),
                required("snapshotId", "string"),
                required("ruleCount", "integer"),
                required("evidenceCount", "integer"),
                required("relationshipCount", "integer"),
                required("skippedCount", "integer"),
                required("createdAtUtc", "string"),
            ],
        ),
        object(
            "ContextEvidence",
            vec![
                required("id", "string"),
                required("kind", "ContextEvidenceKind"),
                required("source", "ContextEvidenceSource"),
                required("trust", "ContextTrust"),
                required("sensitivity", "ContextSensitivity"),
                required("scope", "ContextScope"),
                optional("path", "string"),
                optional("revision", "string"),
                optional("range", "ContextRange"),
                optional("contentHash", "string"),
                optional("summary", "string"),
                required("tokenEstimate", "integer"),
                required("provenance", "ContextProvenance"),
                optional("createdAtUtc", "string"),
                optional("expiresAtUtc", "string"),
            ],
        ),
        enum_definition(
            "ContextEvidenceKind",
            vec![
                "diff",
                "file_span",
                "symbol",
                "test",
                "config",
                "doc",
                "repository_rule",
                "organization_rule",
                "ticket",
                "historical_pr",
                "prior_finding",
                "ci_failure",
                "dependency",
                "cross_repo_contract",
                "tool_output",
                "pack_summary",
            ],
        ),
        enum_definition(
            "ContextEvidenceSource",
            vec!["snapshot", "host", "history", "memory", "tool", "external"],
        ),
        enum_definition(
            "ContextTrust",
            vec![
                "kernel",
                "host_trusted",
                "organization_trusted",
                "repository_untrusted",
                "user_untrusted",
                "external_untrusted",
                "tool_provider",
            ],
        ),
        enum_definition(
            "ContextSensitivity",
            vec!["public", "private", "secret_redacted", "restricted"],
        ),
        enum_definition(
            "ContextScope",
            vec!["run", "snapshot", "workspace", "repository", "organization", "external"],
        ),
        object(
            "ContextRange",
            vec![required("startLine", "integer"), required("endLine", "integer")],
        ),
        object(
            "ContextProvenance",
            vec![
                required("provider", "string"),
                optional("query", "string"),
                optional("toolCallId", "string"),
                optional("snapshotId", "string"),
                optional("originalUrl", "string"),
            ],
        ),
        object(
            "ContextPackRequest",
            vec![
                optional("runId", "string"),
                required("snapshotId", "string"),
                optional("sessionId", "string"),
                required("purpose", "ContextPackPurpose"),
                required("maxTokens", "integer"),
            ],
        ),
        object(
            "ContextPack",
            vec![
                required("id", "string"),
                optional("runId", "string"),
                required("snapshotId", "string"),
                optional("sessionId", "string"),
                required("purpose", "ContextPackPurpose"),
                required("evidence", "ContextEvidence[]"),
                required("relationships", "ContextRelationship[]"),
                required("omittedCandidates", "OmittedContextCandidate[]"),
                required("budget", "ContextBudgetUsage"),
                required("sufficiency", "ContextSufficiency"),
                required("compilerVersion", "string"),
                required("createdAtUtc", "string"),
            ],
        ),
        enum_definition(
            "ContextPackPurpose",
            vec![
                "general_review",
                "correctness",
                "security",
                "tests",
                "architecture",
                "performance",
                "validator",
                "standalone_query",
            ],
        ),
        object(
            "ContextRelationship",
            vec![
                required("from", "string"),
                required("to", "string"),
                required("kind", "ContextRelationshipKind"),
                required("confidence", "number"),
                required("reason", "string"),
            ],
        ),
        enum_definition(
            "ContextRelationshipKind",
            vec![
                "imports",
                "calls",
                "implements",
                "tests",
                "configures",
                "documents",
                "depends_on",
                "same_symbol",
                "similar_history",
                "violates_rule",
                "satisfies_ticket",
                "contradicts",
                "cross_repo_contract",
            ],
        ),
        object(
            "OmittedContextCandidate",
            vec![
                required("evidenceId", "string"),
                required("kind", "ContextEvidenceKind"),
                optional("path", "string"),
                required("score", "number"),
                required("tokenEstimate", "integer"),
                required("reason", "ContextOmissionReason"),
                optional("budgetState", "OmittedContextBudgetState"),
            ],
        ),
        object(
            "OmittedContextBudgetState",
            vec![
                required("remainingTokens", "integer"),
                required("fullContentRemainingTokens", "integer"),
                required("fullContentShortfallTokens", "integer"),
                optional("skeletonTokenEstimate", "integer"),
                optional("skeletonShortfallTokens", "integer"),
            ],
        ),
        enum_definition(
            "ContextOmissionReason",
            vec![
                "budget_exhausted",
                "duplicate",
                "low_relevance",
                "lower_trust",
                "generated_file",
                "binary_file",
                "secret_redacted",
                "outside_scope",
                "superseded_by_summary",
                "requires_ungranted_capability",
            ],
        ),
        object(
            "ContextBudgetUsage",
            vec![required("maxTokens", "integer"), required("usedTokens", "integer")],
        ),
        object(
            "ContextSufficiency",
            vec![
                required("status", "ContextSufficiencyStatus"),
                required("missing", "string[]"),
            ],
        ),
        enum_definition(
            "ContextSufficiencyStatus",
            vec!["sufficient", "probably_sufficient", "insufficient"],
        ),
        object(
            "ContextQuery",
            vec![
                optional("runId", "string"),
                required("snapshotId", "string"),
                optional("sessionId", "string"),
                optional("purpose", "ContextPackPurpose"),
                required("kind", "ContextQueryKind"),
                required("arguments", "json"),
                defaulted("currentEvidence", "string[]", "[]"),
                required("limits", "ContextQueryLimits"),
            ],
        ),
        object(
            "ContextQueryLimits",
            vec![required("maxResults", "integer"), required("maxTokens", "integer")],
        ),
        enum_definition(
            "ContextQueryKind",
            vec![
                "search_text",
                "read_span",
                "explain_pack",
                "related_tests",
                "related_symbols",
                "ticket_requirements",
                "history_similar",
                "cross_repo_contracts",
                "sufficiency_check",
            ],
        ),
        object(
            "ContextQueryResult",
            vec![
                required("kind", "ContextQueryKind"),
                required("evidence", "ContextEvidence[]"),
                optional("sufficiency", "ContextSufficiency"),
                optional("data", "json"),
                required("omitted", "integer"),
            ],
        ),
        object(
            "ContextFeedback",
            vec![
                required("snapshotId", "string"),
                defaulted("evidenceIds", "string[]", "[]"),
                required("feedback", "string"),
                optional("source", "ContextLearningSource"),
                optional("scope", "ContextLearningScope"),
            ],
        ),
        object(
            "ContextFeedbackReceipt",
            vec![
                required("accepted", "boolean"),
                required("message", "string"),
                optional("proposedLearning", "ContextLearning"),
            ],
        ),
        object(
            "ContextLearning",
            vec![
                required("id", "string"),
                required("snapshotId", "string"),
                required("source", "ContextLearningSource"),
                required("status", "ContextLearningStatus"),
                required("scope", "ContextLearningScope"),
                required("evidenceIds", "string[]"),
                required("summary", "string"),
                required("createdAtUtc", "string"),
                optional("expiresAtUtc", "string"),
            ],
        ),
        enum_definition(
            "ContextLearningSource",
            vec![
                "accepted_finding",
                "dismissed_finding",
                "human_feedback",
                "merged_pr",
                "manual_rule",
            ],
        ),
        enum_definition(
            "ContextLearningStatus",
            vec!["proposed", "approved", "expired", "rejected"],
        ),
        enum_definition(
            "ContextLearningScope",
            vec!["repository", "workspace", "organization"],
        ),
        object(
            "ContextLearningApproval",
            vec![
                required("learningId", "string"),
                defaulted("approve", "boolean", "false"),
                optional("expiresAtUtc", "string"),
            ],
        ),
        object(
            "ContextLearningApprovalParams",
            vec![
                required("snapshotId", "string"),
                required("learningId", "string"),
                defaulted("approve", "boolean", "false"),
                optional("expiresAtUtc", "string"),
            ],
        ),
        object(
            "ContextLearningApprovalReceipt",
            vec![
                required("accepted", "boolean"),
                required("learning", "ContextLearning"),
            ],
        ),
        object(
            "ContextFindingEvidence",
            vec![
                required("findingId", "string"),
                required("primaryEvidence", "string[]"),
                required("supportingEvidence", "string[]"),
                required("contradictedBy", "string[]"),
                required("sufficiency", "ContextSufficiencyStatus"),
                optional("artifactId", "string"),
            ],
        ),
        object(
            "ContextFindingsEvidenceArtifact",
            vec![
                required("schemaVersion", "string"),
                required("runId", "string"),
                required("findings", "ContextFindingEvidence[]"),
            ],
        ),
        object(
            "ReviewHttpResponse",
            vec![
                required("statusCode", "integer"),
                required("headers", "object<string,string>"),
                required("body", "string"),
            ],
        ),
        object(
            "ReviewEventRecord",
            vec![
                required("seq", "integer"),
                required("timestampUtc", "string"),
                optional("runId", "string"),
                optional("snapshotId", "string"),
                optional("sessionId", "string"),
                optional("turn", "integer"),
                optional("toolCallId", "string"),
                optional("artifactId", "string"),
                optional("findingId", "string"),
                required("event", "ReviewEvent"),
            ],
        ),
        object(
            "RuntimeEventRecord",
            vec![
                required("seq", "integer"),
                required("timestampUtc", "string"),
                required("context", "RuntimeEventContext"),
                required("event", "RuntimeEvent"),
            ],
        ),
        object(
            "RunFailedNotification",
            vec![
                required("error", "string"),
                required("kind", "string"),
                required("failureKind", "RunnerFailureKind"),
                required("retryHint", "RunnerRetryHint"),
                optional("retryAfterSeconds", "integer"),
            ],
        ),
        object(
            "RunnerModelCompleteParams",
            vec![
                required("protocolVersion", "string"),
                required("runId", "string"),
                required("sessionId", "string"),
                required("role", "Role"),
                required("objective", "string"),
                optional("snapshotId", "string"),
                optional("modelProfileId", "string"),
                required("turn", "integer"),
                required("transcript", "json[]"),
            ],
        ),
        object(
            "RunnerModelCompleteResult",
            vec![
                optional("content", "string"),
                defaulted("toolCalls", "RunnerModelToolCallResult[]", "[]"),
                optional("usage", "RunnerTokenUsage"),
            ],
        ),
        object(
            "RunnerModelToolCallResult",
            vec![
                optional("callId", "string"),
                required("toolId", "string"),
                defaulted("arguments", "json", "null"),
            ],
        ),
        object(
            "RunnerTokenUsage",
            vec![
                required("inputTokens", "integer"),
                required("outputTokens", "integer"),
                required("totalTokens", "integer"),
                defaulted("cachedInputTokens", "integer", "0"),
            ],
        ),
        object(
            "RunnerToolExecuteParams",
            vec![
                required("protocolVersion", "string"),
                required("runId", "string"),
                required("sessionId", "string"),
                required("turn", "integer"),
                required("callId", "string"),
                required("toolId", "string"),
                required("snapshotId", "string"),
                required("providerResources", "string[]"),
                required("arguments", "json"),
            ],
        ),
        object(
            "RunnerToolExecuteResult",
            vec![
                optional("data", "json"),
                optional("artifact", "RunnerToolArtifactResult"),
            ],
        ),
        object(
            "RunnerToolArtifactResult",
            vec![required("key", "string"), required("content", "string")],
        ),
    ]
}

fn payload_ref(name: &'static str) -> RunnerPayloadRef {
    RunnerPayloadRef {
        name: name.to_string(),
    }
}

fn object(name: &'static str, fields: Vec<RunnerPayloadFieldSchema>) -> RunnerPayloadSchema {
    RunnerPayloadSchema {
        name: name.to_string(),
        shape: RunnerPayloadShape::Object,
        fields,
        values: Vec::new(),
    }
}

fn enum_definition(name: &'static str, values: Vec<&'static str>) -> RunnerPayloadSchema {
    RunnerPayloadSchema {
        name: name.to_string(),
        shape: RunnerPayloadShape::Enum,
        fields: Vec::new(),
        values: values.into_iter().map(str::to_string).collect(),
    }
}

fn required(name: &'static str, value_type: &'static str) -> RunnerPayloadFieldSchema {
    field(name, value_type, true, None)
}

fn optional(name: &'static str, value_type: &'static str) -> RunnerPayloadFieldSchema {
    field(name, value_type, false, Some("null"))
}

fn defaulted(
    name: &'static str,
    value_type: &'static str,
    default: &'static str,
) -> RunnerPayloadFieldSchema {
    field(name, value_type, false, Some(default))
}

fn field(
    name: &'static str,
    value_type: &'static str,
    required: bool,
    default: Option<&'static str>,
) -> RunnerPayloadFieldSchema {
    RunnerPayloadFieldSchema {
        name: name.to_string(),
        value_type: value_type.to_string(),
        required,
        default: default.map(str::to_string),
    }
}
