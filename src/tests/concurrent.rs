use super::prelude::*;
use super::support::*;

#[test]
fn concurrent_tool_batch_rejects_finish_mixed_with_other_tools() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = std::sync::Arc::new(RuntimeLimits::standard(1, 64 * 1024, 10));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![
            ModelToolCall {
                call_id: ToolCallId("finish".to_string()),
                index: 0,
                name: ToolId::from(ToolName::Finish),
                raw_arguments: r#"{"reason":"done"}"#.to_string(),
            },
            ModelToolCall {
                call_id: ToolCallId("read".to_string()),
                index: 1,
                name: ToolId::from(ToolName::ReadDiff),
                raw_arguments: "{}".to_string(),
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.ok));
}

#[test]
fn concurrent_duplicate_search_uses_one_underlying_scan() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..200 {
        fs::write(
            temp.path().join(format!("file-{index}.rs")),
            format!("fn f_{index}() {{ let needle = {index}; }}\n"),
        )
        .unwrap();
    }
    let change = test_change_with_file("file-0.rs");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = std::sync::Arc::new(RuntimeLimits::standard(10, 64 * 1024, 20));
    let engine = std::sync::Arc::new(ToolEngine::new(snapshot, limits).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut joins = tokio::task::JoinSet::new();
        for index in 0..10 {
            let engine = std::sync::Arc::clone(&engine);
            joins.spawn(async move {
                let scope = test_scope(&format!("session-{index}"));
                engine
                    .execute_batch(
                        scope,
                        TurnId(0),
                        vec![ModelToolCall {
                            call_id: ToolCallId(format!("search-{index}")),
                            index: 0,
                            name: ToolId::from(ToolName::SearchText),
                            raw_arguments: r#"{"query":"needle"}"#.to_string(),
                        }],
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
            });
        }
        while let Some(result) = joins.join_next().await {
            let batch = result.unwrap();
            assert_eq!(batch.len(), 1);
            assert!(batch[0].ok);
        }
    });
    let counters = engine.snapshot_counters();
    assert_eq!(counters.search_scans, 1);
}

#[test]
fn concurrent_queued_search_observes_cancellation_before_permit() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..50 {
        fs::write(
            temp.path().join(format!("file-{index}.rs")),
            format!("fn f_{index}() {{ let needle = {index}; }}\n"),
        )
        .unwrap();
    }
    let change = test_change_with_file("file-0.rs");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(1, 64 * 1024, 20);
    limits.max_search_jobs_global = 1;
    let engine = Arc::new(ToolEngine::new(snapshot, Arc::new(limits)).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let search_permit = engine.search.acquire_search_permit_for_test().await;
        let cancel = tokio_util::sync::CancellationToken::new();
        let queued_engine = Arc::clone(&engine);
        let queued_cancel = cancel.clone();
        let mut queued = tokio::spawn(async move {
            queued_engine
                .execute_batch(
                    test_scope("queued-search"),
                    TurnId(0),
                    vec![search_call("queued-search-call")],
                    queued_cancel,
                )
                .await
        });
        wait_for_inflight_tool(&engine).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut queued)
                .await
                .is_err(),
            "search should stay queued while the permit is held"
        );

        cancel.cancel();
        let results = tokio::time::timeout(std::time::Duration::from_millis(200), queued)
            .await
            .expect("queued search should observe cancellation")
            .expect("queued search task should join");
        drop(search_permit);

        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        assert_eq!(
            results[0].error.as_ref().unwrap().code,
            ToolErrorCode::Cancelled
        );
        assert_eq!(engine.snapshot_counters().search_scans, 0);
        assert_eq!(engine.inflight_tool_count_for_test(), 0);
        let metrics = engine.snapshot_tool_metrics();
        let search_metrics = &metrics[&ToolMetricKey::builtin(ToolName::SearchText)];
        assert_eq!(search_metrics.calls, 1);
        assert_eq!(search_metrics.errors, 1);
        assert_eq!(search_metrics.cancellations, 1);
    });
}

