use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::reviewer_kernel::agent_loop::budgeted_tool_result_count;
use crate::reviewer_kernel::review_contract::{
    ChangeKind, ChangeScopeV1, ChangedFileEntryV1, ChangedFileStatus, PathPolicyV1,
    RenameDetection, SnapshotMode, ToolCounts,
};

use super::schemas::{child_final_instruction, orchestrator_final_instruction};
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
fn autonomous_default_child_session_cap_bounds_recall_fanout() {
    assert_eq!(DEFAULT_MAX_CHILD_SESSIONS, 16);
    assert!(
        DEFAULT_MAX_CHILD_SESSIONS
            >= MAX_LEAD_GENERATION_ENTRIES + MAX_MANDATORY_VALIDATIONS_PER_REVIEW
    );
}

#[test]
fn lead_generation_uses_narrower_child_budget_than_deep_explore() {
    let lead_budget = delegates::lead_generation_child_budget();
    let explore_budget = delegates::child_budget(DelegateTaskKind::ExploreCode);

    assert!(lead_budget.max_turns < explore_budget.max_turns);
    assert!(lead_budget.max_tool_calls < explore_budget.max_tool_calls);
    assert!(lead_budget.max_prompt_tokens < explore_budget.max_prompt_tokens);
    assert!(lead_budget.max_output_tokens < explore_budget.max_output_tokens);
}

