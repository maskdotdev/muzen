use std::sync::Mutex;

use super::*;

#[test]
fn dedupes_findings_with_identical_normalized_text() {
    let findings = dedupe_runner_findings(vec![
        test_finding(
            "finding-a",
            "Cleanup deletes unrelated reminders",
            "The changed deleteMany predicate removes reminders outside the SMS scope.",
            "src/a.ts",
        ),
        test_finding(
            "finding-b",
            "Cleanup deletes unrelated reminders",
            "The changed deleteMany predicate removes reminders outside the SMS scope.",
            "src/b.ts",
        ),
    ]);

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "finding-a");
    assert!(findings[0].claim.contains("Also observed in: src/b.ts"));
    assert_eq!(
        findings[0].discovered_by,
        vec!["session-finding-a", "session-finding-b"]
    );
}

#[test]
fn dedupes_rephrased_findings_on_the_same_anchor() {
    let findings = dedupe_runner_findings(vec![
            test_finding(
                "finding-a",
                "Zero-length override detection uses Dayjs object identity instead of value equality",
                "The changed override check compares dayjs(date.start) === dayjs(date.end), which compares object identity, so zero-length overrides are never recognized.",
                "src/slots.ts",
            ),
            test_finding(
                "finding-b",
                "Zero-length date overrides are never recognized because the code compares Dayjs objects by identity",
                "dayjs(date.start) === dayjs(date.end) compares two distinct Dayjs object instances, so the zero-length override branch never runs.",
                "src/slots.ts",
            ),
        ]);

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].id, "finding-a");
    assert_eq!(
        findings[0].discovered_by,
        vec!["session-finding-a", "session-finding-b"]
    );
}

#[test]
fn keeps_distinct_findings_on_different_anchors_in_one_file() {
    let mut working_hours = test_finding(
            "finding-a",
            "Working-hours end check reuses the slot start minute",
            "The end boundary recomputes slotStartTime minutes instead of using slotEndTime, so slots can pass the end check.",
            "src/slots.ts",
        );
    working_hours.location.as_mut().unwrap().start_line = Some(137);
    working_hours.location.as_mut().unwrap().end_line = Some(145);
    let mut dayjs_identity = test_finding(
            "finding-b",
            "Zero-length override detection compares Dayjs objects by identity",
            "dayjs(date.start) === dayjs(date.end) compares object identity so the zero-length branch never runs.",
            "src/slots.ts",
        );
    dayjs_identity.location.as_mut().unwrap().start_line = Some(109);
    dayjs_identity.location.as_mut().unwrap().end_line = Some(114);

    let findings = dedupe_runner_findings(vec![working_hours, dayjs_identity]);

    assert_eq!(findings.len(), 2);
}

fn test_finding(id: &str, title: &str, claim: &str, path: &str) -> RunnerFinding {
    RunnerFinding {
        id: id.to_string(),
        title: title.to_string(),
        claim: claim.to_string(),
        evidence_count: 0,
        publishable: true,
        severity: Some("warning".to_string()),
        confidence: Some(0.72),
        validation_status: Some("validated".to_string()),
        challenge_status: Some("not_run".to_string()),
        evidence: Vec::new(),
        discovered_by: vec![format!("session-{id}")],
        validated_by: Vec::new(),
        challenged_by: Vec::new(),
        location: Some(RunnerFindingLocation {
            path: path.to_string(),
            revision: None,
            start_line: Some(1),
            end_line: Some(2),
            start_column: None,
            end_column: None,
            side: None,
            provider_anchor: None,
        }),
        related_paths: Vec::new(),
    }
}

#[test]
fn heartbeat_callback_can_cancel_active_run() {
    struct HeartbeatTransport {
        requests: Mutex<Vec<(String, Value)>>,
    }

    impl RunnerCallbackTransport for HeartbeatTransport {
        fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests
                .lock()
                .expect("heartbeat requests poisoned")
                .push((method.to_string(), params));
            Ok(json!({ "continueRun": false }))
        }

        fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn respond(&self, _response: &crate::runner::JsonRpcResponse) -> Result<()> {
            Ok(())
        }
    }

    let transport = Arc::new(HeartbeatTransport {
        requests: Mutex::new(Vec::new()),
    });
    let cancel = CancellationToken::new();
    let config = RunHeartbeatConfigParams {
        callback: true,
        interval_ms: Some(1),
        lease_seconds: Some(30),
    };

    let guard = start_heartbeat(
        "review-heartbeat",
        Some(&config),
        Some(transport.clone()),
        cancel.clone(),
    )
    .expect("heartbeat starts");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cancel.is_cancelled() {
        assert!(Instant::now() < deadline, "heartbeat did not cancel run");
        thread::sleep(Duration::from_millis(5));
    }
    guard.stop();

    let requests = transport
        .requests
        .lock()
        .expect("heartbeat requests poisoned");
    assert_eq!(requests[0].0, "run.heartbeat");
    assert_eq!(requests[0].1["runId"], "review-heartbeat");
    assert_eq!(requests[0].1["leaseSeconds"], 30);
}
