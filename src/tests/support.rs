use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use crate::reviewer_kernel::events::ReviewEventRecord;
use crate::reviewer_kernel::kernel_types::{
    ArtifactKey, CapabilitySet, ConversationItem, LimitInfo, ModelToolCall, ModelTurn,
    ProviderResourceId, RuntimeError, RuntimeEvent, RuntimeEventContext, RuntimeEventRecord,
    RuntimeEventSink, RuntimeResult, SessionId, SessionScope, ToolCallId, ToolEffects, ToolGrant,
    ToolId, ToolMetricKey, ToolProviderId, TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;
use crate::reviewer_kernel::review_contract::*;
use crate::reviewer_kernel::system::{timestamp_utc, SCHEMA_VERSION};
use crate::reviewer_kernel::tool_engine::registry::{
    JsonRpcToolRegistration, JsonRpcToolRequest, JsonRpcToolResponse, JsonRpcToolTransport,
};
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::reviewer_kernel::tool_engine::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOptions, CustomToolOutput,
    ToolRegistry,
};
use async_trait::async_trait;

pub const TEST_REVIEW_EVENT_LOG_SCHEMA_VERSION: &str = "heimdaal.review-events.v1";

#[derive(Debug, Clone)]
pub struct TestReviewEventJsonlManifest {
    pub path: PathBuf,
    pub schema_version: String,
    pub record_count: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct TestReviewEventJsonlLoad {
    pub path: PathBuf,
    pub schema_version: String,
    pub record_count: usize,
    pub records: Vec<ReviewEventRecord>,
}

pub fn export_test_review_event_records_jsonl(
    path: impl AsRef<Path>,
    records: &[ReviewEventRecord],
) -> RuntimeResult<TestReviewEventJsonlManifest> {
    if let Some(parent) = path
        .as_ref()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!(
                "failed to create review event log directory: {error}"
            ))
        })?;
    }
    let mut file = std::fs::File::create(path.as_ref()).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to create review event log: {error}"))
    })?;
    let mut bytes = 0usize;
    for record in records {
        let line = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": TEST_REVIEW_EVENT_LOG_SCHEMA_VERSION,
            "seq": record.seq,
            "timestampUtc": record.timestamp_utc,
            "runId": record.run_id,
            "snapshotId": record.snapshot_id,
            "sessionId": record.session_id,
            "turn": record.turn,
            "toolCallId": record.tool_call_id,
            "artifactId": record.artifact_id,
            "findingId": record.finding_id,
            "event": record.event,
        }))
        .map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to serialize review event log: {error}"))
        })?;
        file.write_all(&line).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write review event log: {error}"))
        })?;
        file.write_all(b"\n").map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write review event log: {error}"))
        })?;
        bytes += line.len() + 1;
    }
    file.flush().map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to flush review event log: {error}"))
    })?;
    Ok(TestReviewEventJsonlManifest {
        path: path.as_ref().to_path_buf(),
        schema_version: TEST_REVIEW_EVENT_LOG_SCHEMA_VERSION.to_string(),
        record_count: records.len(),
        bytes,
    })
}

