use super::types::{
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
            supported_methods: vec![
                "runner.handshake".to_string(),
                "runner.check".to_string(),
                "runner.schema.export".to_string(),
                "run.start".to_string(),
                "run.cancel".to_string(),
                "run.status".to_string(),
                "run.result".to_string(),
                "artifact.read".to_string(),
                "artifact.export".to_string(),
                "snapshot.readText".to_string(),
                "webhook.github.handle".to_string(),
                "webhook.gitlab.handle".to_string(),
                "worker.runOnce".to_string(),
                "source.materialize".to_string(),
                "run.heartbeat".to_string(),
                "model.complete".to_string(),
                "secret.resolve".to_string(),
                "tool.execute".to_string(),
                "event.review".to_string(),
                "event.runtime".to_string(),
                "run.finished".to_string(),
                "run.failed".to_string(),
            ],
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
        requests: vec![
            implemented(
                "runner.handshake",
                "Negotiate protocol version and capabilities.",
            ),
            implemented("runner.check", "Return local runner diagnostics."),
            implemented(
                "runner.schema.export",
                "Return protocol method metadata for SDK validation.",
            ),
            implemented("run.start", "Start a review run."),
            implemented("run.cancel", "Cancel an active review run."),
            implemented("run.status", "Read active run status."),
            implemented("run.result", "Read final run report."),
            implemented("artifact.read", "Read one redacted or raw artifact."),
            implemented("artifact.export", "Export artifacts using a policy."),
            implemented("snapshot.readText", "Read captured snapshot text."),
            implemented(
                "webhook.github.handle",
                "Verify and schedule a GitHub webhook delivery.",
            ),
            implemented(
                "webhook.gitlab.handle",
                "Verify and schedule a GitLab webhook delivery.",
            ),
            implemented(
                "worker.runOnce",
                "Claim and execute ready durable review sessions through the Rust worker.",
            ),
        ],
        callbacks: vec![
            implemented_runner_to_sdk(
                "source.materialize",
                "Ask the SDK source provider to materialize a review source.",
            ),
            implemented_runner_to_sdk(
                "run.heartbeat",
                "Ask the SDK host to renew an active run lease.",
            ),
            implemented_runner_to_sdk(
                "model.complete",
                "Ask the SDK model adapter for one model turn.",
            ),
            implemented_runner_to_sdk(
                "secret.resolve",
                "Ask the SDK host to resolve one secret reference.",
            ),
            implemented_runner_to_sdk("tool.execute", "Ask the SDK to execute a host custom tool."),
        ],
        notifications: vec![
            implemented_runner_to_sdk("event.review", "Emit one host-facing review event."),
            implemented_runner_to_sdk("event.runtime", "Emit one advanced runtime event."),
            implemented_runner_to_sdk(
                "run.finished",
                "Notify that a run reached a terminal state.",
            ),
            implemented_runner_to_sdk(
                "run.failed",
                "Notify that a run failed before producing a report.",
            ),
        ],
        definitions: payload_definitions(),
    }
}

fn implemented(method: &'static str, summary: &'static str) -> RunnerMethodSchema {
    RunnerMethodSchema {
        method: method.to_string(),
        direction: RunnerMessageDirection::SdkToRunner,
        status: RunnerMethodStatus::Implemented,
        summary: summary.to_string(),
        params: method_params(method),
        result: method_result(method),
    }
}

fn implemented_runner_to_sdk(method: &'static str, summary: &'static str) -> RunnerMethodSchema {
    RunnerMethodSchema {
        method: method.to_string(),
        direction: RunnerMessageDirection::RunnerToSdk,
        status: RunnerMethodStatus::Implemented,
        summary: summary.to_string(),
        params: method_params(method),
        result: method_result(method),
    }
}

