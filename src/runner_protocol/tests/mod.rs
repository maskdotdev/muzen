use std::io::{Result as IoResult, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use super::failures::{RunFailedNotification, RunnerFailureKind, RunnerRetryHint};
use super::schema_types::{RunnerMethodSchema, RunnerMethodStatus};
use super::*;

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
        self.0.lock().expect("writer lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

#[test]
fn handshake_returns_protocol_version() {
    let response = handle_jsonrpc_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"runner.handshake","params":{"protocolVersion":"muzen.runner.v1","clientName":"test"}}"#,
    );

    assert!(response.error.is_none());
    assert_eq!(response.id, Some(json!(1)));
    assert_eq!(
        response
            .result
            .as_ref()
            .and_then(|value| value.get("protocolVersion")),
        Some(&json!(RUNNER_PROTOCOL_VERSION))
    );
}

#[test]
fn handshake_rejects_protocol_mismatch() {
    let response = handle_jsonrpc_line(
        r#"{"jsonrpc":"2.0","id":"bad","method":"runner.handshake","params":{"protocolVersion":"muzen.runner.v0"}}"#,
    );

    let error = response.error.expect("protocol error");
    assert_eq!(error.data.expect("error data").kind, "protocol_error");
}

#[test]
fn schema_fixture_matches_current_schema() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/runner-schema-v1.json"))
            .expect("schema fixture");
    let current = serde_json::to_value(protocol_schema()).expect("current runner schema");

    assert_eq!(current, fixture);
}

#[test]
fn handshake_fixture_matches_current_response() {
    let fixture_line = include_str!("../../../fixtures/runner-handshake-v1.jsonl")
        .lines()
        .nth(1)
        .expect("handshake response fixture");
    let fixture: serde_json::Value = serde_json::from_str(fixture_line).expect("handshake fixture");
    let current = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": runner_handshake()
    });

    assert_eq!(current, fixture);
}

#[test]
fn schema_marks_wired_run_methods_and_callbacks_implemented() {
    let schema = protocol_schema();
    for method in [
        "runner.handshake",
        "runner.check",
        "runner.schema.export",
        "run.start",
        "run.cancel",
        "run.status",
        "run.result",
        "artifact.read",
        "artifact.export",
        "snapshot.readText",
        "context.index",
        "context.pack",
        "context.query",
        "context.feedback",
        "context.learning.approve",
        "webhook.github.handle",
        "webhook.gitlab.handle",
        "worker.runOnce",
    ] {
        assert_method_status(&schema.requests, method, RunnerMethodStatus::Implemented);
    }
    for method in [
        "source.materialize",
        "run.heartbeat",
        "model.complete",
        "secret.resolve",
        "tool.execute",
    ] {
        assert_method_status(&schema.callbacks, method, RunnerMethodStatus::Implemented);
    }
    for method in [
        "event.review",
        "event.runtime",
        "run.finished",
        "run.failed",
    ] {
        assert_method_status(
            &schema.notifications,
            method,
            RunnerMethodStatus::Implemented,
        );
    }
}

