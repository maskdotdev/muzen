use crate::reviewer_kernel::agent_loop::budgeted_tool_result_count;

use super::*;

#[test]
fn delegate_tools_have_stable_names() {
    assert_eq!(DelegateTaskKind::SearchCode.tool_name(), "search_code");
    assert_eq!(DelegateTaskKind::ExploreCode.slug(), "explore");
    assert_eq!(DelegateTaskKind::ValidateFinding.slug(), "validate");
}

#[test]
fn autonomous_default_budget_scales_tool_budget_with_changed_file_count() {
    let baseline = AgentBudget::planned_baseline();
    let budget = autonomous_orchestrator_budget(AgentBudget::planned_baseline(), 12);
    assert_eq!(budget.budget_source, BudgetSource::AdaptiveReview);
    assert_eq!(budget.max_turns, baseline.max_turns);
    assert!(budget.max_tool_calls >= 48);
    assert!(budget.max_prompt_tokens >= 96_000);
}

#[test]
fn autonomous_budget_preserves_caller_hard_caps() {
    let hard_cap = AgentBudget::caller_hard_cap(4, 8, 12_000, 2_000);
    assert_eq!(
        autonomous_orchestrator_budget(hard_cap.clone(), 20),
        hard_cap
    );
}

#[test]
fn orchestrator_finalization_waits_for_tool_budget_before_turn_guard() {
    let budget = AgentBudget::caller_hard_cap(10, 32, 64_000, 8_000);
    let turn_guard = session_turn_guard(SessionKind::Orchestrator, &budget);
    assert!(turn_guard > budget.max_turns);
    assert!(!should_force_final_turn(
        SessionKind::Orchestrator,
        budget.max_turns - 1,
        turn_guard,
        0,
        &budget,
    ));
    assert!(should_force_final_turn(
        SessionKind::Orchestrator,
        turn_guard - 1,
        turn_guard,
        0,
        &budget,
    ));
    assert!(should_force_final_turn(
        SessionKind::Orchestrator,
        0,
        turn_guard,
        budget.max_tool_calls,
        &budget,
    ));
}

#[test]
fn child_finalization_still_reserves_schema_turns() {
    let budget = AgentBudget::caller_hard_cap(10, 32, 64_000, 8_000);
    let turn_guard = session_turn_guard(SessionKind::Child(DelegateTaskKind::SearchCode), &budget);
    assert_eq!(turn_guard, budget.max_turns);
    assert!(!should_force_final_turn(
        SessionKind::Child(DelegateTaskKind::SearchCode),
        7,
        turn_guard,
        0,
        &budget,
    ));
    assert!(should_force_final_turn(
        SessionKind::Child(DelegateTaskKind::SearchCode),
        8,
        turn_guard,
        0,
        &budget,
    ));
}

#[test]
fn custom_delegate_results_consume_tool_budget() {
    let custom_success = tool_result_for_budget_test("search_code", None);
    let builtin_invalid =
        tool_result_for_budget_test("read_file", Some(ToolErrorCode::InvalidArgs));
    let budget_denied =
        tool_result_for_budget_test("explore_code", Some(ToolErrorCode::BudgetExceeded));
    assert_eq!(
        budgeted_tool_result_count(&[custom_success, builtin_invalid, budget_denied]),
        2
    );
}

#[test]
fn diff_risk_inventory_surfaces_generic_async_and_lazy_obligations() {
    let diff = r#"diff --git a/src/workflow.ts b/src/workflow.ts
--- a/src/workflow.ts
+++ b/src/workflow.ts
@@ -10,6 +10,7 @@ export async function run(items) {
-  items.forEach((item) => {
+  items.forEach(async (item) => {
-    const adapter = registry[item.kind];
+    const adapter = await importAdapter(item.kind);
+    pending.push(adapter.delete(item.id));
   });
 }
diff --git a/src/registry.ts b/src/registry.ts
--- a/src/registry.ts
+++ b/src/registry.ts
@@ -1,3 +1,3 @@
-  service: serviceModule,
+  service: import("./service"),
"#;
    let entries = diff_risk_inventory(diff, 10);
    let categories = entries
        .iter()
        .map(|entry| entry.category)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(categories.contains("async_callback"));
    assert!(categories.contains("async_boundary"));
    assert!(categories.contains("side_effect_aggregation"));
    assert!(categories.contains("lazy_module_loading"));
    assert!(entries.iter().all(|entry| !entry.path.is_empty()));
}

#[test]
fn autonomous_review_schemas_are_strict_provider_compatible() {
    assert_strict_object_schema(&orchestrator_response_format().schema);
    assert_strict_object_schema(&child_response_format(DelegateTaskKind::SearchCode).schema);
    assert_strict_object_schema(&child_response_format(DelegateTaskKind::ExploreCode).schema);
    assert_strict_object_schema(&child_response_format(DelegateTaskKind::ValidateFinding).schema);
    assert_strict_object_schema(&DelegateTaskKind::SearchCode.parameters_schema());
    assert_strict_object_schema(&DelegateTaskKind::ExploreCode.parameters_schema());
    assert_strict_object_schema(&DelegateTaskKind::ValidateFinding.parameters_schema());
}

#[test]
fn orchestrator_output_defaults_to_incomplete_when_malformed() {
    let parsed = parse_orchestrator_output(Some("not json"));
    assert_eq!(parsed.verdict, "incomplete");
    assert!(parsed.candidates.is_empty());
}

#[test]
fn orchestrator_output_parses_camel_case_candidate_contract() {
    let parsed = parse_orchestrator_output(Some(
        r#"{
            "verdict": "issues_found",
            "summary": "done",
            "candidates": [{
                "id": "finding_1",
                "title": "Async callback returns early",
                "claim": "The changed loop returns success before writes finish.",
                "severity": "high",
                "path": "src/workflow.ts",
                "startLine": 42,
                "endLine": 43,
                "behaviorBefore": "The caller waited for each write.",
                "behaviorAfter": "The caller returns before write promises settle.",
                "evidenceArtifactIds": ["artifact_1"],
                "relatedPaths": ["src/caller.ts"]
            }],
            "notes": [],
            "completeness": {}
        }"#,
    ));

    assert_eq!(parsed.candidates.len(), 1);
    let candidate = &parsed.candidates[0];
    assert_eq!(candidate.start_line, Some(42));
    assert_eq!(candidate.end_line, Some(43));
    assert_eq!(
        candidate.behavior_after.as_deref(),
        Some("The caller returns before write promises settle.")
    );
    assert_eq!(candidate.evidence_artifact_ids, ["artifact_1"]);
    assert_eq!(candidate.related_paths, ["src/caller.ts"]);
}

#[test]
fn publication_gate_rejects_supported_no_bug_candidate() {
    let candidate = publication_candidate(
        "Async callback remains correct",
        "The new async test call is correct and no issue is introduced.",
    );

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        Some("non_finding_text")
    );
}

