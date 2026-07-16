use std::collections::BTreeSet;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::agent_runtime::{
    AgentDefinition, AgentMessage, ContentBlock, ExecutionErrorCode, MessageRole, ModelProfile,
    ToolProvider, ToolProviderId, Usage,
};

/// Internal persistence envelope used only to reconstruct provider tool-call history.
/// LocalRuntime removes these blocks from public `messages()` pages, while the engine
/// reads the unprojected store transcript.
pub(crate) const ASSISTANT_TOOL_ENVELOPE: &str = "assistant_tool_calls";
const UNRESOLVED_TOOL_PROVIDER: &str = "__muzen_unresolved_tool__";

// v1 intentionally does no proactive input token counting. The provider API
// enforces its own input limit; Muzen only sends the configured output cap.

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub agent: AgentDefinition,
    pub model: ModelProfile,
    pub transcript: Vec<AgentMessage>,
    pub tool_providers: Vec<ToolProvider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStop {
    EndTurn,
    ToolUse,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelToolCall {
    pub id: String,
    pub provider: ToolProviderId,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: Usage,
    pub stop: ModelStop,
}

#[derive(Debug, Clone)]
pub struct ModelProviderError {
    message: String,
    retryable: bool,
    details: Option<Value>,
    code: ExecutionErrorCode,
}

impl ModelProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            details: None,
            code: ExecutionErrorCode::ModelError,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_code(mut self, code: ExecutionErrorCode) -> Self {
        self.code = code;
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }

    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    pub fn code(&self) -> ExecutionErrorCode {
        self.code
    }
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelTurn, ModelProviderError>;
}

pub(super) fn anthropic_request(request: &ModelRequest) -> Value {
    let mut body = Map::from_iter([
        ("model".to_owned(), json!(request.model.model)),
        (
            "max_tokens".to_owned(),
            json!(request.model.max_output_tokens.get()),
        ),
        (
            "system".to_owned(),
            json!(text_blocks(&request.agent.instructions)),
        ),
        ("messages".to_owned(), json!(anthropic_messages(request))),
    ]);
    add_sampling(&mut body, &request.model);
    let tools = anthropic_tools(request);
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}

pub(super) fn chat_request(request: &ModelRequest) -> Value {
    let mut messages = vec![json!({
        "role": "system",
        "content": text_blocks(&request.agent.instructions),
    })];
    messages.extend(openai_messages(request));
    let mut body = Map::from_iter([
        ("model".to_owned(), json!(request.model.model)),
        ("messages".to_owned(), Value::Array(messages)),
        (
            "max_tokens".to_owned(),
            json!(request.model.max_output_tokens.get()),
        ),
    ]);
    add_sampling(&mut body, &request.model);
    let tools = openai_tools(request);
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}

pub(super) fn responses_request(request: &ModelRequest) -> Value {
    let mut input = vec![json!({
        "role": "system",
        "content": text_blocks(&request.agent.instructions),
    })];
    input.extend(responses_input(request));
    let mut body = Map::from_iter([
        ("model".to_owned(), json!(request.model.model)),
        ("input".to_owned(), Value::Array(input)),
        (
            "max_output_tokens".to_owned(),
            json!(request.model.max_output_tokens.get()),
        ),
    ]);
    add_sampling(&mut body, &request.model);
    let tools = responses_tools(request);
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}

fn add_sampling(body: &mut Map<String, Value>, model: &ModelProfile) {
    if let Some(value) = model.temperature {
        body.insert("temperature".to_owned(), json!(value));
    }
    if let Some(value) = model.top_p {
        body.insert("top_p".to_owned(), json!(value));
    }
}

fn text_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn visible_builtin_grants(request: &ModelRequest) -> Vec<(&ToolProviderId, &str)> {
    request
        .agent
        .tools
        .iter()
        .filter(|grant| {
            request.tool_providers.iter().any(
                |provider| matches!(provider, ToolProvider::Builtin { id } if id == &grant.provider),
            ) && matches!(grant.tool.as_str(), "agent.spawn" | "agent.message")
        })
        .map(|grant| (&grant.provider, grant.tool.as_str()))
        .collect()
}

fn wire_tool_name(canonical: &str) -> &str {
    match canonical {
        "agent.spawn" => "agent_spawn",
        "agent.message" => "agent_message",
        name => name,
    }
}