pub fn load_test_review_event_records_jsonl(
    path: impl AsRef<Path>,
) -> RuntimeResult<TestReviewEventJsonlLoad> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to read review event log: {error}"))
    })?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: TestReviewEventJsonlRecord = serde_json::from_str(line).map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "invalid review event log record at line {}: {error}",
                index + 1
            ))
        })?;
        if record.schema_version != TEST_REVIEW_EVENT_LOG_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidInput(format!(
                "unsupported review event log schemaVersion {} at line {}",
                record.schema_version,
                index + 1
            )));
        }
        records.push(record.record);
    }
    Ok(TestReviewEventJsonlLoad {
        path: path.to_path_buf(),
        schema_version: TEST_REVIEW_EVENT_LOG_SCHEMA_VERSION.to_string(),
        record_count: records.len(),
        records,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestReviewEventJsonlRecord {
    schema_version: String,
    #[serde(flatten)]
    record: ReviewEventRecord,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TestEventBackpressurePolicy {
    DropNewest,
    DropOldest,
}

#[derive(Debug)]
pub struct TestBoundedRuntimeEventSink {
    capacity: usize,
    policy: TestEventBackpressurePolicy,
    next_seq: AtomicU64,
    dropped: AtomicUsize,
    records: Mutex<Vec<RuntimeEventRecord>>,
}

impl TestBoundedRuntimeEventSink {
    pub fn new(capacity: usize) -> Self {
        Self::with_policy(capacity, TestEventBackpressurePolicy::DropNewest)
    }

    pub fn with_policy(capacity: usize, policy: TestEventBackpressurePolicy) -> Self {
        Self {
            capacity: capacity.max(1),
            policy,
            next_seq: AtomicU64::new(1),
            dropped: AtomicUsize::new(0),
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn records(&self) -> Vec<RuntimeEventRecord> {
        self.records
            .lock()
            .expect("bounded test event sink poisoned")
            .clone()
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn export_jsonl(
        &self,
        path: impl AsRef<Path>,
    ) -> RuntimeResult<TestRuntimeEventJsonlManifest> {
        export_test_runtime_event_records_jsonl(path, &self.records(), self.dropped_count())
    }

    fn record(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut records = self
            .records
            .lock()
            .expect("bounded test event sink poisoned");
        if records.len() >= self.capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            match self.policy {
                TestEventBackpressurePolicy::DropNewest => return,
                TestEventBackpressurePolicy::DropOldest => {
                    records.remove(0);
                }
            }
        }
        records.push(RuntimeEventRecord {
            seq,
            timestamp_utc: timestamp_utc(),
            context,
            event,
        });
    }
}

impl RuntimeEventSink for TestBoundedRuntimeEventSink {
    fn emit(&self, event: RuntimeEvent) {
        let context = RuntimeEventContext::from_event(&event);
        self.record(context, event);
    }

    fn emit_with_context(&self, context: RuntimeEventContext, event: RuntimeEvent) {
        self.record(context, event);
    }
}

#[derive(Debug, Clone)]
pub struct TestRuntimeEventJsonlManifest {
    pub path: PathBuf,
    pub record_count: usize,
    pub dropped_count: usize,
    pub bytes: usize,
}

fn export_test_runtime_event_records_jsonl(
    path: impl AsRef<Path>,
    records: &[RuntimeEventRecord],
    dropped_count: usize,
) -> RuntimeResult<TestRuntimeEventJsonlManifest> {
    if let Some(parent) = path
        .as_ref()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to create event log directory: {error}"))
        })?;
    }
    let mut file = std::fs::File::create(path.as_ref()).map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to create event log: {error}"))
    })?;
    let mut bytes = 0usize;
    for record in records {
        let line = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": SCHEMA_VERSION,
            "seq": record.seq,
            "timestampUtc": record.timestamp_utc,
            "context": &record.context,
            "event": &record.event,
        }))
        .map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to serialize event log: {error}"))
        })?;
        file.write_all(&line).map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write event log: {error}"))
        })?;
        file.write_all(b"\n").map_err(|error| {
            RuntimeError::RepoUnavailable(format!("failed to write event log: {error}"))
        })?;
        bytes += line.len() + 1;
    }
    file.flush().map_err(|error| {
        RuntimeError::RepoUnavailable(format!("failed to flush event log: {error}"))
    })?;
    Ok(TestRuntimeEventJsonlManifest {
        path: path.as_ref().to_path_buf(),
        record_count: records.len(),
        dropped_count,
        bytes,
    })
}

#[derive(Debug)]
pub struct EchoCustomTool;

#[derive(Debug)]
pub struct PublicFacadeModel {
    pub path: String,
    pub query: String,
}

#[derive(Debug)]
pub struct PublicCustomToolModel(pub crate::reviewer_kernel::kernel_types::ToolId);

#[derive(Debug)]
pub struct PublicJsonRpcReviewTool {
    pub provider_id: crate::reviewer_kernel::kernel_types::ToolProviderId,
    pub tool_id: String,
    pub expected_provider_resources: Vec<crate::reviewer_kernel::kernel_types::ProviderResourceId>,
    pub calls: Arc<AtomicUsize>,
}

pub fn register_test_custom_tool(
    registry: &mut ToolRegistry,
    id: &str,
    description: &str,
    provider_resources: Vec<ProviderResourceId>,
    effects: ToolEffects,
    handler: Arc<dyn CustomToolHandler>,
) -> ToolId {
    let tool_id = ToolId::parse(id).unwrap();
    registry
        .register_custom_with_alias_and_effects(
            tool_id.clone(),
            tool_id.clone(),
            description,
            test_tool_parameters(),
            CustomToolOptions {
                cacheable: false,
                effects,
                provider_resources,
            },
            handler,
        )
        .unwrap();
    tool_id
}