fn method_params(method: &str) -> Option<RunnerPayloadRef> {
    match method {
        "runner.handshake" => Some(payload_ref("RunnerHandshakeParams")),
        "run.start" => Some(payload_ref("RunStartParams")),
        "run.cancel" | "run.status" | "run.result" => Some(payload_ref("RunLookupParams")),
        "artifact.read" => Some(payload_ref("ArtifactReadParams")),
        "artifact.export" => Some(payload_ref("ArtifactExportParams")),
        "snapshot.readText" => Some(payload_ref("SnapshotReadTextParams")),
        "webhook.github.handle" | "webhook.gitlab.handle" => {
            Some(payload_ref("WebhookHandleParams"))
        }
        "worker.runOnce" => Some(payload_ref("WorkerRunOnceParams")),
        "source.materialize" => Some(payload_ref("SourceMaterializeParams")),
        "run.heartbeat" => Some(payload_ref("RunHeartbeatParams")),
        "model.complete" => Some(payload_ref("RunnerModelCompleteParams")),
        "secret.resolve" => Some(payload_ref("RunnerSecretResolveParams")),
        "tool.execute" => Some(payload_ref("RunnerToolExecuteParams")),
        "event.review" => Some(payload_ref("ReviewEventRecord")),
        "event.runtime" => Some(payload_ref("RuntimeEventRecord")),
        "run.finished" => Some(payload_ref("RunnerRunResult")),
        "run.failed" => Some(payload_ref("RunFailedNotification")),
        _ => None,
    }
}

fn method_result(method: &str) -> Option<RunnerPayloadRef> {
    match method {
        "runner.handshake" => Some(payload_ref("RunnerHandshakeResult")),
        "runner.check" => Some(payload_ref("RunnerCheckResult")),
        "runner.schema.export" => Some(payload_ref("RunnerProtocolSchema")),
        "run.start" | "run.result" => Some(payload_ref("RunnerRunResult")),
        "run.cancel" => Some(payload_ref("RunCancelResult")),
        "run.status" => Some(payload_ref("RunStatusResult")),
        "artifact.read" => Some(payload_ref("RunnerArtifactReadResult")),
        "artifact.export" => Some(payload_ref("RunnerArtifactExportResult")),
        "snapshot.readText" => Some(payload_ref("RunnerSnapshotTextResult")),
        "webhook.github.handle" | "webhook.gitlab.handle" => {
            Some(payload_ref("ReviewHttpResponse"))
        }
        "worker.runOnce" => Some(payload_ref("WorkerRunOnceResult")),
        "source.materialize" => Some(payload_ref("SourceMaterializeResult")),
        "run.heartbeat" => Some(payload_ref("RunHeartbeatResult")),
        "model.complete" => Some(payload_ref("RunnerModelCompleteResult")),
        "secret.resolve" => Some(payload_ref("RunnerSecretResolveResult")),
        "tool.execute" => Some(payload_ref("RunnerToolExecuteResult")),
        _ => None,
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
            ],
        ),
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
                defaulted("metadata", "json", "{}"),
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
                defaulted("instructions", "RunInstructionParams[]", "[]"),
                defaulted("toolGrants", "string[]", "[]"),
                optional("budget", "RunAgentBudgetParams"),
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
                optional("maxFileBytes", "integer"),
                optional("maxSearchMatches", "integer"),
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
                optional("workspaceId", "string"),
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
            vec!["fifo", "round_robin_by_workspace"],
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
                optional("maxRunningPerWorkspace", "integer"),
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
                required("artifacts", "integer"),
                required("artifactBytes", "integer"),
                required("snapshotCount", "integer"),
            ],
        ),
        object(
            "RunnerFileReview",
            vec![
                required("path", "string"),
                required("verdict", "string"),
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
                defaulted("evidence", "RunnerFindingEvidence[]", "[]"),
                defaulted("discoveredBy", "string[]", "[]"),
                defaulted("validatedBy", "string[]", "[]"),
                defaulted("challengedBy", "string[]", "[]"),
                optional("location", "RunnerFindingLocation"),
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