#[test]
fn concurrent_deduped_search_waiter_observes_its_own_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..50 {
        fs::write(
            temp.path().join(format!("file-{index}.rs")),
            format!("fn f_{index}() {{ let needle = {index}; }}\n"),
        )
        .unwrap();
    }
    let change = test_change_with_file("file-0.rs");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(2, 64 * 1024, 20);
    limits.max_search_jobs_global = 1;
    let engine = Arc::new(ToolEngine::new(snapshot, Arc::new(limits)).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let search_permit = engine.search.acquire_search_permit_for_test().await;
        let owner_engine = Arc::clone(&engine);
        let owner = tokio::spawn(async move {
            owner_engine
                .execute_batch(
                    test_scope("dedupe-owner"),
                    TurnId(0),
                    vec![search_call("owner-search-call")],
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        });
        wait_for_inflight_tool(&engine).await;

        let waiter_cancel = tokio_util::sync::CancellationToken::new();
        let waiter_engine = Arc::clone(&engine);
        let waiter_cancel_for_task = waiter_cancel.clone();
        let mut waiter = tokio::spawn(async move {
            waiter_engine
                .execute_batch(
                    test_scope("dedupe-waiter"),
                    TurnId(0),
                    vec![search_call("waiter-search-call")],
                    waiter_cancel_for_task,
                )
                .await
        });
        wait_for_search_dedupe_waiter(&engine).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "deduped waiter should stay blocked while owner is still queued"
        );

        waiter_cancel.cancel();
        let waiter_results = tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("deduped waiter should observe its own cancellation")
            .expect("deduped waiter task should join");
        assert_eq!(waiter_results.len(), 1);
        assert!(!waiter_results[0].ok);
        assert_eq!(
            waiter_results[0].error.as_ref().unwrap().code,
            ToolErrorCode::Cancelled
        );

        drop(search_permit);
        let owner_results = tokio::time::timeout(std::time::Duration::from_secs(1), owner)
            .await
            .expect("owner search should finish after permit release")
            .expect("owner search task should join");
        assert_eq!(owner_results.len(), 1);
        assert!(owner_results[0].ok);
        assert_eq!(engine.snapshot_counters().search_dedupe_waiters, 1);
        assert_eq!(engine.snapshot_counters().search_scans, 1);
        let metrics = engine.snapshot_tool_metrics();
        let search_metrics = &metrics[&ToolMetricKey::builtin(ToolName::SearchText)];
        assert_eq!(search_metrics.calls, 2);
        assert_eq!(search_metrics.successes, 1);
        assert_eq!(search_metrics.errors, 1);
        assert_eq!(search_metrics.cancellations, 1);
    });
}

