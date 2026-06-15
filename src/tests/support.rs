use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::reviewer_kernel::kernel_types::{
    ArtifactKey, CapabilitySet, ConversationItem, LimitInfo, ModelToolCall, ModelTurn,
    ProviderResourceId, RuntimeError, RuntimeResult, SessionId, SessionScope, ToolCallId, ToolId,
    ToolProviderId, TurnId,
};
use crate::reviewer_kernel::model::ConcurrentModelClient;
use crate::reviewer_kernel::review_contract::*;
use crate::reviewer_kernel::tool_engine::ToolEngine;
use crate::reviewer_kernel::tool_engine::{
    CustomToolArtifact, CustomToolContext, CustomToolHandler, CustomToolOutput, JsonRpcToolRequest,
    JsonRpcToolResponse, JsonRpcToolTransport,
};
use async_trait::async_trait;

#[derive(Debug)]
pub struct EchoCustomTool;

#[derive(Debug)]
pub struct PublicFacadeModel {
    pub path: String,
    pub query: String,
}

#[derive(Debug)]
pub struct PublicCustomToolModel(pub crate::reviewer_kernel::adapters::ids::ToolId);

#[derive(Debug)]
pub struct PublicJsonRpcReviewTool {
    pub provider_id: crate::reviewer_kernel::adapters::tool_adapters::ToolProviderId,
    pub tool_id: String,
    pub expected_provider_resources:
        Vec<crate::reviewer_kernel::adapters::tool_adapters::ProviderResourceId>,
    pub calls: Arc<AtomicUsize>,
}

pub struct LoopbackJsonRpcToolServer {
    pub endpoint: String,
    pub handle: std::thread::JoinHandle<serde_json::Value>,
}

impl LoopbackJsonRpcToolServer {
    pub fn spawn() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request_bytes = read_http_request(&mut stream);
            let (headers, body) = split_http_body(&request_bytes);
            let content_length = http_content_length(headers);
            let request: serde_json::Value =
                serde_json::from_slice(&body[..content_length]).unwrap();
            let result = serde_json::to_value(JsonRpcToolResponse {
                data: Some(serde_json::json!({
                    "wire": "ok",
                    "value": request["params"]["arguments"]["value"].clone()
                })),
                artifact: None,
                limits: LimitInfo::default(),
            })
            .unwrap();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": result
            });
            let response_body = serde_json::to_vec(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            )
            .unwrap();
            stream.write_all(&response_body).unwrap();
            request
        });
        Self { endpoint, handle }
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn join(self) -> serde_json::Value {
        self.handle.join().expect("loopback JSON-RPC server")
    }
}

pub fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request_bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let bytes_read = stream.read(&mut chunk).unwrap();
        if bytes_read == 0 {
            break;
        }
        request_bytes.extend_from_slice(&chunk[..bytes_read]);
        if let Some((headers, body)) = try_split_http_body(&request_bytes) {
            let content_length = http_content_length(headers);
            if body.len() >= content_length {
                break;
            }
        }
    }
    request_bytes
}

pub fn split_http_body(request_bytes: &[u8]) -> (&str, &[u8]) {
    try_split_http_body(request_bytes).expect("complete HTTP request")
}

pub fn try_split_http_body(request_bytes: &[u8]) -> Option<(&str, &[u8])> {
    let body_start = request_bytes
        .windows(b"\r\n\r\n".len())
        .position(|window| window == b"\r\n\r\n")?
        + b"\r\n\r\n".len();
    let headers = std::str::from_utf8(&request_bytes[..body_start]).ok()?;
    Some((headers, &request_bytes[body_start..]))
}