#[test]
fn lead_generation_expands_budget_for_persisted_identity_only() {
    let base_budget = delegates::lead_generation_child_budget();
    let identity_budget = lead_generation_budget_for_category("persisted_identity_propagation");
    let optional_budget = lead_generation_budget_for_category("unchecked_optional_access");
    let explore_budget = delegates::child_budget(DelegateTaskKind::ExploreCode);

    assert!(identity_budget.max_turns > base_budget.max_turns);
    assert!(identity_budget.max_tool_calls > base_budget.max_tool_calls);
    assert!(identity_budget.max_turns < explore_budget.max_turns);
    assert!(identity_budget.max_tool_calls < explore_budget.max_tool_calls);
    assert_eq!(optional_budget.max_turns, base_budget.max_turns);
    assert_eq!(optional_budget.max_tool_calls, base_budget.max_tool_calls);
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
fn custom_tool_results_are_visible_in_tool_counts() {
    let mut counts = ToolCounts::default();
    let custom_success = tool_result_for_budget_test("search_code", None);
    let builtin_success = tool_result_for_budget_test("read_file", None);
    let custom_denied =
        tool_result_for_budget_test("explore_code", Some(ToolErrorCode::BudgetExceeded));

    crate::reviewer_kernel::tool_engine::count_tool_result(&mut counts, &custom_success);
    crate::reviewer_kernel::tool_engine::count_tool_result(&mut counts, &builtin_success);
    crate::reviewer_kernel::tool_engine::count_tool_result(&mut counts, &custom_denied);

    assert_eq!(counts.custom, 1);
    assert_eq!(counts.read_file, 1);
    assert_eq!(counts.total(), 2);
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
fn diff_risk_inventory_surfaces_general_code_review_obligations() {
    let diff = r#"diff --git a/src/Command.java b/src/Command.java
--- a/src/Command.java
+++ b/src/Command.java
@@ -1,3 +1,10 @@
+  String part = value.substring(4, 6);
+  this.rawId = Objects.requireNonNull(grantType);
+  List values = Json.readValue(payload, List.class);
+  Optional<CredentialModel> credential = RecoveryAuthnCodesUtils.getCredential(user);
+  return credential.get().getId();
+  StoredCredentialModel saved = new StoredCredentialModel(type, secret);
+  store.removeById(saved.getId());
+  commandLine.exit(42);
+  if (profile.isFeatureEnabled(NEW_FLOW)) cleanup();
+  store.findByName(server, resource.getId(), owner.getId());
+  try { run(); } catch (RuntimeException ignored) {}
+  /**
+   * Shortcut is usually like 3-letters.
+   */
+  private String santizeAnchors(String value) { return value; }
diff --git a/i18n/messages_fr.properties b/i18n/messages_fr.properties
--- a/i18n/messages_fr.properties
+++ b/i18n/messages_fr.properties
@@ -1,2 +1,3 @@
+welcome=Welcome {0}
"#;
    let entries = diff_risk_inventory(diff, 20);
    let categories = entries
        .iter()
        .map(|entry| entry.category)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(categories.contains("offset_or_slice_boundary"));
    assert!(categories.contains("nullability_contract"));
    assert!(categories.contains("unchecked_collection_shape"));
    assert!(categories.contains("unchecked_optional_access"));
    assert!(categories.contains("persisted_identity_propagation"));
    assert!(categories.contains("process_exit_boundary"));
    assert!(categories.contains("feature_gate_consistency"));
    assert!(categories.contains("identifier_lookup_contract"));
    assert!(categories.contains("broad_exception_boundary"));
    assert!(categories.contains("documentation_contract_consistency"));
    assert!(categories.contains("suspicious_identifier_spelling"));
    assert!(categories.contains("localized_resource_change"));
}

#[test]
fn diff_risk_inventory_surfaces_stored_model_reconstruction_identity_obligation() {
    let diff = r#"diff --git a/src/Credentials.java b/src/Credentials.java
--- a/src/Credentials.java
+++ b/src/Credentials.java
@@ -10,6 +10,8 @@ class Credentials {
+  Optional<StoredCredentialModel> storedCredential = credentialStore.find(user);
+  RecoveryCredentialModel model = RecoveryCredentialModel.createFromCredentialModel(storedCredential.get());
+  auditModel.setId(KeycloakModelUtils.generateId());
 }
"#;
    let entries = diff_risk_inventory(diff, 20);
    assert!(
        entries
            .iter()
            .any(|entry| entry.category == "persisted_identity_propagation"
                && entry.code.contains("createFromCredentialModel")),
        "reconstructing a domain model from a stored credential should require id/identity propagation review: {entries:#?}"
    );
    let first_identity = entries
        .iter()
        .find(|entry| entry.category == "persisted_identity_propagation")
        .expect("expected persisted identity entry");
    assert!(
        first_identity.code.contains("createFromCredentialModel"),
        "stored-model reconstruction is a stronger identity lead than unrelated id generation: {entries:#?}"
    );
}

#[test]
fn diff_risk_inventory_does_not_treat_information_as_format_doc_contract() {
    let diff = r#"diff --git a/src/Verifier.java b/src/Verifier.java
--- a/src/Verifier.java
+++ b/src/Verifier.java
@@ -10,3 +10,4 @@ class Verifier {
+  // TODO: move the RTL information for emails
 }
"#;
    let entries = diff_risk_inventory(diff, 20);

    assert!(
        entries
            .iter()
            .all(|entry| entry.category != "documentation_contract_consistency"),
        "`information` must not substring-match the `format` documentation-contract signal: {entries:#?}"
    );
}

#[test]
fn diff_risk_inventory_keeps_late_high_priority_code_risks() {
    let mut diff = String::new();
    for locale_index in 0..60 {
        diff.push_str(&format!(
            "diff --git a/i18n/messages_{locale_index}.properties b/i18n/messages_{locale_index}.properties\n--- a/i18n/messages_{locale_index}.properties\n+++ b/i18n/messages_{locale_index}.properties\n@@ -1,1 +1,2 @@\n+key=Copied source text {locale_index}\n"
        ));
    }
    diff.push_str(
        r#"diff --git a/src/Auth.java b/src/Auth.java
--- a/src/Auth.java
+++ b/src/Auth.java
@@ -1,2 +1,4 @@
+  Optional<CredentialModel> credential = credentialStore.find(user);
+  return credential.get().getCredentialData();
"#,
    );

    let entries = diff_risk_inventory(&diff, 10);
    assert!(
        entries
            .iter()
            .any(|entry| entry.category == "unchecked_optional_access"),
        "high-priority code risks should not be crowded out by many localized resource files: {entries:#?}"
    );
}

#[test]
fn diff_risk_inventory_surfaces_regex_matcher_contracts() {
    let diff = r#"diff --git a/src/Sanitizer.java b/src/Sanitizer.java
--- a/src/Sanitizer.java
+++ b/src/Sanitizer.java
@@ -1,3 +1,8 @@
+  import java.util.regex.Matcher;
+  Pattern tagPattern = Pattern.compile("</?a[^>]*>");
+  Matcher sourceMatcher = tagPattern.matcher(source);
+  Matcher targetMatcher = tagPattern.matcher(target);
+  while (targetMatcher.find()) {
+    output = output.replaceFirst(Pattern.quote(sourceMatcher.group()), "");
+  }
"#;
    let entries = diff_risk_inventory(diff, 20);
    assert!(
        entries
            .iter()
            .any(|entry| entry.category == "regex_matcher_contract"),
        "regex/matcher sanitizer changes should become explicit review obligations: {entries:#?}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| !entry.code.starts_with("import ")),
        "plain imports should not consume behavior-risk inventory slots: {entries:#?}"
    );
}

#[test]
fn diff_risk_inventory_prioritizes_matcher_consumption_over_setup() {
    let diff = r#"diff --git a/src/Sanitizer.java b/src/Sanitizer.java
--- a/src/Sanitizer.java
+++ b/src/Sanitizer.java
@@ -1,3 +1,8 @@
+  Pattern tagPattern = Pattern.compile("</?a[^>]*>");
+  Matcher sourceMatcher = tagPattern.matcher(source);
+  Matcher targetMatcher = tagPattern.matcher(target);
+  while (targetMatcher.find()) {
+    output = output.replaceFirst(Pattern.quote(sourceMatcher.group()), "");
+  }
"#;
    let entries = diff_risk_inventory(diff, 20);
    let first_regex = entries
        .iter()
        .find(|entry| entry.category == "regex_matcher_contract")
        .expect("expected a regex matcher risk entry");

    assert!(
        first_regex.code.contains("group()") || first_regex.code.contains("replaceFirst("),
        "stateful matcher consumption should be the highest-signal regex lead: {entries:#?}"
    );
}

#[test]
fn lead_generation_has_category_specific_spelling_guidance() {
    let guidance = lead_generation_category_instruction("suspicious_identifier_spelling");

    assert!(guidance.contains("do not refute solely because"));
    assert!(guidance.contains("private"));
    assert!(guidance.contains("maintenance/search/discoverability"));
}

#[test]
fn lead_generation_has_optional_collection_shape_guidance() {
    let guidance = lead_generation_category_instruction("unchecked_optional_access");

    assert!(guidance.contains("List.class/Map.class"));
    assert!(guidance.contains("same domain value"));
    assert!(guidance.contains("names both the unchecked shape source"));
    assert!(guidance.contains("dominating precondition"));
    assert!(guidance.contains("older pre-existing unwrap"));
    assert!(guidance.contains("changed producer/unwrap pair"));
}

#[test]
fn lead_generation_has_unchecked_collection_shape_guidance() {
    let guidance = lead_generation_category_instruction("unchecked_collection_shape");

    assert!(guidance.contains("deserialization/cast"));
    assert!(guidance.contains("first typed consumer"));
    assert!(guidance.contains("Optional.get()/unwrap()/expect()"));
    assert!(guidance.contains("names both the raw collection shape source"));
    assert!(guidance.contains("validated before typed consumption"));
}

#[test]
fn lead_generation_has_documentation_contract_guidance() {
    let guidance = lead_generation_category_instruction("documentation_contract_consistency");

    assert!(guidance.contains("numeric length/count claims"));
    assert!(guidance.contains("built-in implementations"));
    assert!(guidance.contains("documented typical/example wording"));
    assert!(guidance.contains("usually or example"));
}

#[test]
fn lead_generation_has_matcher_parity_guidance() {
    let guidance = lead_generation_category_instruction("regex_matcher_contract");

    assert!(guidance.contains("source-vs-target parity"));
    assert!(guidance.contains("not only whether group() is guarded"));
    assert!(guidance.contains("extra target groups"));
    assert!(guidance.contains("replacement/break behavior"));
}

#[test]
fn validate_instruction_supports_direct_localized_script_evidence() {
    let instruction = child_final_instruction(DelegateTaskKind::ValidateFinding);

    assert!(instruction.contains("localized resource language/script candidates"));
    assert!(instruction.contains("wrong language or script for that locale"));
    assert!(instruction.contains("without requiring a base-file before value"));
}

#[test]
fn lead_generation_has_persisted_identity_guidance() {
    let guidance = lead_generation_category_instruction("persisted_identity_propagation");

    assert!(guidance.contains("read the changed construction/reconstruction site"));
    assert!(guidance.contains("downstream update/remove/lookup/audit/callback consumer"));
    assert!(guidance.contains("created/stored domain models"));
    assert!(guidance.contains("authoritative stored object"));
    assert!(guidance.contains("leave stale persisted state"));
}

#[test]
fn orchestrator_final_instruction_keeps_connected_collection_optional_evidence_together() {
    let instruction = orchestrator_final_instruction();

    assert!(instruction.contains("unsafe Optional.get()/unwrap()/expect()"));
    assert!(instruction.contains("raw List.class/Map.class"));
    assert!(instruction.contains("one evidence-complete candidate"));
}

#[test]
fn explore_child_final_instruction_forbids_supported_empty_candidates() {
    let instruction = child_final_instruction(DelegateTaskKind::ExploreCode);

    assert!(instruction.contains("candidateFindings contains"));
    assert!(instruction.contains("Do not mark status=supported"));
    assert!(instruction.contains("negative-evidence disclaimers"));
}

#[test]
fn validate_child_final_instruction_exposes_review_concern_tier() {
    let instruction = child_final_instruction(DelegateTaskKind::ValidateFinding);

    assert!(instruction.contains("Use review_concern"));
    assert!(instruction.contains("review_concern is not publishable"));
    assert!(instruction.contains("changed producer"));
    assert!(instruction.contains("dominating presence check"));
    assert!(instruction.contains("one reachable caller/precondition"));
    assert!(instruction.contains("unchecked collection source"));
    assert!(instruction.contains("do not support a narrower optional-only candidate"));
    assert!(instruction.contains("persisted identity candidates"));
    assert!(instruction.contains("authoritative stored id/identity"));
    assert!(instruction.contains("exact replay through every storage backend is not required"));
    assert!(instruction.contains("read the method/consumer body before finalizing"));
    assert!(instruction.contains("negative-evidence disclaimers"));
}

#[test]
fn validation_rescue_retries_missing_evidence_statuses() {
    let ordinary = publication_candidate(
        "Changed grant shortcut can break decoding",
        "A changed shortcut contract can break token decoding.",
    );
    let optional = publication_candidate(
        "Recovery-code form unwraps an Optional without checking presence",
        "The changed form calls Optional.get() on a producer that can return empty.",
    );
    let identity = publication_candidate(
        "Reconstructed credential model drops the stored id",
        "The changed reconstruction path does not copy the stored credential id, so later remove-by-id targets a missing null identity.",
    );
    let spelling = publication_candidate(
        "Method name is misspelled and confusing to future callers",
        "The changed method identifier has a spelling typo, creating maintenance/search/discoverability confusion.",
    );

    assert!(validation_status_needs_rescue_for_candidate(
        "needs_more_evidence",
        &ordinary
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        " insufficient ",
        &ordinary
    ));
    assert!(validation_status_needs_rescue_for_candidate(
        " insufficient ",
        &optional
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        "refuted", &optional
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        "supported",
        &optional
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        "review_concern",
        &optional
    ));
    assert!(validation_status_needs_rescue_for_candidate(
        "insufficient",
        &identity
    ));
    assert!(validation_status_needs_rescue_for_candidate(
        "review_concern",
        &identity
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        "refuted", &identity
    ));
    assert!(!validation_status_needs_rescue_for_candidate(
        "review_concern",
        &ordinary
    ));
    assert!(validation_status_needs_rescue_for_candidate(
        "insufficient",
        &spelling
    ));
}

#[test]
fn validation_budget_expands_for_high_signal_persisted_identity_candidates() {
    let ordinary = publication_candidate(
        "Changed parser accepts invalid shortcut",
        "The changed parser can accept a token with the wrong shortcut shape.",
    );
    let optional = publication_candidate(
        "Recovery-code form unwraps an Optional without checking presence",
        "The changed form calls Optional.get() on a producer that can return empty.",
    );
    let mut identity = publication_candidate(
        "Persisted credential reconstruction drops stored identity",
        "The changed reconstruction path does not preserve the stored id before a later update.",
    );
    identity.negative_outcome =
        "The id-based update can target no persisted object and leave stale stored state."
            .to_string();

    let base_budget = delegates::child_budget(DelegateTaskKind::ValidateFinding);
    let ordinary_budget = validation_budget_for_candidate(&ordinary);
    let optional_budget = validation_budget_for_candidate(&optional);
    let identity_budget = validation_budget_for_candidate(&identity);

    assert_eq!(ordinary_budget.max_turns, base_budget.max_turns);
    assert_eq!(ordinary_budget.max_tool_calls, base_budget.max_tool_calls);
    assert!(optional_budget.max_turns > base_budget.max_turns);
    assert!(optional_budget.max_tool_calls > base_budget.max_tool_calls);
    assert!(identity_budget.max_turns > base_budget.max_turns);
    assert!(identity_budget.max_tool_calls > base_budget.max_tool_calls);
}

#[test]
fn validation_candidate_selection_caps_fanout_but_keeps_high_signal_recall_candidates() {
    let ordinary_a = publication_candidate(
        "Changed branch has a possible edge case",
        "The changed branch may behave differently for one caller.",
    );
    let ordinary_b = publication_candidate(
        "Changed formatter may produce confusing output",
        "The changed formatter can produce a confusing message.",
    );
    let optional = publication_candidate(
        "Changed credential lookup is unwrapped without proving presence",
        "The changed lookup calls Optional.get() without proving the credential is present.",
    );
    let identity = publication_candidate(
        "Reconstructed stored model drops the persisted id",
        "The changed reconstruction does not preserve the stored id before remove-by-id.",
    );
    let matcher = publication_candidate(
        "Matcher group is consumed without matching the same source",
        "The changed regex matcher reads a group after matching a different target matcher.",
    );
    let mut child = publication_candidate(
        "Child-discovered cleanup edge case",
        "The child exploration found a changed cleanup path with a concrete stale-state outcome.",
    );
    child.id = "child_low_signal".to_string();
    child
        .evidence_artifact_ids
        .push("artifact_child_packet".to_string());
    let mut extra_child = publication_candidate(
        "Another child-discovered edge case",
        "A second child exploration found a possible but lower-signal changed cleanup path.",
    );
    extra_child.id = "child_extra".to_string();

    let candidates = vec![
        ordinary_a,
        child,
        optional,
        ordinary_b,
        identity,
        matcher,
        extra_child,
    ];

    let selected = select_validation_candidate_indexes(&candidates, 4);

    assert_eq!(selected.len(), 4);
    assert!(
        selected.contains(&2),
        "optional unwrap candidate should be kept"
    );
    assert!(
        selected.contains(&4),
        "persisted identity candidate should be kept"
    );
    assert!(
        selected.contains(&5),
        "matcher contract candidate should be kept"
    );
    assert!(
        selected.contains(&0),
        "the first ordinary orchestrator candidate should win the remaining slot"
    );
    assert!(
        !selected.contains(&6),
        "low-signal excess child candidates should be capped before validation"
    );
}

#[test]
fn validation_candidate_selection_can_disable_validation_with_zero_budget() {
    let candidate = publication_candidate(
        "Changed branch has a possible edge case",
        "The changed branch may behave differently for one caller.",
    );

    let selected = select_validation_candidate_indexes(&[candidate], 0);

    assert!(selected.is_empty());
}

#[test]
fn lead_generation_retries_only_failed_empty_packets() {
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            false,
            "model_failed",
            false,
            "regex_matcher_contract"
        ),
        Some("failed_empty_packet")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "model_failed",
            false,
            "regex_matcher_contract"
        ),
        Some("failed_empty_packet")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "max_turns",
            false,
            "regex_matcher_contract"
        ),
        Some("failed_empty_packet")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "needs_more_evidence",
            true,
            "max_turns",
            true,
            "regex_matcher_contract"
        ),
        Some("missing_evidence")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "max_turns",
            true,
            "persisted_identity_propagation"
        ),
        Some("missing_evidence")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "max_turns",
            true,
            "documentation_contract_consistency"
        ),
        Some("missing_evidence")
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "max_turns",
            true,
            "suspicious_identifier_spelling"
        ),
        Some("missing_evidence")
    );
    assert_eq!(
        lead_generation_retry_kind(
            1,
            "needs_more_evidence",
            false,
            "model_failed",
            false,
            "regex_matcher_contract"
        ),
        None
    );
    assert_eq!(
        lead_generation_retry_kind(
            0,
            "insufficient",
            true,
            "max_turns",
            true,
            "regex_matcher_contract"
        ),
        None
    );
    assert_eq!(
        lead_generation_retry_kind(
            1,
            "insufficient",
            false,
            "model_failed",
            false,
            "persisted_identity_propagation"
        ),
        None
    );
}