#[test]
fn concurrent_duplicate_tool_calls_in_one_turn_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![
            ModelToolCall {
                call_id: ToolCallId("read-diff-1".to_string()),
                index: 0,
                name: ToolId::from(ToolName::ReadDiff),
                raw_arguments: "{}".to_string(),
            },
            ModelToolCall {
                call_id: ToolCallId("read-diff-2".to_string()),
                index: 1,
                name: ToolId::from(ToolName::ReadDiff),
                raw_arguments: "{}".to_string(),
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(results.len(), 2);
    assert!(results[0].ok);
    assert!(!results[1].ok);
    assert_eq!(
        results[1].error.as_ref().unwrap().code,
        ToolErrorCode::InvalidArgs
    );
    let metrics = engine.snapshot_tool_metrics();
    let read_diff_metrics = &metrics[&ToolMetricKey::builtin(ToolName::ReadDiff)];
    assert_eq!(read_diff_metrics.successes, 1);
    assert_eq!(read_diff_metrics.errors, 1);
}

#[test]
fn concurrent_tool_invalid_args_and_path_denied_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![
            ModelToolCall {
                call_id: ToolCallId("invalid-args".to_string()),
                index: 0,
                name: ToolId::from(ToolName::ReadFile),
                raw_arguments: "{}".to_string(),
            },
            ModelToolCall {
                call_id: ToolCallId("path-denied".to_string()),
                index: 1,
                name: ToolId::from(ToolName::ReadFile),
                raw_arguments: serde_json::json!({ "path": "missing.md" }).to_string(),
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(results.len(), 2);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::InvalidArgs
    );
    assert!(!results[1].ok);
    assert_eq!(
        results[1].error.as_ref().unwrap().code,
        ToolErrorCode::PathDenied
    );
    assert_eq!(engine.snapshot_counters().tool_errors, 2);
    let metrics = engine.snapshot_tool_metrics();
    let read_file_metrics = &metrics[&ToolMetricKey::builtin(ToolName::ReadFile)];
    assert_eq!(read_file_metrics.calls, 2);
    assert_eq!(read_file_metrics.errors, 2);
}

#[test]
fn concurrent_search_cache_is_scoped_by_filesystem_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("README.md"), "needle in root\n").unwrap();
    fs::write(temp.path().join("src/lib.rs"), "needle in src\n").unwrap();
    let change = test_change_with_file("src/lib.rs");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(2, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let root = runtime.block_on(engine.execute_batch(
        test_scope("root-session"),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("root-search".to_string()),
            index: 0,
            name: ToolId::from(ToolName::SearchText),
            raw_arguments: r#"{"query":"needle"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert!(root[0].ok);
    assert_eq!(root[0].limits.searched_files, 2);

    let mut scoped_capabilities = CapabilitySet::review_read_only();
    scoped_capabilities.fs_scope = FsScope::subtree(RepoPath::parse("src").unwrap());
    let scoped = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("src-session", scoped_capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("src-search".to_string()),
            index: 0,
            name: ToolId::from(ToolName::SearchText),
            raw_arguments: r#"{"query":"needle"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert!(scoped[0].ok);
    assert_eq!(scoped[0].limits.searched_files, 1);
    let matches = scoped[0].data.as_ref().unwrap()["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(matches.iter().all(|line| line.starts_with("src/lib.rs:")));

    let counters = engine.snapshot_counters();
    assert_eq!(counters.search_scans, 2);
    let metrics = engine.snapshot_tool_metrics();
    let search_key = ToolMetricKey::builtin(ToolName::SearchText);
    assert_eq!(
        search_key.provider_id(),
        Some(ToolProviderId::builtin_review())
    );
    let search_metrics = &metrics[&search_key];
    assert_eq!(search_metrics.calls, 2);
    assert_eq!(search_metrics.successes, 2);
}

#[test]
fn concurrent_read_file_serves_captured_snapshot_after_worktree_mutation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "original needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    fs::write(temp.path().join("README.md"), "mutated needle\n").unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("read".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadFile),
            raw_arguments: serde_json::json!({ "path": "README.md" }).to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    let content = results[0].data.as_ref().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(content.contains("original needle"));
    assert!(!content.contains("mutated needle"));
}

#[test]
fn concurrent_search_serves_captured_snapshot_after_worktree_mutation() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "original needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    fs::write(temp.path().join("README.md"), "mutated needle\n").unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("search".to_string()),
            index: 0,
            name: ToolId::from(ToolName::SearchText),
            raw_arguments: serde_json::json!({ "query": "needle" }).to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    let matches = results[0].data.as_ref().unwrap()["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(matches.iter().any(|line| line.contains("original needle")));
    assert!(!matches.iter().any(|line| line.contains("mutated needle")));
}

#[test]
fn concurrent_tool_grant_enforces_max_calls_per_session() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut capabilities = CapabilitySet::review_read_only();
    capabilities
        .tool_grants
        .get_mut(&ToolId::from(ToolName::ReadDiff))
        .unwrap()
        .max_calls = Some(1);

    let first = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session-a", capabilities.clone()),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("read-diff-first".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadDiff),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(first.len(), 1);
    assert!(first[0].ok);

    let second = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session-a", capabilities.clone()),
        TurnId(1),
        vec![ModelToolCall {
            call_id: ToolCallId("read-diff-second".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadDiff),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(second.len(), 1);
    assert!(!second[0].ok);
    assert_eq!(
        second[0].error.as_ref().unwrap().code,
        ToolErrorCode::BudgetExceeded
    );

    let other_session = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session-b", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("read-diff-other-session".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadDiff),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(other_session.len(), 1);
    assert!(other_session[0].ok);
}

#[test]
fn concurrent_tool_grant_denies_effects_outside_grant() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 20);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20));
    let engine = ToolEngine::new(snapshot, limits).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.grant_tool(
        ToolId::from(ToolName::ReadDiff),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("effect-denied".to_string()),
            index: 0,
            name: ToolId::from(ToolName::ReadDiff),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::ToolNotAllowed
    );
}