#[test]
fn schema_definitions_cover_referenced_payload_types() {
    let schema = protocol_schema();
    let definitions = schema
        .definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut missing = Vec::new();

    for method in schema
        .requests
        .iter()
        .chain(schema.callbacks.iter())
        .chain(schema.notifications.iter())
    {
        for payload in method.params.iter().chain(method.result.iter()) {
            if !definitions.contains(payload.name.as_str()) {
                missing.push(format!("{} -> {}", method.method, payload.name));
            }
        }
    }
    for definition in &schema.definitions {
        for field in &definition.fields {
            let Some(payload_type) = referenced_payload_type(&field.value_type) else {
                continue;
            };
            if !definitions.contains(payload_type) {
                missing.push(format!(
                    "{}.{} -> {}",
                    definition.name, field.name, payload_type
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "missing schema definitions: {missing:?}"
    );
}

#[test]
fn schema_definitions_are_reachable_from_methods() {
    let schema = protocol_schema();
    let definitions = schema
        .definitions
        .iter()
        .map(|definition| (definition.name.as_str(), definition))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut reachable = std::collections::BTreeSet::new();
    let mut stack = Vec::new();

    for method in schema
        .requests
        .iter()
        .chain(schema.callbacks.iter())
        .chain(schema.notifications.iter())
    {
        stack.extend(
            method
                .params
                .iter()
                .chain(method.result.iter())
                .map(|payload| payload.name.as_str()),
        );
    }
    while let Some(name) = stack.pop() {
        if !reachable.insert(name) {
            continue;
        }
        let Some(definition) = definitions.get(name) else {
            continue;
        };
        for field in &definition.fields {
            if let Some(payload_type) = referenced_payload_type(&field.value_type) {
                stack.push(payload_type);
            }
        }
    }

    let unreachable = definitions
        .keys()
        .filter(|name| !reachable.contains(**name))
        .copied()
        .collect::<Vec<_>>();
    assert!(
        unreachable.is_empty(),
        "unreachable schema definitions: {unreachable:?}"
    );
}

fn referenced_payload_type(value_type: &str) -> Option<&str> {
    let value_type = value_type.strip_suffix("[]").unwrap_or(value_type);
    if value_type.contains('<') || value_type.contains('|') {
        return None;
    }
    if value_type
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
    {
        Some(value_type)
    } else {
        None
    }
}

#[test]
fn stdio_handles_multiple_requests() {
    let input = br#"{"jsonrpc":"2.0","id":1,"method":"runner.check"}
{"jsonrpc":"2.0","id":2,"method":"runner.schema.export"}
"#;
    let mut reader = std::io::Cursor::new(input);
    let mut writer = Vec::new();

    run_stdio(&mut reader, &mut writer).expect("stdio run");

    let output = String::from_utf8(writer).expect("utf8 output");
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"ok\":true"));
    assert!(lines[1].contains("runner.schema.export"));
}

#[test]
fn stdio_starts_run_emits_events_and_stores_result() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .expect("fixture file");
    let start = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run.start",
        "params": {
            "protocolVersion": RUNNER_PROTOCOL_VERSION,
            "runId": "fixture-run",
            "repo": repo.path(),
            "changedFiles": ["Cargo.toml"],
            "model": {},
            "sessions": [
                {
                    "id": "security",
                    "role": "security",
                    "objective": "Check fixture repo"
                }
            ]
        }
    });
    let status = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "run.status",
        "params": {"runId": "fixture-run"}
    });
    let result = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "run.result",
        "params": {"runId": "fixture-run"}
    });
    let input = format!("{start}\n{status}\n{result}\n");
    let mut reader = std::io::Cursor::new(input.into_bytes());
    let mut writer = Vec::new();

    run_stdio(&mut reader, &mut writer).expect("stdio run");

    let output = String::from_utf8(writer).expect("utf8 output");
    let values = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("event.review"))));
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("run.finished"))));
    let start_response = values
        .iter()
        .find(|value| value.get("id") == Some(&json!(1)))
        .expect("start response");
    assert_eq!(start_response["result"]["runId"], "fixture-run");
    assert_eq!(start_response["result"]["status"], "completed");
    assert_eq!(start_response["result"]["summary"]["completedSessions"], 1);
    let status_response = values
        .iter()
        .find(|value| value.get("id") == Some(&json!(2)))
        .expect("status response");
    assert_eq!(status_response["result"]["status"], "completed");
    let result_response = values
        .iter()
        .find(|value| value.get("id") == Some(&json!(3)))
        .expect("result response");
    assert_eq!(result_response["result"]["runId"], "fixture-run");
}

#[test]
fn stdio_failed_run_emits_structured_failure_notification() {
    let mut session = RunnerStdioSession::default();
    let mut writer = Vec::new();
    let start = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run.start",
        "params": {
            "protocolVersion": RUNNER_PROTOCOL_VERSION,
            "runId": "failed-run",
            "source": {
                "type": "custom",
                "provider": "acme",
                "id": "review-123"
            }
        }
    });

    let values = send_jsonrpc(&mut session, &mut writer, start);

    let failed = values
        .iter()
        .find(|value| value.get("method") == Some(&json!("run.failed")))
        .expect("run.failed notification");
    assert_eq!(failed["params"]["kind"], "runner_error");
    assert_eq!(failed["params"]["failureKind"], "source_unavailable");
    assert_eq!(failed["params"]["retryHint"], "requires_user_action");
    let response = values
        .iter()
        .find(|value| value.get("id") == Some(&json!(1)))
        .expect("run.start response");
    assert_eq!(response["error"]["data"]["kind"], "runner_error");
}