fn canonical_tool_name(wire: &str) -> &str {
    match wire {
        "agent_spawn" => "agent.spawn",
        "agent_message" => "agent.message",
        name => name,
    }
}

fn tool_schema(name: &str) -> Value {
    // Model-facing built-ins advertise the same lenient text forms normalized by
    // `engine::execute_tool`: a string or an explicit text ContentBlock.
    let text_block = json!({
        "type": "object",
        "properties": {
            "type": { "const": "text" },
            "text": { "type": "string" }
        },
        "required": ["type", "text"],
        "additionalProperties": false
    });
    let content_blocks = json!({
        "oneOf": [
            { "type": "string" },
            {
                "type": "array",
                "items": { "oneOf": [{ "type": "string" }, text_block] }
            }
        ]
    });
    let content = json!({
        "type": "object",
        "properties": { "content": content_blocks },
        "required": ["content"],
        "additionalProperties": false
    });
    let input = json!({ "oneOf": [{ "type": "string" }, content] });
    match name {
        "agent.spawn" => json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "minLength": 1 },
                        "instructions": content_blocks,
                        "model": { "type": "string", "minLength": 1 },
                        "tools": { "type": "array" },
                        "budget": { "type": "object" },
                        "output": { "type": "object" },
                        "metadata": { "type": "object" }
                    },
                    "required": ["name", "instructions", "model", "tools"],
                    "additionalProperties": false
                },
                "input": input,
                "idempotencyKey": { "type": "string" }
            },
            "required": ["agent", "input"],
            "additionalProperties": false
        }),
        "agent.message" => json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string" },
                "input": input,
                "delivery": { "type": "string", "enum": ["steer", "follow_up"] },
                "idempotencyKey": { "type": "string" }
            },
            "required": ["sessionId", "input", "delivery"],
            "additionalProperties": false
        }),
        _ => json!({ "type": "object" }),
    }
}

fn anthropic_tools(request: &ModelRequest) -> Vec<Value> {
    visible_builtin_grants(request)
        .into_iter()
        .map(|(_, name)| json!({ "name": wire_tool_name(name), "input_schema": tool_schema(name) }))
        .collect()
}

fn openai_tools(request: &ModelRequest) -> Vec<Value> {
    visible_builtin_grants(request)
        .into_iter()
        .map(|(_, name)| {
            json!({
                "type": "function",
                "function": { "name": wire_tool_name(name), "parameters": tool_schema(name) }
            })
        })
        .collect()
}

fn responses_tools(request: &ModelRequest) -> Vec<Value> {
    visible_builtin_grants(request)
        .into_iter()
        .map(|(_, name)| {
            json!({
                "type": "function", "name": wire_tool_name(name), "parameters": tool_schema(name)
            })
        })
        .collect()
}

fn assistant_calls(message: &AgentMessage) -> Option<Vec<Value>> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|value| value["_muzen"] == ASSISTANT_TOOL_ENVELOPE)
            .and_then(|value| value["calls"].as_array().cloned()),
        _ => None,
    })
}

fn visible_text(message: &AgentMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text }
                if serde_json::from_str::<Value>(text)
                    .ok()
                    .is_none_or(|value| value["_muzen"] != ASSISTANT_TOOL_ENVELOPE) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result(message: &AgentMessage) -> Option<Value> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => serde_json::from_str(text).ok(),
        _ => None,
    })
}

fn legacy_call(envelope: &Value) -> Value {
    json!({
        "id": envelope["callId"],
        "provider": envelope["provider"],
        "name": envelope["tool"],
        "arguments": envelope.get("arguments").cloned().unwrap_or_else(|| json!({})),
    })
}

fn openai_call(call: &Value) -> Value {
    let name = call["name"].as_str().unwrap_or_default();
    json!({
        "id": call["id"],
        "type": "function",
        "function": {
            "name": wire_tool_name(name),
            "arguments": serde_json::to_string(&call["arguments"]).unwrap_or_else(|_| "{}".to_owned())
        }
    })
}