#[test]
fn concurrent_custom_tool_artifact_write_requires_grant() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 10));
    let tool_id = ToolId::parse("artifact_writer").unwrap();
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_custom_with_effects(
            tool_id.clone(),
            "Custom check that writes an artifact.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            false,
            ToolEffects {
                artifact_write: true,
                ..ToolEffects::default()
            },
            Arc::new(EchoCustomTool),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, limits, Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("artifact-write-denied".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: r#"{"value":"ok"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::ToolNotAllowed
    );
}

#[test]
fn concurrent_in_process_tool_provider_timeout_is_typed() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    limits.max_tool_provider_ms = 5;
    let tool_id = ToolId::parse("slow_custom_check").unwrap();
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_custom(
            tool_id.clone(),
            "Slow custom check used to prove provider timeout.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            false,
            Arc::new(SlowCustomTool),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = trusted_custom_capabilities();
    capabilities.grant_tool(tool_id.clone(), ToolGrant::allow_custom_read_only());

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("slow-timeout".to_string()),
            index: 0,
            name: tool_id.clone(),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::Timeout
    );
    assert_eq!(results[0].provider_id, ToolProviderId::in_process());
    let metrics = engine.snapshot_tool_metrics();
    let slow_metrics = &metrics[&ToolMetricKey::in_process(&tool_id)];
    assert_eq!(slow_metrics.errors, 1);
    assert_eq!(slow_metrics.timeouts, 1);
    assert_eq!(slow_metrics.cancellations, 0);
    assert_eq!(slow_metrics.input_bytes, "{}".len());
    assert!(slow_metrics.latency_ms >= slow_metrics.max_latency_ms);
    assert!(slow_metrics.max_latency_ms > 0);
    let health = engine.snapshot_provider_health();
    let in_process_health = health
        .iter()
        .find(|snapshot| snapshot.provider_id == ToolProviderId::in_process())
        .unwrap();
    assert_eq!(in_process_health.state, ToolProviderHealthState::Degraded);
    assert_eq!(in_process_health.errors, 1);
    assert_eq!(in_process_health.timeouts, 1);
    assert_eq!(in_process_health.consecutive_errors, 1);
}

#[test]
fn concurrent_jsonrpc_tool_provider_executes_external_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_jsonrpc_check").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_test_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External JSON-RPC check.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::custom_read_only(),
                provider_resources: Vec::new(),
            },
            Arc::new(EchoJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = trusted_custom_capabilities();
    capabilities.grant_tool(tool_id.clone(), ToolGrant::allow_custom_read_only());

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-call".to_string()),
            index: 0,
            name: tool_id.clone(),
            raw_arguments: r#"{"value":"ok"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].provider_id, provider_id);
    assert_eq!(results[0].data.as_ref().unwrap()["value"], "ok");
    assert_eq!(
        results[0].data.as_ref().unwrap()["secret"],
        serde_json::Value::String("[REDACTED]".to_string())
    );
    let artifact_text = engine
        .artifacts
        .list()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(artifact_text.contains("[REDACTED]"));
    assert!(!artifact_text.contains("AKIA1234567890ABCDEF"));
    let raw_artifact_text = engine
        .artifacts
        .list_raw()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(raw_artifact_text.contains("AKIA1234567890ABCDEF"));
    let metrics = engine.snapshot_tool_metrics();
    let external_metrics = &metrics[&ToolMetricKey::new(&provider_id, &tool_id)];
    assert_eq!(external_metrics.calls, 1);
    assert_eq!(external_metrics.successes, 1);
    assert_eq!(external_metrics.artifacts, 1);
    let health = engine.snapshot_provider_health();
    let external_health = health
        .iter()
        .find(|snapshot| snapshot.provider_id == provider_id)
        .unwrap();
    assert_eq!(external_health.state, ToolProviderHealthState::Healthy);
    assert_eq!(external_health.calls, 1);
}