#[test]
fn lead_generation_selects_high_priority_uncovered_risks() {
    let diff = r#"diff --git a/src/Sanitizer.java b/src/Sanitizer.java
--- a/src/Sanitizer.java
+++ b/src/Sanitizer.java
@@ -1,3 +1,8 @@
+  Pattern tagPattern = Pattern.compile("</?a[^>]*>");
+  Matcher sourceMatcher = tagPattern.matcher(source);
+  Matcher targetMatcher = tagPattern.matcher(target);
+  while (targetMatcher.find()) {
+    output = output.replaceFirst(Pattern.quote(sourceMatcher.group()), "");
+  }
diff --git a/i18n/messages_lt.properties b/i18n/messages_lt.properties
--- a/i18n/messages_lt.properties
+++ b/i18n/messages_lt.properties
@@ -1,1 +1,2 @@
+totpStep1=Install one app
diff --git a/i18n/messages_zh_CN.properties b/i18n/messages_zh_CN.properties
--- a/i18n/messages_zh_CN.properties
+++ b/i18n/messages_zh_CN.properties
@@ -1,1 +1,2 @@
+totpStep1=在您的手機上安裝以下應用程式之一：
"#;
    let entries = diff_risk_inventory(diff, 40);
    let existing = vec![CandidateFinding {
        id: "C1".to_string(),
        title: "Lithuanian copied-language issue".to_string(),
        claim: "Lithuanian text changed to another language".to_string(),
        negative_outcome: "Lithuanian users see the wrong language".to_string(),
        severity: Some("medium".to_string()),
        path: "i18n/messages_lt.properties".to_string(),
        start_line: Some(2),
        end_line: Some(2),
        behavior_before: None,
        behavior_after: None,
        evidence_artifact_ids: Vec::new(),
        related_paths: Vec::new(),
    }];
    let selected = select_lead_generation_entries(
        &entries,
        &existing,
        &json!({"unreviewedRiskEntries": ["R1", "R2", "R3"]}),
        2,
    );
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].category, "regex_matcher_contract");
    assert_eq!(selected[0].path, "src/Sanitizer.java");
    assert_eq!(selected[1].category, "localized_script_mismatch");
    assert_eq!(selected[1].path, "i18n/messages_zh_CN.properties");
    assert!(
        selected
            .iter()
            .all(|entry| entry.path != "i18n/messages_lt.properties"),
        "paths with existing candidates should not be re-explored"
    );
}