#[test]
fn run_failed_notification_classifies_common_terminal_failures() {
    let auth = RunFailedNotification::from_runner_error("provider auth failed");
    assert_eq!(auth.failure_kind, RunnerFailureKind::AuthFailed);
    assert_eq!(auth.retry_hint, RunnerRetryHint::RequiresUserAction);

    let model = RunFailedNotification::from_runner_error(
        "SDK callback model.complete failed: upstream timeout",
    );
    assert_eq!(model.failure_kind, RunnerFailureKind::ModelFailed);
    assert_eq!(model.retry_hint, RunnerRetryHint::Retryable);

    let tool = RunFailedNotification::from_runner_error(
        "SDK callback tool.execute failed: service unavailable",
    );
    assert_eq!(tool.failure_kind, RunnerFailureKind::ToolFailed);
    assert_eq!(tool.retry_hint, RunnerRetryHint::Retryable);

    let budget = RunFailedNotification::from_runner_error("budget exhausted");
    assert_eq!(budget.failure_kind, RunnerFailureKind::BudgetExhausted);
    assert_eq!(budget.retry_hint, RunnerRetryHint::NotRetryable);
}

#[test]
fn stdio_reads_artifacts_snapshots_and_cancel_status() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .expect("fixture file");
    let mut session = RunnerStdioSession::default();
    let mut writer = Vec::new();
    let start = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run.start",
        "params": {
            "protocolVersion": RUNNER_PROTOCOL_VERSION,
            "runId": "resource-run",
            "repo": repo.path(),
            "changedFiles": ["Cargo.toml"],
            "model": {},
            "sessions": [
                {
                    "id": "security",
                    "role": "security",
                    "objective": "Check fixture repo"
                }
            ]
        }
    });

    session
        .handle_line(&start.to_string(), &mut writer)
        .expect("start run");
    let start_values = parse_json_lines(&writer);
    let artifact_id = start_values
        .iter()
        .find_map(|value| value.get("params")?.get("artifactId")?.as_str())
        .expect("artifact id")
        .to_string();
    let snapshot_id = start_values
        .iter()
        .find(|value| value.get("id") == Some(&json!(1)))
        .and_then(|value| value["result"]["snapshots"][0]["snapshotId"].as_str())
        .expect("snapshot id")
        .to_string();

    let read = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "artifact.read",
            "params": {"runId": "resource-run", "artifactId": artifact_id}
        }),
    );
    assert_eq!(read[0]["result"]["view"], "redacted");
    assert!(read[0]["result"]["artifact"]["content"]
        .as_str()
        .expect("artifact content")
        .contains("Cargo.toml"));

    let export = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "artifact.export",
            "params": {
                "runId": "resource-run",
                "artifactIds": [artifact_id],
                "maxArtifacts": 1,
                "maxBytes": 1000
            }
        }),
    );
    assert_eq!(export[0]["result"]["artifactCount"], 1);

    let snapshot = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "snapshot.readText",
            "params": {
                "runId": "resource-run",
                "snapshotId": snapshot_id,
                "path": "Cargo.toml",
                "maxBytes": 1000
            }
        }),
    );
    assert_eq!(snapshot[0]["result"]["path"], "Cargo.toml");
    assert!(snapshot[0]["result"]["content"]
        .as_str()
        .expect("snapshot content")
        .contains("fixture"));

    let cancel = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "run.cancel",
            "params": {"runId": "resource-run"}
        }),
    );
    assert_eq!(cancel[0]["result"]["status"], "completed");
    assert_eq!(cancel[0]["result"]["cancelled"], false);
}