#[test]
fn concurrent_jsonrpc_provider_artifact_write_requires_capability_policy() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_artifact_smuggler").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_policy_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool that returns an undeclared artifact.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: Vec::new(),
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: None,
                    artifact: Some(CustomToolArtifact {
                        key: ArtifactKey("smuggled_external_artifact".to_string()),
                        content: "external artifact".to_string(),
                    }),
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.artifact_access.write = false;
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-artifact-write-denied".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::ToolNotAllowed
    );
    assert!(engine.artifacts.list().is_empty());
}

#[test]
fn concurrent_jsonrpc_provider_network_read_requires_runtime_authority_policy() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_network_reader").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_network_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    let network_effects = ToolEffects {
        network_read: true,
        ..ToolEffects::default()
    };
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool that requires network read.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: network_effects,
                provider_resources: Vec::new(),
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id,
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: Some(serde_json::json!({"ok": true})),
                    artifact: None,
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: network_effects,
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-network-denied".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    let error = results[0].error.as_ref().unwrap();
    assert_eq!(error.code, ToolErrorCode::ToolNotAllowed);
    assert!(error.message.contains("network read"));
}

#[test]
fn concurrent_jsonrpc_provider_requires_runtime_provider_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_provider_scoped").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_scoped_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool behind provider scope.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: Vec::new(),
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id,
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: Some(serde_json::json!({"ok": true})),
                    artifact: None,
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.runtime_authority = capabilities
        .runtime_authority
        .scoped_to_providers(vec![ToolProviderId::parse("other_provider").unwrap()]);
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-provider-denied".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    let error = results[0].error.as_ref().unwrap();
    assert_eq!(error.code, ToolErrorCode::ToolNotAllowed);
    assert!(error.message.contains("provider"));
}

#[test]
fn concurrent_jsonrpc_provider_resource_requires_runtime_resource_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_resource_scoped").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_resource_provider").unwrap();
    let resource_id = ProviderResourceId::parse("github/org-a/repo-a").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool behind provider resource scope.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: vec![resource_id],
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: Some(serde_json::json!({"ok": true})),
                    artifact: None,
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.runtime_authority =
        capabilities
            .runtime_authority
            .scoped_to_provider_resources(vec![ProviderResourceScope::new(
                provider_id,
                ProviderResourceId::parse("github/org-b/repo-b").unwrap(),
            )]);
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-resource-denied".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    let error = results[0].error.as_ref().unwrap();
    assert_eq!(error.code, ToolErrorCode::ToolNotAllowed);
    assert!(error.message.contains("provider resource"));
}

#[test]
fn concurrent_jsonrpc_provider_resource_scope_is_sent_when_allowed() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("external_resource_allowed").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_resource_allowed_provider").unwrap();
    let resource_id = ProviderResourceId::parse("github/org-a/repo-a").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool behind matching provider resource scope.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: vec![resource_id.clone()],
            },
            Arc::new(ResourceCheckingJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                expected_provider_resources: vec![resource_id.clone()],
                response: JsonRpcToolResponse {
                    data: Some(serde_json::json!({"ok": true})),
                    artifact: None,
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.runtime_authority =
        capabilities
            .runtime_authority
            .scoped_to_provider_resources(vec![ProviderResourceScope::new(
                provider_id.clone(),
                resource_id,
            )]);
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-resource-allowed".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].provider_id, provider_id);
}

#[test]
fn concurrent_jsonrpc_provider_rejects_oversized_artifacts_before_storage() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    limits.max_tool_artifact_bytes = 4;
    let tool_id = ToolId::parse("external_large_artifact").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_large_artifact_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool that returns an oversized artifact.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: Vec::new(),
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: None,
                    artifact: Some(CustomToolArtifact {
                        key: ArtifactKey("large_external_artifact".to_string()),
                        content: "too large".to_string(),
                    }),
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-artifact-too-large".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::TooLarge
    );
    assert!(engine.artifacts.list().is_empty());
}