pub fn http_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().ok();
            }
            None
        })
        .expect("HTTP content-length")
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
        cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
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
        _cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
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
        _cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
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
impl crate::reviewer_kernel::adapters::tool_adapters::JsonRpcToolTransport
    for PublicJsonRpcReviewTool
{
    async fn call(
        &self,
        request: crate::reviewer_kernel::adapters::tool_adapters::JsonRpcToolRequest,
        _cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
        crate::reviewer_kernel::adapters::tool_adapters::JsonRpcToolResponse,
    > {
        assert_eq!(request.provider_id, self.provider_id);
        assert_eq!(request.tool_id.as_str(), self.tool_id);
        assert_eq!(request.provider_resources, self.expected_provider_resources);
        assert_eq!(request.arguments["value"], "ok");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(
            crate::reviewer_kernel::adapters::tool_adapters::JsonRpcToolResponse {
                data: Some(serde_json::json!({
                    "provider": request.provider_id.as_str(),
                    "tool": request.tool_id.as_str(),
                    "value": request.arguments["value"]
                })),
                artifact: None,
                limits: crate::reviewer_kernel::adapters::metrics::LimitInfo::default(),
            },
        )
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

#[async_trait]
impl crate::reviewer_kernel::review_tools::ReviewToolHandler for EchoCustomTool {
    async fn execute_review_tool(
        &self,
        context: crate::reviewer_kernel::review_tools::ReviewToolContext,
        args: serde_json::Value,
        _cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
        crate::reviewer_kernel::review_tools::ReviewToolOutput,
    > {
        Ok(crate::reviewer_kernel::review_tools::ReviewToolOutput {
            data: Some(serde_json::json!({
                "tool": context.tool_id,
                "session": context.session_id,
                "value": args["value"],
                "secret": "AKIA1234567890ABCDEF"
            })),
            artifact: Some(crate::reviewer_kernel::review_tools::ReviewToolArtifact {
                key: "host_custom_check".to_string(),
                content: "artifact AKIA1234567890ABCDEF".to_string(),
            }),
        })
    }
}

pub struct ResourceScopedReviewTool {
    pub expected_provider_resources: Vec<ProviderResourceId>,
    pub calls: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::reviewer_kernel::review_tools::ReviewToolHandler for ResourceScopedReviewTool {
    async fn execute_review_tool(
        &self,
        context: crate::reviewer_kernel::review_tools::ReviewToolContext,
        _args: serde_json::Value,
        _cancel: crate::reviewer_kernel::adapters::Cancellation,
    ) -> crate::reviewer_kernel::adapters::runtime::RuntimeResult<
        crate::reviewer_kernel::review_tools::ReviewToolOutput,
    > {
        assert_eq!(context.provider_resources, self.expected_provider_resources);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(crate::reviewer_kernel::review_tools::ReviewToolOutput {
            data: Some(serde_json::json!({
                "resources": context
                    .provider_resources
                    .iter()
                    .map(|resource| resource.as_str())
                    .collect::<Vec<_>>()
            })),
            artifact: None,
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
pub fn passing_model_provider_canary_evidence(
) -> crate::reviewer_kernel::canaries::ModelProviderCanaryEvidence {
    passing_model_provider_canary_evidence_at(&crate::reviewer_kernel::system::timestamp_utc())
}

pub fn current_passing_canary_manifest() -> crate::reviewer_kernel::canaries::CanaryEvidenceManifest
{
    let now = crate::reviewer_kernel::system::timestamp_utc();
    passing_canary_manifest_at(&now)
}

pub fn passing_canary_manifest_at(
    generated_at_utc: &str,
) -> crate::reviewer_kernel::canaries::CanaryEvidenceManifest {
    let model_provider = passing_model_provider_canary_evidence_at(generated_at_utc);
    let snapshot_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let artifact_client =
        Arc::new(crate::reviewer_kernel::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let mut snapshot = crate::reviewer_kernel::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        snapshot_client.as_ref(),
    );
    let mut artifact = crate::reviewer_kernel::canaries::run_remote_artifact_object_store_canary(
        "s3://muzen-test-artifacts/canary",
        artifact_client.as_ref(),
    );
    snapshot.generated_at_utc = generated_at_utc.to_string();
    artifact.generated_at_utc = generated_at_utc.to_string();
    crate::reviewer_kernel::canaries::CanaryEvidenceManifest::with_generated_at(
        generated_at_utc,
        Some(model_provider),
        vec![snapshot, artifact],
    )
}

pub fn passing_model_provider_canary_evidence_at(
    generated_at_utc: &str,
) -> crate::reviewer_kernel::canaries::ModelProviderCanaryEvidence {
    let reports = crate::reviewer_kernel::canaries::openai_provider_canary_protocols()
        .iter()
        .map(
            |protocol| crate::reviewer_kernel::canaries::ModelProviderCanaryReport {
                protocol: *protocol,
                base_url: "https://example.invalid/v1".to_string(),
                model: "canary-model".to_string(),
                credential_ref: "env:OPENAI_API_KEY".to_string(),
                status: crate::reviewer_kernel::canaries::ModelProviderCanaryStatus::Passed,
            },
        )
        .collect::<Vec<_>>();
    crate::reviewer_kernel::canaries::ModelProviderCanaryEvidence::with_generated_at(
        generated_at_utc,
        reports,
    )
}