#[test]
fn stdio_context_index_pack_and_query() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::create_dir_all(repo.path().join("src/auth")).expect("src dir");
    std::fs::write(
        repo.path().join("src/auth/token.rs"),
        "pub fn authorize_request() {}\n",
    )
    .expect("source file");
    std::fs::create_dir_all(repo.path().join("tests/auth")).expect("tests dir");
    std::fs::write(
        repo.path().join("tests/auth/token_test.rs"),
        "#[test]\nfn authorize_request_test() {}\n",
    )
    .expect("test file");
    let mut session = RunnerStdioSession::default();
    let mut writer = Vec::new();

    let index = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "context.index",
            "params": {
                "repo": repo.path(),
                "changedFiles": ["src/auth/token.rs"]
            }
        }),
    );
    let snapshot_id = index[0]["result"]["snapshotId"]
        .as_str()
        .expect("snapshot id")
        .to_string();
    let pack = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "context.pack",
            "params": {
                "snapshotId": snapshot_id,
                "purpose": "tests",
                "maxTokens": 12000
            }
        }),
    );
    let query = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "context.query",
            "params": {
                "snapshotId": snapshot_id,
                "kind": "related_tests",
                "arguments": { "path": "src/auth/token.rs" },
                "currentEvidence": [],
                "limits": { "maxResults": 10, "maxTokens": 1000 }
            }
        }),
    );
    let feedback = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "context.feedback",
            "params": {
                "snapshotId": snapshot_id,
                "evidenceIds": [],
                "feedback": "Suppress duplicate generated auth wrapper warning.",
                "source": "human_feedback",
                "scope": "repository"
            }
        }),
    );
    let learning_id = feedback[0]["result"]["proposedLearning"]["id"]
        .as_str()
        .expect("learning id")
        .to_string();
    let approval = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "context.learning.approve",
            "params": {
                "snapshotId": snapshot_id,
                "learningId": learning_id,
                "approve": true
            }
        }),
    );

    assert_eq!(
        index[0]["result"]["schemaVersion"],
        json!("muzen.context_manifest.v1")
    );
    assert_eq!(pack[0]["result"]["purpose"], json!("tests"));
    assert_eq!(query[0]["result"]["kind"], json!("related_tests"));
    assert!(query[0]["result"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|evidence| evidence["path"] == json!("tests/auth/token_test.rs")));
    assert_eq!(
        feedback[0]["result"]["proposedLearning"]["status"],
        json!("proposed")
    );
    assert_eq!(
        approval[0]["result"]["learning"]["status"],
        json!("approved")
    );
}

#[test]
fn stdio_handles_github_webhook_through_rust_core() {
    let mut session = RunnerStdioSession::default();
    let mut writer = Vec::new();
    let body = json!({
        "action": "opened",
        "repository": {
            "full_name": "maskdotdev/heimdaal"
        },
        "pull_request": {
            "number": 123
        }
    })
    .to_string();
    let signature =
        crate::review_sessions::github_webhook_signature("secret", body.as_bytes()).unwrap();

    let frames = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "webhook.github.handle",
            "params": {
                "projectId": "acme",
                "headers": {
                    "X-GitHub-Event": "pull_request",
                    "X-GitHub-Delivery": "delivery-1",
                    "X-Hub-Signature-256": signature
                },
                "body": body,
                "secret": "secret",
                "options": {
                    "reviewOptions": {
                        "dedupe": "source"
                    }
                }
            }
        }),
    );

    let result = frames[0]["result"].as_object().expect("result");
    let body: serde_json::Value =
        serde_json::from_str(result["body"].as_str().expect("body")).unwrap();

    assert_eq!(result["statusCode"], json!(202));
    assert_eq!(result["headers"]["Content-Type"], json!("application/json"));
    assert_eq!(body["type"], json!("review_created"));
    assert_eq!(body["deliveryId"], json!("delivery-1"));
    assert_eq!(body["status"], json!("queued"));

    let worker_frames = send_jsonrpc(
        &mut session,
        &mut writer,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "worker.runOnce",
            "params": {
                "workerId": "worker-a",
                "maxSessions": 1,
                "hostConfig": {
                    "scheduling": {
                        "defaultRetryPolicy": {
                            "maxAttempts": 1,
                            "initialBackoffSeconds": 1,
                            "maxBackoffSeconds": 1
                        }
                    }
                }
            }
        }),
    );
    let worker_result = worker_frames[0]["result"].as_object().expect("result");

    assert_eq!(worker_result["workerId"], json!("worker-a"));
    assert_eq!(worker_result["claimed"], json!(1));
    assert_eq!(worker_result["failed"], json!(1));
}