#[test]
fn concurrent_jsonrpc_provider_rejects_oversized_output_data() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    limits.max_tool_output_bytes = 16;
    let tool_id = ToolId::parse("external_large_output").unwrap();
    let provider_id = ToolProviderId::parse("jsonrpc_large_output_provider").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_jsonrpc_tool(
            provider_id.clone(),
            tool_id.clone(),
            "External tool that returns oversized data.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::default(),
                provider_resources: Vec::new(),
            },
            Arc::new(StaticJsonRpcTransport {
                provider_id: provider_id.clone(),
                tool_id: tool_id.clone(),
                calls: Arc::clone(&calls),
                response: JsonRpcToolResponse {
                    data: Some(serde_json::json!({
                        "payload": "this provider output is much too large"
                    })),
                    artifact: None,
                    limits: LimitInfo::default(),
                },
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: ToolEffects::default(),
        },
    );

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("external-output-too-large".to_string()),
            index: 0,
            name: tool_id,
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::TooLarge
    );
    assert_eq!(results[0].limits.output_bytes, 0);
}

#[test]
fn concurrent_tool_provider_concurrency_is_bounded_per_provider() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let mut limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    limits.max_tool_provider_concurrency_per_provider = 1;
    limits.max_tool_provider_ms = 500;
    let tool_id = ToolId::parse("counted_custom_check").unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_custom(
            tool_id.clone(),
            "Counting custom check.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "additionalProperties": true
            }),
            false,
            Arc::new(CountingSlowCustomTool {
                active: Arc::clone(&active),
                max_seen: Arc::clone(&max_seen),
            }),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = trusted_custom_capabilities();
    capabilities.grant_tool(tool_id.clone(), ToolGrant::allow_custom_read_only());

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![
            ModelToolCall {
                call_id: ToolCallId("counted-a".to_string()),
                index: 0,
                name: tool_id.clone(),
                raw_arguments: r#"{"value":"a"}"#.to_string(),
            },
            ModelToolCall {
                call_id: ToolCallId("counted-b".to_string()),
                index: 1,
                name: tool_id.clone(),
                raw_arguments: r#"{"value":"b"}"#.to_string(),
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result.ok));
    assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    let metrics = engine.snapshot_tool_metrics();
    let counted_metrics = &metrics[&ToolMetricKey::in_process(&tool_id)];
    assert_eq!(counted_metrics.calls, 2);
    assert_eq!(counted_metrics.successes, 2);
    assert_eq!(
        counted_metrics.input_bytes,
        r#"{"value":"a"}"#.len() + r#"{"value":"b"}"#.len()
    );
    assert!(counted_metrics.latency_ms >= counted_metrics.max_latency_ms);
    assert!(counted_metrics.max_latency_ms > 0);
    assert!(counted_metrics.queue_wait_ms >= counted_metrics.max_queue_wait_ms);
    assert!(counted_metrics.max_queue_wait_ms > 0);
}

#[test]
fn concurrent_in_process_tool_provider_panic_is_contained() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = RuntimeLimits::standard(1, 64 * 1024, 10);
    let tool_id = ToolId::parse("panic_custom_check").unwrap();
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_custom(
            tool_id.clone(),
            "Panic custom check.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            false,
            Arc::new(PanicCustomTool),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, Arc::new(limits), Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut capabilities = trusted_custom_capabilities();
    capabilities.grant_tool(tool_id.clone(), ToolGrant::allow_custom_read_only());

    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("panic-call".to_string()),
            index: 0,
            name: tool_id.clone(),
            raw_arguments: "{}".to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert_eq!(results[0].provider_id, ToolProviderId::in_process());
    assert_eq!(
        results[0].error.as_ref().unwrap().code,
        ToolErrorCode::Internal
    );
    let metrics = engine.snapshot_tool_metrics();
    let panic_metrics = &metrics[&ToolMetricKey::in_process(&tool_id)];
    assert_eq!(panic_metrics.errors, 1);
}