pub fn register_test_jsonrpc_tool(
    registry: &mut ToolRegistry,
    provider_id: ToolProviderId,
    id: &str,
    description: &str,
    provider_resources: Vec<ProviderResourceId>,
    effects: ToolEffects,
    transport: Arc<dyn JsonRpcToolTransport>,
) -> ToolId {
    let tool_id = ToolId::parse(id).unwrap();
    registry
        .register_jsonrpc_tool_with_alias(JsonRpcToolRegistration {
            provider_id,
            id: tool_id.clone(),
            model_alias: tool_id.clone(),
            description: description.to_string(),
            parameters: test_tool_parameters(),
            options: CustomToolOptions {
                cacheable: false,
                effects,
                provider_resources,
            },
            transport,
        })
        .unwrap();
    tool_id
}

fn test_tool_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "value": { "type": "string" }
        },
        "required": ["value"],
        "additionalProperties": false
    })
}

#[derive(Debug)]
pub struct CancellingModel {
    pub parent_cancel: tokio_util::sync::CancellationToken,
    pub calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub struct SingleExternalToolModel {
    pub tool_id: ToolId,
    pub calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub struct CancelAfterToolResultModel {
    pub tool_id: ToolId,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::reviewer_kernel::review_model::ReviewModel for CancellingModel {
    async fn complete_review(
        &self,
        _request: crate::reviewer_kernel::review_model::ReviewModelRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<
        crate::reviewer_kernel::review_model::ReviewModelTurn,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.parent_cancel.cancel();
        cancel.cancelled().await;
        Err(RuntimeError::Cancelled)
    }
}

#[async_trait]
impl ConcurrentModelClient for CancellingModel {
    async fn complete(
        &self,
        _scope: &SessionScope,
        _transcript: &[ConversationItem],
        _turn_id: TurnId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.parent_cancel.cancel();
        cancel.cancelled().await;
        Err(RuntimeError::Cancelled)
    }
}

#[async_trait]
impl ConcurrentModelClient for SingleExternalToolModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        _transcript: &[ConversationItem],
        turn_id: TurnId,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelTurn::ToolCalls {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cached_input_tokens: 0,
            },
            calls: vec![ModelToolCall {
                call_id: ToolCallId(format!("{}-{}-external", scope.id.0, turn_id.0)),
                index: 0,
                name: self.tool_id.clone(),
                raw_arguments: "{}".to_string(),
            }],
        })
    }
}

#[async_trait]
impl ConcurrentModelClient for CancelAfterToolResultModel {
    async fn complete(
        &self,
        scope: &SessionScope,
        _transcript: &[ConversationItem],
        turn_id: TurnId,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<ModelTurn> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelTurn::ToolCalls {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cached_input_tokens: 0,
            },
            calls: vec![ModelToolCall {
                call_id: ToolCallId(format!("{}-{}-cancel-after-success", scope.id.0, turn_id.0)),
                index: 0,
                name: self.tool_id.clone(),
                raw_arguments: "{}".to_string(),
            }],
        })
    }
}