fn openai_messages(request: &ModelRequest) -> Vec<Value> {
    let mut result = Vec::new();
    let mut pending_calls = BTreeSet::new();
    for message in &request.transcript {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                result.push(json!({ "role": "user", "content": visible_text(message) }));
                pending_calls.clear();
            }
            MessageRole::Assistant => {
                let calls = assistant_calls(message).unwrap_or_default();
                let mut value = json!({ "role": "assistant", "content": visible_text(message) });
                if !calls.is_empty() {
                    value["tool_calls"] = Value::Array(calls.iter().map(openai_call).collect());
                }
                pending_calls = calls
                    .iter()
                    .filter_map(|call| call["id"].as_str().map(str::to_owned))
                    .collect();
                result.push(value);
            }
            MessageRole::Tool => {
                let Some(envelope) = tool_result(message) else {
                    continue;
                };
                let call_id = envelope["callId"].as_str().unwrap_or_default();
                if !pending_calls.remove(call_id) {
                    result.push(json!({
                        "role": "assistant", "content": "",
                        "tool_calls": [openai_call(&legacy_call(&envelope))]
                    }));
                }
                let content = envelope
                    .get("result")
                    .or_else(|| envelope.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                result.push(json!({
                    "role": "tool",
                    "tool_call_id": envelope["callId"],
                    "content": serde_json::to_string(&content).unwrap_or_else(|_| "null".to_owned())
                }));
            }
        }
    }
    result
}

fn anthropic_messages(request: &ModelRequest) -> Vec<Value> {
    let mut result = Vec::new();
    let mut pending_calls = BTreeSet::new();
    for message in &request.transcript {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                result.push(json!({ "role": "user", "content": visible_text(message) }));
                pending_calls.clear();
            }
            MessageRole::Assistant => {
                let calls = assistant_calls(message).unwrap_or_default();
                let mut content = Vec::new();
                let text = visible_text(message);
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                content.extend(calls.iter().map(|call| {
                    let name = call["name"].as_str().unwrap_or_default();
                    json!({
                        "type": "tool_use", "id": call["id"], "name": wire_tool_name(name),
                        "input": call["arguments"]
                    })
                }));
                pending_calls = calls
                    .iter()
                    .filter_map(|call| call["id"].as_str().map(str::to_owned))
                    .collect();
                result.push(json!({ "role": "assistant", "content": content }));
            }
            MessageRole::Tool => {
                let Some(envelope) = tool_result(message) else {
                    continue;
                };
                let call_id = envelope["callId"].as_str().unwrap_or_default();
                if !pending_calls.remove(call_id) {
                    let call = legacy_call(&envelope);
                    let name = call["name"].as_str().unwrap_or_default();
                    result.push(json!({ "role": "assistant", "content": [{
                        "type": "tool_use", "id": call["id"], "name": wire_tool_name(name),
                        "input": call["arguments"]
                    }] }));
                }
                let content = envelope
                    .get("result")
                    .or_else(|| envelope.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let tool_result = json!({
                    "type": "tool_result", "tool_use_id": envelope["callId"],
                    "content": serde_json::to_string(&content).unwrap_or_else(|_| "null".to_owned()),
                    "is_error": envelope.get("error").is_some()
                });
                if let Some(previous) = result.last_mut().filter(|previous| {
                    previous["role"] == "user"
                        && previous["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().all(|block| block["type"] == "tool_result")
                        })
                }) {
                    previous["content"]
                        .as_array_mut()
                        .expect("tool result content")
                        .push(tool_result);
                } else {
                    result.push(json!({ "role": "user", "content": [tool_result] }));
                }
            }
        }
    }
    result
}

fn responses_input(request: &ModelRequest) -> Vec<Value> {
    let mut result = Vec::new();
    let mut pending_calls = BTreeSet::new();
    for message in &request.transcript {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                result.push(json!({ "role": "user", "content": visible_text(message) }));
                pending_calls.clear();
            }
            MessageRole::Assistant => {
                let text = visible_text(message);
                if !text.is_empty() {
                    result.push(json!({ "role": "assistant", "content": text }));
                }
                let calls = assistant_calls(message).unwrap_or_default();
                result.extend(calls.iter().map(|call| {
                    let name = call["name"].as_str().unwrap_or_default();
                    json!({
                        "type": "function_call", "call_id": call["id"],
                        "name": wire_tool_name(name),
                        "arguments": serde_json::to_string(&call["arguments"]).unwrap_or_else(|_| "{}".to_owned())
                    })
                }));
                pending_calls = calls
                    .iter()
                    .filter_map(|call| call["id"].as_str().map(str::to_owned))
                    .collect();
            }
            MessageRole::Tool => {
                let Some(envelope) = tool_result(message) else {
                    continue;
                };
                let call_id = envelope["callId"].as_str().unwrap_or_default();
                if !pending_calls.remove(call_id) {
                    let call = legacy_call(&envelope);
                    let name = call["name"].as_str().unwrap_or_default();
                    result.push(json!({
                        "type": "function_call", "call_id": call["id"],
                        "name": wire_tool_name(name),
                        "arguments": serde_json::to_string(&call["arguments"]).unwrap_or_else(|_| "{}".to_owned())
                    }));
                }
                let output = envelope
                    .get("result")
                    .or_else(|| envelope.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                result.push(json!({
                    "type": "function_call_output", "call_id": envelope["callId"],
                    "output": serde_json::to_string(&output).unwrap_or_else(|_| "null".to_owned())
                }));
            }
        }
    }
    result
}