#[test]
fn lead_generation_keeps_distinct_risks_on_same_path() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/Sanitizer.java".to_string(),
            line: Some(12),
            category: "offset_or_slice_boundary",
            code: "String value = text.substring(0, 2);".to_string(),
            obligation: "Verify slicing bounds.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/Sanitizer.java".to_string(),
            line: Some(80),
            category: "regex_matcher_contract",
            code: "output = output.replaceFirst(sourceMatcher.group(), \"\");".to_string(),
            obligation: "Verify matcher state.",
        },
    ];
    let existing = vec![CandidateFinding {
        id: "C1".to_string(),
        title: "Substring boundary issue".to_string(),
        claim: "The substring uses the wrong bound.".to_string(),
        negative_outcome: "Short inputs crash.".to_string(),
        severity: Some("medium".to_string()),
        path: "src/Sanitizer.java".to_string(),
        start_line: Some(12),
        end_line: Some(12),
        behavior_before: None,
        behavior_after: None,
        evidence_artifact_ids: Vec::new(),
        related_paths: Vec::new(),
    }];

    let selected = select_lead_generation_entries(&entries, &existing, &json!({}), 2);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "R2");
    assert_eq!(selected[0].category, "regex_matcher_contract");
}

#[test]
fn lead_generation_does_not_treat_optional_candidate_as_identity_coverage() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/Credentials.java".to_string(),
            line: Some(42),
            category: "unchecked_optional_access",
            code: "RecoveryCredentialModel model = RecoveryCredentialModel.createFromCredentialModel(storedCredential.get());".to_string(),
            obligation: "Verify presence before unwrap.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/Credentials.java".to_string(),
            line: Some(42),
            category: "persisted_identity_propagation",
            code: "RecoveryCredentialModel model = RecoveryCredentialModel.createFromCredentialModel(storedCredential.get());".to_string(),
            obligation: "Verify reconstructed model preserves persisted identity.",
        },
    ];
    let mut existing = publication_candidate(
        "Stored credential Optional is unwrapped without checking",
        "The changed reconstruction path calls Optional.get() without proving the stored credential is present.",
    );
    existing.path = "src/Credentials.java".to_string();
    existing.start_line = Some(42);
    existing.end_line = Some(42);
    existing.negative_outcome =
        "If the stored credential is absent, the changed path throws NoSuchElementException."
            .to_string();
    existing.behavior_after =
        Some("The changed code calls storedCredential.get() without a guard.".to_string());

    let selected = select_lead_generation_entries(&entries, &[existing], &json!({}), 2);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "R2");
    assert_eq!(selected[0].category, "persisted_identity_propagation");
}

#[test]
fn lead_generation_prioritizes_category_before_unreviewed_status() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "i18n/messages_fr.properties".to_string(),
            line: Some(4),
            category: "localized_resource_change",
            code: "welcome=Welcome".to_string(),
            obligation: "Verify localized resource parity.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/Sanitizer.java".to_string(),
            line: Some(80),
            category: "regex_matcher_contract",
            code: "sourceMatcher.group()".to_string(),
            obligation: "Verify matcher state.",
        },
    ];

    let selected =
        select_lead_generation_entries(&entries, &[], &json!({"unreviewedRiskEntries": ["R1"]}), 1);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "R2");
    assert_eq!(selected[0].category, "regex_matcher_contract");
}

#[test]
fn lead_generation_diversifies_categories_before_duplicate_category_leads() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/DocA.java".to_string(),
            line: Some(10),
            category: "documentation_contract_consistency",
            code: "shortcut is usually 3 letters".to_string(),
            obligation: "Verify docs match code.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/DocB.java".to_string(),
            line: Some(20),
            category: "documentation_contract_consistency",
            code: "shortcut length comment".to_string(),
            obligation: "Verify docs match code.",
        },
        DiffRiskEntry {
            id: "R3".to_string(),
            path: "src/AccessTokenContext.java".to_string(),
            line: Some(30),
            category: "nullability_contract",
            code: "Objects.requireNonNull(grantType);".to_string(),
            obligation: "Verify checked value is consumed value.",
        },
    ];

    let selected = select_lead_generation_entries(&entries, &[], &json!({}), 2);

    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].id, "R1");
    assert_eq!(selected[0].category, "documentation_contract_consistency");
    assert_eq!(selected[1].id, "R3");
    assert_eq!(selected[1].category, "nullability_contract");
}