#[async_trait]
impl crate::reviewer_kernel::review_model::ReviewModel for PublicFacadeModel {
    async fn complete_review(
        &self,
        request: crate::reviewer_kernel::review_model::ReviewModelRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<
        crate::reviewer_kernel::review_model::ReviewModelTurn,
    > {
        let usage = TokenUsage {
            input_tokens: request.transcript_item_count() as u64,
            output_tokens: 1,
            total_tokens: request.transcript_item_count() as u64 + 1,
            cached_input_tokens: 0,
        };
        if request.tool_result_count() == 0 && !review_request_is_final_turn(&request) {
            return Ok(
                crate::reviewer_kernel::review_model::ReviewModelTurn::ToolCalls {
                    usage,
                    calls: vec![
                        reviewer_call(&request, "diff", "read_diff", serde_json::json!({})),
                        reviewer_call(
                            &request,
                            "file",
                            "read_file",
                            serde_json::json!({ "path": self.path }),
                        ),
                        reviewer_call(
                            &request,
                            "search",
                            "search_text",
                            serde_json::json!({ "query": self.query }),
                        ),
                    ],
                },
            );
        }
        let content = if request.session_id.contains('/') {
            serde_json::json!({
                "status": if request.session_id.contains("/validate-") { "supported" } else { "insufficient" },
                "summary": "public facade structured child packet complete",
                "checkedPaths": [self.path],
                "evidence": [],
                "openQuestions": [],
                "suggestedNextSearches": [],
                "candidateFindings": []
            })
        } else {
            serde_json::json!({
                "verdict": "issues_found",
                "summary": "public facade structured review complete",
                "candidates": [{
                    "id": "public-facade-finding",
                    "title": "Changed marker no longer satisfies the lookup",
                    "claim": format!("The changed marker omits the required {} lookup value, so callers searching for it fail.", self.query),
                    "severity": "medium",
                    "path": self.path,
                    "startLine": 1,
                    "endLine": 1,
                    "behaviorBefore": format!("The reviewed file exposed the {} lookup value to callers.", self.query),
                    "behaviorAfter": format!("The reviewed file omits the {} lookup value and callers fail to find it.", self.query),
                    "evidenceArtifactIds": [],
                    "relatedPaths": []
                }],
                "notes": [],
                "completeness": {
                    "reviewedChangedFiles": [self.path],
                    "reviewedRiskEntries": [],
                    "unreviewedRiskEntries": [],
                    "unresolvedQuestions": [],
                    "incompleteReasons": [],
                    "ignoredChildCandidates": []
                }
            })
        };
        Ok(
            crate::reviewer_kernel::review_model::ReviewModelTurn::Text {
                usage,
                content: content.to_string(),
            },
        )
    }
}

#[async_trait]
impl crate::reviewer_kernel::review_model::ReviewModel for PublicCustomToolModel {
    async fn complete_review(
        &self,
        request: crate::reviewer_kernel::review_model::ReviewModelRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<
        crate::reviewer_kernel::review_model::ReviewModelTurn,
    > {
        let usage = TokenUsage {
            input_tokens: request.transcript_item_count() as u64,
            output_tokens: 1,
            total_tokens: request.transcript_item_count() as u64 + 1,
            cached_input_tokens: 0,
        };
        if request.tool_result_count() > 0 || review_request_is_final_turn(&request) {
            let content = if request.session_id.contains('/') {
                serde_json::json!({
                    "status": if request.session_id.contains("/validate-") { "supported" } else { "insufficient" },
                    "summary": "custom tool child packet complete",
                    "checkedPaths": [],
                    "evidence": [],
                    "openQuestions": [],
                    "suggestedNextSearches": [],
                    "candidateFindings": []
                })
            } else {
                serde_json::json!({
                    "verdict": "clean",
                    "summary": "custom tool completed",
                    "candidates": [],
                    "notes": [],
                    "completeness": {
                        "reviewedChangedFiles": [],
                        "reviewedRiskEntries": [],
                        "unreviewedRiskEntries": [],
                        "unresolvedQuestions": [],
                        "incompleteReasons": [],
                        "ignoredChildCandidates": []
                    }
                })
            };
            return Ok(
                crate::reviewer_kernel::review_model::ReviewModelTurn::Text {
                    content: content.to_string(),
                    usage,
                },
            );
        }
        Ok(
            crate::reviewer_kernel::review_model::ReviewModelTurn::ToolCalls {
                usage,
                calls: vec![crate::reviewer_kernel::review_model::ReviewToolCall::new(
                    self.0.as_str(),
                    serde_json::json!({ "value": "ok" }),
                )
                .with_call_id(request.tool_call_id("custom"))],
            },
        )
    }
}

fn review_request_is_final_turn(
    request: &crate::reviewer_kernel::review_model::ReviewModelRequest,
) -> bool {
    request.transcript.iter().any(|item| match item {
        crate::reviewer_kernel::review_model::ReviewTranscriptItem::User { content } => {
            content.starts_with("Return the final ")
        }
        _ => false,
    })
}

#[async_trait]
impl JsonRpcToolTransport for PublicJsonRpcReviewTool {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<JsonRpcToolResponse> {
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id.as_str(), self.tool_id);
        assert_eq!(request.provider_resources, self.expected_provider_resources);
        assert_eq!(request.arguments["value"], "ok");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JsonRpcToolResponse {
            data: Some(serde_json::json!({
                "provider": request.provider_id.as_str(),
                "tool": request.tool_id.as_str(),
                "value": request.arguments["value"]
            })),
            artifact: None,
            limits: crate::reviewer_kernel::kernel_types::LimitInfo::default(),
        })
    }
}