fn tool_provider(request: &ModelRequest, name: &str) -> ToolProviderId {
    visible_builtin_grants(request)
        .into_iter()
        .find(|(_, tool)| *tool == name)
        .map(|(provider, _)| provider.clone())
        .unwrap_or_else(|| {
            ToolProviderId::new(UNRESOLVED_TOOL_PROVIDER).expect("valid unresolved provider id")
        })
}

fn parse_arguments(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| value.clone())
}

pub(super) fn parse_anthropic(
    request: &ModelRequest,
    value: Value,
) -> Result<ModelTurn, ModelProviderError> {
    let blocks = value["content"]
        .as_array()
        .ok_or_else(|| ModelProviderError::new("Anthropic response omitted content"))?;
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => content.push(ContentBlock::Text {
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("tool_use") => {
                let name = canonical_tool_name(block["name"].as_str().unwrap_or_default());
                tool_calls.push(ModelToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    provider: tool_provider(request, name),
                    name: name.to_owned(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                });
            }
            _ => {}
        }
    }
    Ok(turn(
        content,
        tool_calls,
        value["usage"]["input_tokens"].as_u64().unwrap_or(0),
        value["usage"]["output_tokens"].as_u64().unwrap_or(0),
    ))
}

pub(super) fn parse_chat(
    request: &ModelRequest,
    value: Value,
) -> Result<ModelTurn, ModelProviderError> {
    let choice = value["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .ok_or_else(|| ModelProviderError::new("chat completions response omitted choices"))?;
    let message = &choice["message"];
    let content = message["content"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(|text| {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        })
        .unwrap_or_default();
    let tool_calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| {
            let name = canonical_tool_name(call["function"]["name"].as_str().unwrap_or_default());
            ModelToolCall {
                id: call["id"].as_str().unwrap_or_default().to_owned(),
                provider: tool_provider(request, name),
                name: name.to_owned(),
                arguments: parse_arguments(&call["function"]["arguments"]),
            }
        })
        .collect::<Vec<_>>();
    Ok(turn(
        content,
        tool_calls,
        value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    ))
}

pub(super) fn parse_responses(
    request: &ModelRequest,
    value: Value,
) -> Result<ModelTurn, ModelProviderError> {
    let output = value["output"]
        .as_array()
        .ok_or_else(|| ModelProviderError::new("responses response omitted output"))?;
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for block in item["content"].as_array().into_iter().flatten() {
                    if matches!(block["type"].as_str(), Some("output_text") | Some("text")) {
                        content.push(ContentBlock::Text {
                            text: block["text"].as_str().unwrap_or_default().to_owned(),
                        });
                    }
                }
            }
            Some("function_call") => {
                let name = canonical_tool_name(item["name"].as_str().unwrap_or_default());
                tool_calls.push(ModelToolCall {
                    id: item["call_id"]
                        .as_str()
                        .or_else(|| item["id"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    provider: tool_provider(request, name),
                    name: name.to_owned(),
                    arguments: parse_arguments(&item["arguments"]),
                });
            }
            _ => {}
        }
    }
    Ok(turn(
        content,
        tool_calls,
        value["usage"]["input_tokens"].as_u64().unwrap_or(0),
        value["usage"]["output_tokens"].as_u64().unwrap_or(0),
    ))
}

fn turn(
    content: Vec<ContentBlock>,
    tool_calls: Vec<ModelToolCall>,
    input_tokens: u64,
    output_tokens: u64,
) -> ModelTurn {
    let stop = if tool_calls.is_empty() {
        ModelStop::EndTurn
    } else {
        ModelStop::ToolUse
    };
    ModelTurn {
        content,
        tool_calls,
        usage: Usage {
            input_tokens,
            output_tokens,
            tool_calls: 0,
        },
        stop,
    }
}