#[test]
fn lead_generation_prioritizes_broad_exception_over_identity_seed_leads() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/Factory.java".to_string(),
            line: Some(10),
            category: "persisted_identity_propagation",
            code: "return new Provider(session, this);".to_string(),
            obligation: "Verify identity propagation.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/ProviderTest.java".to_string(),
            line: Some(20),
            category: "broad_exception_boundary",
            code: "catch (RuntimeException e) {".to_string(),
            obligation: "Verify broad catch does not hide unrelated failures.",
        },
        DiffRiskEntry {
            id: "R3".to_string(),
            path: "src/GrantType.java".to_string(),
            line: Some(30),
            category: "documentation_contract_consistency",
            code: "usually like 3-letters shortcut".to_string(),
            obligation: "Verify documentation matches implementations.",
        },
    ];

    let selected = select_lead_generation_entries(&entries, &[], &json!({}), 2);

    assert_eq!(
        selected
            .iter()
            .map(|entry| entry.category)
            .collect::<Vec<_>>(),
        vec![
            "broad_exception_boundary",
            "documentation_contract_consistency"
        ]
    );
}

#[test]
fn lead_generation_prioritizes_spelling_over_generic_documentation_leads() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/Docs.java".to_string(),
            line: Some(10),
            category: "documentation_contract_consistency",
            code: "usually like 3-letters shortcut".to_string(),
            obligation: "Verify documentation matches implementations.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/Sanitizer.java".to_string(),
            line: Some(20),
            category: "suspicious_identifier_spelling",
            code: "private String santizeAnchors(String value)".to_string(),
            obligation: "Verify identifier spelling.",
        },
        DiffRiskEntry {
            id: "R3".to_string(),
            path: "src/VerifierTest.java".to_string(),
            line: Some(30),
            category: "broad_exception_boundary",
            code: "catch (RuntimeException ignored) {".to_string(),
            obligation: "Verify broad catch precision.",
        },
    ];

    let selected = select_lead_generation_entries(&entries, &[], &json!({}), 2);

    assert_eq!(
        selected
            .iter()
            .map(|entry| entry.category)
            .collect::<Vec<_>>(),
        vec!["broad_exception_boundary", "suspicious_identifier_spelling"]
    );
}

#[test]
fn diff_risk_inventory_prioritizes_clear_localized_script_mismatches() {
    let diff = r#"diff --git a/i18n/messages_zh_CN.properties b/i18n/messages_zh_CN.properties
--- a/i18n/messages_zh_CN.properties
+++ b/i18n/messages_zh_CN.properties
@@ -1,1 +1,2 @@
+totpStep1=在您的手機上安裝以下應用程式之一：
"#;
    let entries = diff_risk_inventory(diff, 20);
    assert!(
        entries
            .iter()
            .any(|entry| entry.category == "localized_script_mismatch"),
        "Traditional-only Chinese characters in zh_CN resources should become a high-signal localization lead: {entries:#?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.category == "localized_resource_change"),
        "script mismatch should not replace the broader localization review obligation"
    );
}