#[derive(Clone)]
pub struct SharedWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct ResourceScopedReviewTool {
    pub expected_provider_resources: Vec<ProviderResourceId>,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl CustomToolHandler for ResourceScopedReviewTool {
    async fn execute(
        &self,
        context: CustomToolContext,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        assert_eq!(context.provider_resources, self.expected_provider_resources);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CustomToolOutput {
            data: Some(serde_json::json!({
                "resources": context
                    .provider_resources
                    .iter()
                    .map(|resource| resource.as_str())
                    .collect::<Vec<_>>()
            })),
            artifact: None,
            limits: LimitInfo::default(),
        })
    }
}

#[async_trait]
impl CustomToolHandler for EchoCustomTool {
    async fn execute(
        &self,
        context: CustomToolContext,
        args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        Ok(CustomToolOutput {
            data: Some(serde_json::json!({
                "tool": context.tool_id.as_str(),
                "session": context.session_id.0,
                "value": args["value"],
                "secret": "AKIA1234567890ABCDEF"
            })),
            artifact: Some(CustomToolArtifact {
                key: ArtifactKey("host_custom_check".to_string()),
                content: "artifact AKIA1234567890ABCDEF".to_string(),
            }),
            limits: LimitInfo::default(),
        })
    }
}

pub struct CancelAfterSuccessCustomTool {
    pub parent_cancel: tokio_util::sync::CancellationToken,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl CustomToolHandler for CancelAfterSuccessCustomTool {
    async fn execute(
        &self,
        context: CustomToolContext,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.parent_cancel.cancel();
        Ok(CustomToolOutput {
            data: Some(serde_json::json!({
                "tool": context.tool_id.as_str(),
                "cancelled": true
            })),
            artifact: None,
            limits: LimitInfo::default(),
        })
    }
}

pub struct SlowCustomTool;

#[async_trait]
impl CustomToolHandler for SlowCustomTool {
    async fn execute(
        &self,
        _context: CustomToolContext,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(CustomToolOutput::default())
    }
}

pub struct EchoJsonRpcTransport {
    pub provider_id: ToolProviderId,
    pub tool_id: ToolId,
    pub calls: Arc<AtomicUsize>,
}

pub struct StaticJsonRpcTransport {
    pub provider_id: ToolProviderId,
    pub tool_id: ToolId,
    pub calls: Arc<AtomicUsize>,
    pub response: JsonRpcToolResponse,
}

pub struct ResourceCheckingJsonRpcTransport {
    pub provider_id: ToolProviderId,
    pub tool_id: ToolId,
    pub calls: Arc<AtomicUsize>,
    pub expected_provider_resources: Vec<ProviderResourceId>,
    pub response: JsonRpcToolResponse,
}

pub struct CancellingJsonRpcTransport {
    pub provider_id: ToolProviderId,
    pub tool_id: ToolId,
    pub parent_cancel: tokio_util::sync::CancellationToken,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl JsonRpcToolTransport for EchoJsonRpcTransport {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse> {
        assert!(!cancel.is_cancelled());
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id, self.tool_id);
        assert_eq!(request.arguments["value"], "ok");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JsonRpcToolResponse {
            data: Some(serde_json::json!({
                "value": request.arguments["value"],
                "secret": "AKIA1234567890ABCDEF"
            })),
            artifact: Some(CustomToolArtifact {
                key: ArtifactKey("external_jsonrpc_artifact".to_string()),
                content: "external AKIA1234567890ABCDEF".to_string(),
            }),
            limits: LimitInfo::default(),
        })
    }
}