#[test]
fn publication_gate_rejects_explicitly_bundled_candidate() {
    let candidate = publication_candidate(
        "Handler combines unrelated failures",
        "The changed handler drops failed retries and also skips cleanup after errors.",
    );

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        Some("bundled_claim")
    );
}

#[test]
fn publication_gate_accepts_single_concrete_negative_outcome() {
    let candidate = publication_candidate(
            "Async callback returns before writes finish",
            "The changed forEach async callback returns success before delete promises finish, so failed deletes can be silently skipped.",
        );

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        None
    );
}

#[test]
fn final_output_schema_gate_requires_required_fields() {
    assert!(session_output_valid(
        SessionKind::Orchestrator,
        Some(
            r#"{"verdict":"clean","summary":"done","candidates":[],"notes":[],"completeness":{}}"#
        )
    ));
    assert!(!session_output_valid(
        SessionKind::Orchestrator,
        Some(r#"{"verdict":"clean","candidates":[],"notes":[],"completeness":{}}"#)
    ));
    assert!(session_output_valid(
        SessionKind::Child(DelegateTaskKind::SearchCode),
        Some(
            r#"{"status":"insufficient","summary":"none","checkedPaths":[],"evidence":[],"openQuestions":[],"candidateFindings":[]}"#
        )
    ));
    assert!(!session_output_valid(
        SessionKind::Child(DelegateTaskKind::SearchCode),
        Some(r#"{"status":"insufficient","summary":"none"}"#)
    ));
}

fn publication_candidate(title: &str, claim: &str) -> CandidateFinding {
    CandidateFinding {
            id: stable_id(&[title, claim]),
            title: title.to_string(),
            claim: claim.to_string(),
            severity: Some("medium".to_string()),
            path: "src/workflow.ts".to_string(),
            start_line: Some(42),
            end_line: Some(42),
            behavior_before: Some("The workflow awaited each delete before returning success.".to_string()),
            behavior_after: Some(
                "The workflow starts delete promises inside an unawaited callback and returns success before those deletes finish."
                    .to_string(),
            ),
            evidence_artifact_ids: Vec::new(),
            related_paths: Vec::new(),
        }
}

fn publication_changed_paths() -> std::collections::BTreeSet<String> {
    ["src/workflow.ts"]
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

fn publication_changed_ranges() -> BTreeMap<String, Vec<(usize, usize)>> {
    BTreeMap::from([("src/workflow.ts".to_string(), vec![(40, 45)])])
}

fn tool_result_for_budget_test(
    tool_name: &str,
    error_code: Option<ToolErrorCode>,
) -> ToolResultEnvelope {
    ToolResultEnvelope {
        ok: error_code.is_none(),
        tool_call_id: ToolCallId(format!("{tool_name}-call")),
        tool_name: ToolId::parse(tool_name).expect("valid tool id"),
        provider_id: ToolProviderId::in_process(),
        snapshot_id: SnapshotId("snapshot".to_string()),
        artifact_id: None,
        cache: CacheInfo {
            status: CacheStatus::NotCacheable,
            key_hash: None,
        },
        limits: LimitInfo::default(),
        data: None,
        error: error_code.map(|code| ToolErrorInfo {
            code,
            message: "test error".to_string(),
            retryable: false,
            partial: false,
        }),
    }
}

fn assert_strict_object_schema(schema: &Value) {
    if schema.get("type").and_then(Value::as_str) == Some("object") {
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "object schema must set additionalProperties=false: {schema}"
        );
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("object schema must declare properties");
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .expect("object schema must declare required");
        let property_names = properties
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let required_names = required
            .iter()
            .map(|value| value.as_str().expect("required entries must be strings"))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            required_names, property_names,
            "strict object schema required list must match properties"
        );
    }
    match schema {
        Value::Array(items) => {
            for item in items {
                assert_strict_object_schema(item);
            }
        }
        Value::Object(object) => {
            for value in object.values() {
                assert_strict_object_schema(value);
            }
        }
        _ => {}
    }
}