#[test]
fn diff_risk_inventory_groups_repetitive_localized_resource_lines() {
    let mut diff = String::from(
        "diff --git a/i18n/messages_fr.properties b/i18n/messages_fr.properties\n--- a/i18n/messages_fr.properties\n+++ b/i18n/messages_fr.properties\n@@ -1,1 +1,45 @@\n",
    );
    for index in 0..44 {
        diff.push_str(&format!("+key{index}=English fallback {index}\n"));
    }
    diff.push_str(
        "diff --git a/src/Command.java b/src/Command.java\n--- a/src/Command.java\n+++ b/src/Command.java\n@@ -1,1 +1,3 @@\n+  commandLine.exit(42);\n+  store.findByName(server, resource.getId(), owner.getId());\n",
    );

    let entries = diff_risk_inventory(&diff, 40);
    let localized_count = entries
        .iter()
        .filter(|entry| entry.category == "localized_resource_change")
        .count();
    let categories = entries
        .iter()
        .map(|entry| entry.category)
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(localized_count, 1);
    assert!(categories.contains("process_exit_boundary"));
    assert!(categories.contains("identifier_lookup_contract"));
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
                "negativeOutcome": "Callers can observe success while delete writes are still pending, so failed deletes can be silently skipped.",
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
            "completeness": {
                "reviewedChangedFiles": ["src/workflow.ts"],
                "reviewedRiskEntries": [],
                "unreviewedRiskEntries": [],
                "unresolvedQuestions": [],
                "incompleteReasons": [],
                "ignoredChildCandidates": []
            }
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
    assert_eq!(
        candidate.negative_outcome,
        "Callers can observe success while delete writes are still pending, so failed deletes can be silently skipped."
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
        "The changed handler has multiple issues: it drops failed retries and skips cleanup after errors.",
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
fn publication_gate_rejects_inline_todo_comment_only_contracts() {
    let candidate = publication_candidate(
        "Inline TODO comment describes a stricter normalization order",
        "The inline comment sits inside a path that still requires another file before the TODO-described normalization can apply.",
    );

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        Some("weak_documentation_contract")
    );
}

#[test]
fn publication_gate_accepts_changed_misspelled_identifier_without_behavior_before() {
    let mut candidate = publication_candidate(
        "Anchor sanitization helper name is misspelled",
        "The new anchor sanitization helper is misspelled as `santizeAnchors`; adjacent sanitizer code makes the intended term `sanitizeAnchors` clear.",
    );
    candidate.behavior_before = None;
    candidate.behavior_after =
        Some("The helper introduced for anchor handling is named `santizeAnchors`.".to_string());
    candidate.negative_outcome =
        "Future maintainers searching for sanitization helpers may miss this method or propagate the typo, reducing readability and discoverability."
            .to_string();

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
fn publication_gate_accepts_single_matcher_invariant_with_two_checks() {
    let candidate = publication_candidate(
        "Matcher checks the wrong field and returns on equality",
        "The changed matcher reads the wrong two-character window and also rejects the value when it equals the expected shortcut.",
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
fn publication_gate_accepts_negated_identity_preservation_failure() {
    let mut candidate = publication_candidate(
        "Reconstructed record drops the stored identifier",
        "The changed reconstruction path does not preserve the stored record id before calling updateRecord.",
    );
    candidate.negative_outcome =
        "The update can target no persisted record and leave stale stored state behind."
            .to_string();
    candidate.behavior_before = Some(
        "The update path copied the stored record id into the reconstructed model.".to_string(),
    );
    candidate.behavior_after = Some(
        "The update path reconstructs the model without preserving the stored id.".to_string(),
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
fn publication_gate_accepts_inconsistent_regression_text() {
    let candidate = publication_candidate(
        "Simplified Chinese message uses Traditional Chinese",
        "The changed locale string is inconsistent with the Simplified Chinese locale, causing a localization regression for zh_CN users.",
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
fn publication_gate_accepts_user_visible_localization_language_failures() {
    let mut candidate = publication_candidate(
        "Lithuanian setup message is copied from another language",
        "The Lithuanian locale accidentally contains an Italian translation for the setup step.",
    );
    candidate.negative_outcome =
        "Users selecting Lithuanian will see Italian text in the setup flow.".to_string();
    candidate.behavior_before = Some("The same localized key was Lithuanian.".to_string());
    candidate.behavior_after = Some("The localized key is now Italian text.".to_string());

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
fn publication_gate_accepts_locale_users_see_wrong_language_instruction() {
    let mut candidate = publication_candidate(
        "Lithuanian translation was replaced with Italian text",
        "In messages_lt.properties, the changed totpStep1 value is Italian rather than Lithuanian.",
    );
    candidate.negative_outcome =
        "Users of the Lithuanian locale see an Italian instruction in the account setup flow."
            .to_string();
    candidate.behavior_before =
        Some("The same locale file contained Lithuanian text for the setup step.".to_string());
    candidate.behavior_after =
        Some("The Lithuanian locale now contains an Italian instruction.".to_string());

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
fn publication_gate_accepts_user_visible_localized_script_failures() {
    let mut candidate = publication_candidate(
        "zh_CN setup message uses Traditional Chinese script",
        "The Simplified Chinese locale contains a Traditional Chinese-script replacement for the setup step.",
    );
    candidate.negative_outcome =
        "Users selecting zh_CN will see inconsistent Traditional Chinese text in the setup flow."
            .to_string();
    candidate.behavior_before =
        Some("Neighboring zh_CN strings use Simplified Chinese forms.".to_string());
    candidate.behavior_after = Some(
        "The changed zh_CN string uses Traditional Chinese forms such as 手機 and 安裝."
            .to_string(),
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
fn publication_gate_accepts_concrete_negative_outcome_without_behavior_comparison() {
    let mut candidate = publication_candidate(
        "Async callback may skip pending deletes",
        "The changed forEach async callback returns success before delete promises finish, so failed deletes can be silently skipped.",
    );
    candidate.behavior_before = None;
    candidate.behavior_after = None;

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
fn publication_gate_accepts_unchanged_primary_path_with_changed_related_path() {
    let mut candidate = publication_candidate(
        "Caller observes changed encoder contract",
        "The unchanged caller now receives token ids from the changed encoder before consumers parse them.",
    );
    candidate.path = "src/caller.ts".to_string();
    candidate.related_paths = vec!["src/workflow.ts".to_string()];

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
fn publication_gate_rejects_unchanged_primary_path_without_changed_related_path() {
    let mut candidate = publication_candidate(
        "Caller observes changed encoder contract",
        "The unchanged caller now receives token ids from the changed encoder before consumers parse them.",
    );
    candidate.path = "src/caller.ts".to_string();
    candidate.related_paths = vec!["src/other.ts".to_string()];

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        Some("unchanged_path")
    );
}

#[test]
fn publication_gate_accepts_rejected_contract_outcome_without_behavior_before() {
    let mut candidate = publication_candidate(
        "Shortcut docs allow a width the parser rejects",
        "The changed API contract says implementers can return a three-character shortcut, but the changed parser consumes a fixed two-character field.",
    );
    candidate.negative_outcome =
        "Implementations following the documented shortcut length are rejected during token parsing."
            .to_string();
    candidate.behavior_before = None;
    candidate.behavior_after = Some(
        "The changed parser slices a fixed two-character shortcut from the token prefix."
            .to_string(),
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
fn publication_gate_rejects_test_fixture_claiming_production_user_impact() {
    let mut candidate = publication_candidate(
        "Federated recovery codes validate without being consumed",
        "The test storage provider validates recovery-code credentials without consuming the matched code.",
    );
    candidate.path = "testsuite/integration/services/TestStorage.java".to_string();
    candidate.start_line = Some(20);
    candidate.end_line = Some(20);
    candidate.negative_outcome =
        "A user can authenticate multiple times with the same recovery credential.".to_string();
    candidate.behavior_before =
        Some("Production credentials consumed used recovery codes.".to_string());
    candidate.behavior_after =
        Some("The test storage provider leaves the submitted credential reusable.".to_string());

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths_for(&candidate.path),
            &publication_changed_ranges_for(&candidate.path, 20, 20),
        ),
        Some("test_fixture_production_impact")
    );
}

#[test]
fn publication_gate_keeps_test_scoped_failures_in_test_paths() {
    let mut candidate = publication_candidate(
        "Broad RuntimeException catch lets invalid-grant test pass on unrelated NPE",
        "The changed test catches RuntimeException even though the implementation throws IllegalArgumentException.",
    );
    candidate.path = "services/src/test/java/org/example/ProviderTest.java".to_string();
    candidate.start_line = Some(20);
    candidate.end_line = Some(20);
    candidate.negative_outcome =
        "The unit test can report success for an unrelated NullPointerException.".to_string();
    candidate.behavior_before =
        Some("The test was intended to verify malformed-token rejection.".to_string());
    candidate.behavior_after =
        Some("The broad catch accepts any runtime exception as success.".to_string());

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths_for(&candidate.path),
            &publication_changed_ranges_for(&candidate.path, 20, 20),
        ),
        None
    );
}

#[test]
fn publication_gate_rejects_vague_candidate_without_behavior_comparison() {
    let mut candidate = publication_candidate(
        "Async callback changed",
        "The changed callback may have different behavior.",
    );
    candidate.negative_outcome.clear();
    candidate.behavior_before = None;
    candidate.behavior_after = None;

    assert_eq!(
        autonomous_candidate_rejection_reason(
            &candidate,
            &publication_changed_paths(),
            &publication_changed_ranges(),
        ),
        Some("missing_negative_outcome")
    );
}

#[test]
fn build_findings_records_publication_decisions() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let accepted = publication_candidate(
        "Async callback returns before writes finish",
        "The changed forEach async callback returns success before delete promises finish, so failed deletes can be silently skipped.",
    );
    let missing_validation = publication_candidate(
        "Missing validator result",
        "The changed handler returns success before the queued delete is observed by callers.",
    );
    let validation = ValidationPacket {
        candidate_id: accepted.id.clone(),
        status: "supported".to_string(),
        summary: "raw diff and file evidence support the candidate".to_string(),
        artifact_id: None,
        child_session_id: Some("review-orchestrator/validate-0001".to_string()),
    };

    let outcome = build_findings(
        &tools,
        &snapshot,
        "head",
        &[accepted.clone(), missing_validation.clone()],
        &[validation],
    );

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.publication_decisions.len(), 2);
    let accepted_decision = outcome
        .publication_decisions
        .iter()
        .find(|decision| decision.candidate_id == accepted.id)
        .expect("accepted decision");
    assert_eq!(accepted_decision.decision, "accepted");
    assert_eq!(accepted_decision.reason, "published");
    assert_eq!(
        accepted_decision.validator_session_id.as_deref(),
        Some("review-orchestrator/validate-0001")
    );
    let rejected_decision = outcome
        .publication_decisions
        .iter()
        .find(|decision| decision.candidate_id == missing_validation.id)
        .expect("rejected decision");
    assert_eq!(rejected_decision.decision, "rejected");
    assert_eq!(rejected_decision.reason, "missing_validation");
    assert_eq!(outcome.rejection_reasons["missing_validation"], 1);
}

#[test]
fn build_findings_rejects_duplicate_candidate_ids_before_validation_reuse() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let first = publication_candidate(
        "Async callback returns before writes finish",
        "The changed forEach async callback returns success before delete promises finish.",
    );
    let mut second = publication_candidate(
        "Different candidate with reused id",
        "The changed handler drops failed retries after reporting success.",
    );
    second.id = first.id.clone();
    let validation = ValidationPacket {
        candidate_id: first.id.clone(),
        status: "supported".to_string(),
        summary: "raw diff and file evidence support the first candidate".to_string(),
        artifact_id: None,
        child_session_id: Some("review-orchestrator/validate-0001".to_string()),
    };

    let outcome = build_findings(&tools, &snapshot, "head", &[first, second], &[validation]);

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.rejection_reasons["duplicate_candidate_id"], 1);
}

#[test]
fn build_findings_rejects_duplicate_published_behavior() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let first = publication_candidate(
        "Async delete promises are reported before being awaited",
        "The changed workflow reports success while delete promises are still running.",
    );
    let second = publication_candidate(
        "Async deletes are accepted before being awaited",
        "The changed handler lets success return before the same delete promises finish.",
    );
    let validations = [
        ValidationPacket {
            candidate_id: first.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the first candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0001".to_string()),
        },
        ValidationPacket {
            candidate_id: second.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the second candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0002".to_string()),
        },
    ];

    let outcome = build_findings(&tools, &snapshot, "head", &[first, second], &validations);

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.rejection_reasons["duplicate_published_behavior"], 1);
    assert_eq!(outcome.publication_decisions[1].decision, "rejected");
    assert_eq!(
        outcome.publication_decisions[1].reason,
        "duplicate_published_behavior"
    );
}

#[test]
fn build_findings_rejects_duplicate_published_behavior_with_different_title_words() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let first = publication_candidate(
        "Federated recovery code validation does not consume used codes",
        "The changed validation path checks the submitted recovery code but does not consume it.",
    );
    let second = publication_candidate(
        "Federated recovery codes validate without being consumed",
        "The changed validator accepts the same recovery code without removing the matched value.",
    );
    let validations = [
        ValidationPacket {
            candidate_id: first.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the first candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0001".to_string()),
        },
        ValidationPacket {
            candidate_id: second.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the second candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0002".to_string()),
        },
    ];

    let outcome = build_findings(&tools, &snapshot, "head", &[first, second], &validations);

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.rejection_reasons["duplicate_published_behavior"], 1);
    assert_eq!(outcome.publication_decisions[1].decision, "rejected");
}

#[test]
fn build_findings_rejects_duplicate_behavior_with_shared_affected_context() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let mut first = publication_candidate(
        "requestNonce null check accidentally checks tenantId",
        "The changed RequestContext constructor validates tenantId twice and never validates requestNonce.",
    );
    first.negative_outcome =
        "Invalid RequestContext instances can be created with a null requestNonce.".to_string();
    first.path = "src/RequestContext.ts".to_string();
    first.related_paths = vec!["src/workflow.ts".to_string()];
    let mut second = publication_candidate(
        "RequestContext does not enforce requestNonce before encoding",
        "The changed request encoding path intends to reject null requestNonce values, but RequestContext checks tenantId instead.",
    );
    second.negative_outcome =
        "The encoder can produce request identifiers with a null requestNonce suffix.".to_string();
    second.path = "src/RequestEncoder.ts".to_string();
    second.related_paths = vec![
        "src/RequestContext.ts".to_string(),
        "src/workflow.ts".to_string(),
    ];
    let validations = [
        ValidationPacket {
            candidate_id: first.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the first candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0001".to_string()),
        },
        ValidationPacket {
            candidate_id: second.id.clone(),
            status: "supported".to_string(),
            summary: "raw diff and file evidence support the second candidate".to_string(),
            artifact_id: None,
            child_session_id: Some("review-orchestrator/validate-0002".to_string()),
        },
    ];

    let outcome = build_findings(&tools, &snapshot, "head", &[first, second], &validations);

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.rejection_reasons["duplicate_published_behavior"], 1);
}

#[test]
fn build_findings_rejects_validator_review_concerns_from_publication() {
    let snapshot = publication_snapshot();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let tools = ToolEngine::new(Arc::clone(&snapshot), limits).expect("tool engine");
    let candidate = publication_candidate(
        "Changed matcher can skip an edge-case validation",
        "The changed matcher/replacement loop can miss an unauthorized duplicated token in a localized message.",
    );
    let validation = ValidationPacket {
        candidate_id: candidate.id.clone(),
        status: "review_concern".to_string(),
        summary: "raw changed code establishes an actionable edge-case concern".to_string(),
        artifact_id: None,
        child_session_id: Some("review-orchestrator/validate-0007".to_string()),
    };

    let outcome = build_findings(&tools, &snapshot, "head", &[candidate], &[validation]);

    assert!(outcome.findings.is_empty());
    assert_eq!(outcome.rejection_reasons["validator_review_concern"], 1);
    assert_eq!(outcome.publication_decisions[0].decision, "rejected");
    assert_eq!(
        outcome.publication_decisions[0].validator_status.as_deref(),
        Some("review_concern")
    );
}

#[test]
fn child_candidate_discoveries_are_promoted_with_stable_unique_ids() {
    let orchestrator = publication_candidate(
        "Async callback returns before writes finish",
        "The changed forEach async callback returns success before delete promises finish.",
    );
    let mut duplicate_child = orchestrator.clone();
    duplicate_child.id = "child-used-same-claim".to_string();
    let mut distinct_child = publication_candidate(
        "Queued delete is not awaited",
        "The changed handler reports success before the queued delete promise is observed.",
    );
    distinct_child.id = orchestrator.id.clone();

    let merged = merged_candidate_findings(
        std::slice::from_ref(&orchestrator),
        &[
            ChildCandidateDiscovery {
                candidate: duplicate_child,
                child_session_id: "review-orchestrator/search-0001".to_string(),
                task_type: "search_code",
                artifact_id: None,
            },
            ChildCandidateDiscovery {
                candidate: distinct_child,
                child_session_id: "review-orchestrator/explore-0002".to_string(),
                task_type: "explore_code",
                artifact_id: Some(ArtifactId("artifact_child_packet".to_string())),
            },
        ],
    );

    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].id, orchestrator.id);
    assert_ne!(merged[1].id, orchestrator.id);
    assert!(merged[1].id.starts_with("child_"));
    assert_eq!(merged[1].evidence_artifact_ids, ["artifact_child_packet"]);
}