#[async_trait]
impl JsonRpcToolTransport for StaticJsonRpcTransport {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse> {
        assert!(!cancel.is_cancelled());
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id, self.tool_id);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

#[async_trait]
impl JsonRpcToolTransport for ResourceCheckingJsonRpcTransport {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse> {
        assert!(!cancel.is_cancelled());
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id, self.tool_id);
        assert_eq!(request.provider_resources, self.expected_provider_resources);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

#[async_trait]
impl JsonRpcToolTransport for CancellingJsonRpcTransport {
    async fn call(
        &self,
        request: JsonRpcToolRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> RuntimeResult<JsonRpcToolResponse> {
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id, self.tool_id);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.parent_cancel.cancel();
        cancel.cancelled().await;
        Err(RuntimeError::Cancelled)
    }
}

pub struct CountingSlowCustomTool {
    pub active: Arc<AtomicUsize>,
    pub max_seen: Arc<AtomicUsize>,
}

#[async_trait]
impl CustomToolHandler for CountingSlowCustomTool {
    async fn execute(
        &self,
        _context: CustomToolContext,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(CustomToolOutput::default())
    }
}

pub struct PanicCustomTool;

#[async_trait]
impl CustomToolHandler for PanicCustomTool {
    async fn execute(
        &self,
        _context: CustomToolContext,
        _args: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::reviewer_kernel::kernel_types::RuntimeResult<CustomToolOutput> {
        panic!("intentional custom tool panic")
    }
}

pub fn reviewer_call(
    request: &crate::reviewer_kernel::review_model::ReviewModelRequest,
    suffix: &str,
    tool_id: &str,
    arguments: serde_json::Value,
) -> crate::reviewer_kernel::review_model::ReviewToolCall {
    crate::reviewer_kernel::review_model::ReviewToolCall::new(tool_id, arguments)
        .with_call_id(request.tool_call_id(suffix))
}

pub fn public_budget() -> crate::reviewer_kernel::review_contract::AgentBudget {
    crate::reviewer_kernel::review_contract::AgentBudget {
        max_turns: 4,
        max_tool_calls: 8,
        max_prompt_tokens: 32_000,
        max_output_tokens: 512,
        budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
    }
}

pub fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "connection closed before complete HTTP request");
        request.extend_from_slice(&buffer[..read]);
        if let Some((headers, body)) = try_split_http_body(&request) {
            let content_length = http_content_length(headers);
            if body.len() >= content_length {
                return request;
            }
        }
    }
}

pub fn split_http_body(request: &[u8]) -> (&str, &[u8]) {
    try_split_http_body(request).expect("HTTP request missing header terminator")
}

fn try_split_http_body(request: &[u8]) -> Option<(&str, &[u8])> {
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
    let body = &request[header_end + 4..];
    Some((headers, body))
}

pub fn http_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0)
}

pub fn test_scope(id: &str) -> SessionScope {
    test_scope_with_capabilities(id, CapabilitySet::review_read_only())
}

pub fn search_call(call_id: &str) -> ModelToolCall {
    ModelToolCall {
        call_id: ToolCallId(call_id.to_string()),
        index: 0,
        name: ToolId::from(ToolName::SearchText),
        raw_arguments: r#"{"query":"needle"}"#.to_string(),
    }
}

pub async fn wait_for_inflight_tool(engine: &ToolEngine) {
    for _ in 0..50 {
        if engine.inflight_tool_count_for_test() > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for in-flight tool cell");
}

pub async fn wait_for_search_dedupe_waiter(engine: &ToolEngine) {
    for _ in 0..50 {
        if engine.snapshot_counters().search_dedupe_waiters > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for search dedupe waiter");
}

pub fn trusted_custom_capabilities() -> CapabilitySet {
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.runtime_authority.host_read = true;
    capabilities
}

pub fn custom_read_only_grant() -> ToolGrant {
    ToolGrant {
        allow: true,
        max_calls: None,
        effects_allowed: ToolEffects::custom_read_only(),
    }
}

pub fn builtin_metric_key(tool: ToolName) -> ToolMetricKey {
    ToolMetricKey::new(&ToolProviderId::builtin_review(), &ToolId::from(tool))
}

pub fn in_process_metric_key(tool_id: &ToolId) -> ToolMetricKey {
    ToolMetricKey::new(&ToolProviderId::in_process(), tool_id)
}

pub fn test_scope_with_capabilities(id: &str, capabilities: CapabilitySet) -> SessionScope {
    SessionScope {
        id: SessionId(id.to_string()),
        role: Role::Generalist,
        objective: "test review scope".to_string(),
        instructions: Vec::new(),
        snapshot_id: None,
        model_profile_id: Some("test-model".to_string()),
        response_format: None,
        capabilities,
        budget: AgentBudget {
            max_turns: 4,
            max_tool_calls: 8,
            max_prompt_tokens: 32_000,
            max_output_tokens: 512,
            budget_source: crate::reviewer_kernel::review_contract::BudgetSource::PlannedDefault,
        },
    }
}

pub fn test_change_with_file(path: &str) -> ChangeScopeV1 {
    ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "test".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: None,
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files: vec![ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from(path)),
            new_path: Some(PathBuf::from(path)),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        }],
    }
}