#[test]
fn interactive_stdio_runs_model_and_tool_callbacks() {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
    )
    .expect("fixture file");
    let start = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run.start",
        "params": {
            "protocolVersion": RUNNER_PROTOCOL_VERSION,
            "runId": "interactive-run",
            "repo": repo.path(),
            "changedFiles": ["Cargo.toml"],
            "model": {"callback": true},
            "tools": [
                {
                    "id": "host_context",
                    "description": "Return host-supplied context.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "topic": {"type": "string"}
                        },
                        "required": ["topic"],
                        "additionalProperties": false
                    }
                }
            ],
            "sessions": [
                {
                    "id": "callback-session",
                    "role": "correctness",
                    "objective": "Exercise SDK callbacks"
                }
            ],
            "limits": {"maxActiveSessions": 1}
        }
    });
    let first_model = json!({
        "jsonrpc": "2.0",
        "id": "runner-callback-1",
        "result": {
            "toolCalls": [
                {"toolId": "read_diff", "arguments": {}},
                {"toolId": "read_file", "arguments": {"path": "Cargo.toml"}},
                {"toolId": "host_context", "arguments": {"topic": "sdk"}},
                {"toolId": "search_text", "arguments": {"query": "fixture"}}
            ],
            "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}
        }
    });
    let tool_result = json!({
        "jsonrpc": "2.0",
        "id": "runner-callback-2",
        "result": {
            "data": {"topic": "sdk", "message": "host context received"},
            "artifact": {"key": "host-context", "content": "context artifact"}
        }
    });
    let second_model = json!({
        "jsonrpc": "2.0",
        "id": "runner-callback-3",
        "result": {
            "content": "{\"verdict\":\"clean\",\"summary\":\"callback test complete\",\"candidates\":[],\"notes\":[],\"completeness\":{\"reviewedChangedFiles\":[\"Cargo.toml\"],\"reviewedRiskEntries\":[],\"unreviewedRiskEntries\":[],\"unresolvedQuestions\":[],\"incompleteReasons\":[],\"ignoredChildCandidates\":[]}}",
            "usage": {"inputTokens": 20, "outputTokens": 5, "totalTokens": 25}
        }
    });
    let input = format!("{start}\n{first_model}\n{tool_result}\n{second_model}\n");
    let reader = std::io::Cursor::new(input.into_bytes());
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedWriter(output.clone());

    run_stdio_interactive(reader, writer).expect("interactive stdio");

    let values = wait_for_json_lines(&output, |values| {
        values
            .iter()
            .any(|value| value.get("method") == Some(&json!("run.finished")))
    });
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("model.complete"))));
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("tool.execute"))));
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("event.runtime"))));
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("event.review"))));
    assert!(values
        .iter()
        .any(|value| value.get("method") == Some(&json!("run.finished"))));
    let start_response = values
        .iter()
        .find(|value| value.get("id") == Some(&json!(1)))
        .expect("start response");
    assert_eq!(start_response["result"]["status"], "completed");
    assert_eq!(start_response["result"]["summary"]["completedSessions"], 1);
}

fn send_jsonrpc(
    session: &mut RunnerStdioSession,
    writer: &mut Vec<u8>,
    request: serde_json::Value,
) -> Vec<serde_json::Value> {
    let start = writer.len();
    session
        .handle_line(&request.to_string(), writer)
        .expect("handle request");
    parse_json_lines(&writer[start..])
}

fn parse_json_lines(bytes: &[u8]) -> Vec<serde_json::Value> {
    let output = std::str::from_utf8(bytes).expect("utf8 output");
    output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect()
}

fn wait_for_json_lines(
    output: &Arc<Mutex<Vec<u8>>>,
    ready: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let bytes = output.lock().expect("output lock").clone();
        if let Ok(values) = try_parse_json_lines(&bytes) {
            if ready(&values) {
                return values;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for interactive stdio output: {}",
            String::from_utf8_lossy(&bytes)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn try_parse_json_lines(bytes: &[u8]) -> Result<Vec<serde_json::Value>, serde_json::Error> {
    let output = std::str::from_utf8(bytes).expect("utf8 output");
    output
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect()
}

fn assert_method_status(
    methods: &[RunnerMethodSchema],
    method: &str,
    expected: RunnerMethodStatus,
) {
    let actual = methods
        .iter()
        .find(|candidate| candidate.method == method)
        .unwrap_or_else(|| panic!("missing runner method {method}"));
    assert_eq!(actual.status, expected, "{method}");
}