#[test]
fn concurrent_registry_executes_allowed_custom_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64, 10);
    let snapshot = RepoSnapshot::build(temp.path(), &policy, &change).unwrap();
    let limits = Arc::new(RuntimeLimits::standard(1, 64 * 1024, 10));
    let tool_id = ToolId::parse("host_custom_check").unwrap();
    let mut registry = ToolRegistry::review_defaults().unwrap();
    registry
        .register_custom(
            tool_id.clone(),
            "Host engine supplied custom reviewer check.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            false,
            Arc::new(EchoCustomTool),
        )
        .unwrap();
    let engine = ToolEngine::with_registry(snapshot, limits, Arc::new(registry)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let denied = runtime.block_on(engine.execute_batch(
        test_scope("session"),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("denied-custom".to_string()),
            index: 0,
            name: tool_id.clone(),
            raw_arguments: r#"{"value":"ok"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(denied.len(), 1);
    assert!(!denied[0].ok);
    assert_eq!(
        denied[0].error.as_ref().unwrap().code,
        ToolErrorCode::ToolNotAllowed
    );

    let mut allowed_capabilities = trusted_custom_capabilities();
    allowed_capabilities.grant_tool(tool_id.clone(), ToolGrant::allow_custom_read_only());
    let results = runtime.block_on(engine.execute_batch(
        test_scope_with_capabilities("session", allowed_capabilities),
        TurnId(0),
        vec![ModelToolCall {
            call_id: ToolCallId("custom".to_string()),
            index: 0,
            name: tool_id.clone(),
            raw_arguments: r#"{"value":"ok"}"#.to_string(),
        }],
        tokio_util::sync::CancellationToken::new(),
    ));
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].tool_name, tool_id);
    assert!(results[0].artifact_id.is_some());
    let data = results[0].data.as_ref().unwrap().to_string();
    assert!(data.contains("[REDACTED]"));
    assert!(!data.contains("AKIA1234567890ABCDEF"));
    let metrics = engine.snapshot_tool_metrics();
    let custom_key = ToolMetricKey::in_process(&tool_id);
    assert_eq!(custom_key.provider_id(), Some(ToolProviderId::in_process()));
    let custom_metrics = &metrics[&custom_key];
    assert_eq!(custom_metrics.calls, 2);
    assert_eq!(custom_metrics.successes, 1);
    assert_eq!(custom_metrics.errors, 1);
}

#[test]
fn concurrent_tool_registry_rejects_alias_collisions() {
    let mut registry = ToolRegistry::review_defaults().unwrap();
    let alias = ToolId::parse("provider_visible_name").unwrap();
    let first = ToolId::parse("first_custom_alias_check").unwrap();
    registry
        .register_custom_with_alias_and_effects(
            first.clone(),
            alias.clone(),
            "First custom check.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CustomToolOptions {
                cacheable: false,
                effects: ToolEffects::custom_read_only(),
                provider_resources: Vec::new(),
            },
            Arc::new(EchoCustomTool),
        )
        .unwrap();

    let table = registry.alias_table().unwrap();
    assert_eq!(table.alias_for(&first), Some(&alias));
    assert_eq!(table.tool_for_alias(&alias), Some(&first));

    let duplicate = registry.register_custom_with_alias_and_effects(
        ToolId::parse("second_custom_alias_check").unwrap(),
        alias,
        "Second custom check.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        CustomToolOptions {
            cacheable: false,
            effects: ToolEffects::custom_read_only(),
            provider_resources: Vec::new(),
        },
        Arc::new(EchoCustomTool),
    );
    assert!(duplicate.is_err());
}