#[test]
fn compact_child_packet_preserves_rescue_breadcrumbs() {
    let packet = json!({
        "status": "insufficient",
        "summary": "Need one downstream consumer before supporting the candidate.",
        "checkedPaths": ["src/ModelFactory.java"],
        "openQuestions": [
            "Which downstream update/remove/lookup consumes the reconstructed model id?"
        ],
        "suggestedNextSearches": [
            "Search for removeStoredCredentialById or updateCredential consumers of the reconstructed model."
        ],
        "evidence": [{
            "path": "src/ModelFactory.java",
            "startLine": 40,
            "endLine": 55,
            "whyItMatters": "The factory rebuilds the domain model without setting the stored id."
        }],
        "candidateFindings": []
    });

    let compact = findings::compact_child_packet(
        DelegateTaskKind::ValidateFinding,
        &SessionId("review-orchestrator/validate-0001".to_string()),
        &packet,
        &ArtifactId("artifact_validation_packet".to_string()),
    );

    assert_eq!(
        compact["openQuestions"][0],
        "Which downstream update/remove/lookup consumes the reconstructed model id?"
    );
    assert_eq!(
        compact["suggestedNextSearches"][0],
        "Search for removeStoredCredentialById or updateCredential consumers of the reconstructed model."
    );
    assert_eq!(compact["evidence"][0]["path"], "src/ModelFactory.java");
    assert_eq!(
        compact["evidence"][0]["whyItMatters"],
        "The factory rebuilds the domain model without setting the stored id."
    );
}

