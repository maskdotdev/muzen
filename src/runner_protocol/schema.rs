use super::schema_catalog::payload_definitions;
use super::schema_types::{
    RunnerCapabilities, RunnerCheckResult, RunnerHandshakeResult, RunnerMessageDirection,
    RunnerMethodSchema, RunnerMethodStatus, RunnerPayloadRef, RunnerProtocolSchema,
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

pub(crate) fn sdk_to_runner_methods() -> impl Iterator<Item = &'static str> {
    SDK_TO_RUNNER_METHODS.iter().map(|spec| spec.method)
}

#[cfg(test)]
pub(crate) fn runner_to_sdk_callbacks() -> impl Iterator<Item = &'static str> {
    RUNNER_TO_SDK_CALLBACKS.iter().map(|spec| spec.method)
}

#[cfg(test)]
pub(crate) fn runner_to_sdk_notifications() -> impl Iterator<Item = &'static str> {
    RUNNER_TO_SDK_NOTIFICATIONS.iter().map(|spec| spec.method)
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
        "runner.debugState",
        "Return read-only state retention counts for the current stateful runner session.",
        None,
        Some("RunnerDebugStateResult"),
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
        "run.release",
        "Release a terminal run report and its retained artifacts from a stateful runner.",
        Some("RunLookupParams"),
        Some("RunReleaseResult"),
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

fn payload_ref(name: &'static str) -> RunnerPayloadRef {
    RunnerPayloadRef {
        name: name.to_string(),
    }
}