#[test]
fn risk_seed_candidates_surface_uncovered_persisted_identity_risks() {
    let entries = vec![DiffRiskEntry {
        id: "R1".to_string(),
        path: "src/Credentials.java".to_string(),
        line: Some(42),
        category: "persisted_identity_propagation",
        code: "CredentialModel model = CredentialModel.createFromStored(stored);".to_string(),
        obligation: "Verify reconstructed model preserves stored identity.",
    }];

    let seeds = risk_seed_candidate_findings(&entries, &[], 2);

    assert_eq!(seeds.len(), 1);
    assert!(seeds[0].id.starts_with("risk_seed_"));
    assert_eq!(seeds[0].path, "src/Credentials.java");
    assert_eq!(seeds[0].start_line, Some(42));
    assert_eq!(seeds[0].end_line, Some(42));
    assert!(seeds[0].claim.contains("id/identity propagation"));
    assert!(seeds[0].negative_outcome.contains("stale stored state"));
}

#[test]
fn risk_seed_candidates_skip_covered_identity_risks_and_other_categories() {
    let entries = vec![
        DiffRiskEntry {
            id: "R1".to_string(),
            path: "src/Credentials.java".to_string(),
            line: Some(42),
            category: "persisted_identity_propagation",
            code: "CredentialModel model = CredentialModel.createFromStored(stored);".to_string(),
            obligation: "Verify reconstructed model preserves stored identity.",
        },
        DiffRiskEntry {
            id: "R2".to_string(),
            path: "src/Credentials.java".to_string(),
            line: Some(50),
            category: "unchecked_optional_access",
            code: "stored.get()".to_string(),
            obligation: "Verify presence before unwrap.",
        },
    ];
    let mut existing = publication_candidate(
        "Reconstructed credential drops the stored id",
        "The changed reconstruction does not preserve the stored id before remove-by-id.",
    );
    existing.path = "src/Credentials.java".to_string();
    existing.start_line = Some(42);
    existing.end_line = Some(42);
    existing.negative_outcome =
        "The remove call can target no persisted credential and leave stale stored state."
            .to_string();

    let seeds = risk_seed_candidate_findings(&entries, &[existing], 2);

    assert!(seeds.is_empty());
}

#[test]
fn final_output_schema_gate_requires_required_fields() {
    assert!(session_output_valid(
        SessionKind::Orchestrator,
        Some(
            r#"{"verdict":"clean","summary":"done","candidates":[],"notes":[],"completeness":{"reviewedChangedFiles":[],"reviewedRiskEntries":[],"unreviewedRiskEntries":[],"unresolvedQuestions":[],"incompleteReasons":[],"ignoredChildCandidates":[]}}"#
        )
    ));
    assert!(!session_output_valid(
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
    assert!(session_output_valid(
        SessionKind::Child(DelegateTaskKind::ValidateFinding),
        Some(
            r#"{"status":"review_concern","summary":"actionable concern","checkedPaths":[],"evidence":[],"openQuestions":[],"candidateFindings":[]}"#
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
            negative_outcome:
                "Callers can observe success before delete promises finish, so failed deletes can be silently skipped."
                    .to_string(),
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

fn publication_changed_paths_for(path: &str) -> std::collections::BTreeSet<String> {
    [path.to_string()].into_iter().collect()
}

fn publication_changed_ranges_for(
    path: &str,
    start: usize,
    end: usize,
) -> BTreeMap<String, Vec<(usize, usize)>> {
    BTreeMap::from([(path.to_string(), vec![(start, end)])])
}

fn publication_snapshot() -> Arc<RepoSnapshot> {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("src")).expect("create src");
    std::fs::write(
        temp.path().join("src/workflow.ts"),
        "export async function run(items) {\n  items.forEach(async (item) => deleteItem(item));\n}\n",
    )
    .expect("write workflow");
    let change = ChangeScopeV1 {
        kind: ChangeKind::LocalDiff,
        change_id: "publication-test".to_string(),
        source_ref: "head".to_string(),
        target_ref: "base".to_string(),
        base_revision_id: "base".to_string(),
        head_revision_id: "head".to_string(),
        merge_base_revision_id: None,
        changed_files_manifest_ref: None,
        diff_manifest_ref: None,
        inline_diff: Some(
            r#"diff --git a/src/workflow.ts b/src/workflow.ts
--- a/src/workflow.ts
+++ b/src/workflow.ts
@@ -42,6 +42,7 @@ export async function run(items) {
+  items.forEach(async (item) => deleteItem(item));
 }
"#
            .to_string(),
        ),
        snapshot_mode: SnapshotMode::WorktreeHead,
        rename_detection: RenameDetection::None,
        changed_files: vec![ChangedFileEntryV1 {
            status: ChangedFileStatus::Modified,
            old_path: Some(PathBuf::from("src/workflow.ts")),
            new_path: Some(PathBuf::from("src/workflow.ts")),
            old_content_hash: None,
            new_content_hash: None,
            is_binary: false,
            is_generated: false,
        }],
    };
    RepoSnapshot::build(temp.path(), &PathPolicyV1::bench(64, 20), &change).expect("snapshot")
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
