use super::prelude::*;
use super::support::*;

#[test]
fn public_reviewer_facade_runs_mock_review() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-1",
            "head-1",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run through the public reviewer facade.",
        public_budget(),
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    fs::write(temp.path().join("README.md"), "mutated needle\n").unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.run_id, "public-run");
    assert_eq!(report.summary.status, "completed");
    assert_eq!(report.summary.snapshot_count, 1);
    let manifests = report.snapshot_manifests();
    assert_eq!(manifests.len(), 1);
    let manifest = &manifests[0];
    assert_eq!(manifest.file_count, 1);
    assert_eq!(manifest.changed_file_count, 1);
    assert_eq!(manifest.captured_text_file_count, 1);
    assert!(manifest.captured_text_bytes > 0);
    assert!(!manifest.manifest_hash.is_empty());
    assert!(!manifest.path_policy_hash.is_empty());
    assert_eq!(manifest.changed_files.len(), 1);
    assert_eq!(manifest.changed_files[0].path.display(), "README.md");
    let readme = manifest
        .files
        .iter()
        .find(|file| file.path.display() == "README.md")
        .expect("README.md in public snapshot manifest");
    assert!(readme.captured);
    assert!(readme.is_changed);
    assert!(readme.is_text_candidate);
    assert!(readme.content_hash.is_some());
    let reader = report
        .snapshot_reader(&report.snapshot.snapshot_id)
        .expect("snapshot reader");
    let snapshot_text = reader.read_text_path("README.md", 64 * 1024).unwrap();
    assert_eq!(snapshot_text.path.display(), "README.md");
    assert!(snapshot_text.content.contains("needle"));
    assert!(!snapshot_text.content.contains("mutated"));
    assert_eq!(
        snapshot_text.content_hash,
        readme.content_hash.clone().unwrap()
    );
    assert_eq!(report.summary.completed_sessions, 2);
    assert_eq!(report.summary.findings, 1);
    let findings = report.findings();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].publishable);
    assert_eq!(
        findings[0]
            .location
            .as_ref()
            .map(|location| location.path.as_str()),
        Some("README.md")
    );
    let redacted_artifacts = report.artifacts.list();
    assert!(!redacted_artifacts.is_empty());
    assert!(report.summary.artifacts >= redacted_artifacts.len());
    let raw_artifacts = report.artifacts.list_raw();
    assert_eq!(raw_artifacts.len(), redacted_artifacts.len());
    assert!(raw_artifacts
        .iter()
        .all(|artifact| !artifact.content.is_empty()));
    for evidence in &findings[0].evidence {
        let artifact_id = evidence.artifact_id.clone();
        let artifact = report
            .artifacts
            .get(&artifact_id)
            .expect("finding evidence artifact");
        assert_eq!(artifact.content_hash, evidence.content_hash);
        assert!(!artifact.content.is_empty());
    }
    let event_records = events.records();
    assert!(!event_records.is_empty());
    for (index, record) in event_records.iter().enumerate() {
        assert_eq!(record.seq, index as u64 + 1);
        assert!(!record.timestamp_utc.is_empty());
        assert_eq!(record.run_id.as_deref(), Some("public-run"));
    }
    let review_event_log_dir = tempfile::tempdir().unwrap();
    let review_event_log = crate::reviewer_kernel::events::export_review_event_records_jsonl(
        review_event_log_dir.path().join("review-events.jsonl"),
        &event_records,
    )
    .unwrap();
    assert_eq!(
        review_event_log.schema_version,
        crate::reviewer_kernel::events::REVIEW_EVENT_LOG_SCHEMA_VERSION
    );
    assert_eq!(review_event_log.record_count, event_records.len());
    assert!(review_event_log.bytes > 0);
    let first_review_event_line = fs::read_to_string(&review_event_log.path)
        .unwrap()
        .lines()
        .next()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .unwrap();
    assert_eq!(
        first_review_event_line["schemaVersion"].as_str(),
        Some(crate::reviewer_kernel::events::REVIEW_EVENT_LOG_SCHEMA_VERSION)
    );
    assert_eq!(
        first_review_event_line["runId"].as_str(),
        Some("public-run")
    );
    let loaded_review_event_log =
        crate::reviewer_kernel::events::load_review_event_records_jsonl(&review_event_log.path)
            .unwrap();
    assert_eq!(loaded_review_event_log.path, review_event_log.path);
    assert_eq!(
        loaded_review_event_log.schema_version,
        crate::reviewer_kernel::events::REVIEW_EVENT_LOG_SCHEMA_VERSION
    );
    assert_eq!(loaded_review_event_log.record_count, event_records.len());
    assert_eq!(loaded_review_event_log.records, event_records);
    assert_eq!(
        event_records[0].snapshot_id,
        Some(report.snapshot.snapshot_id.clone())
    );
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallCompleted {
            ok: true,
            error_code: None,
            ..
        }
    ) && record.snapshot_id
        == Some(report.snapshot.snapshot_id.clone())
        && record
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
        && record.turn.is_some()
        && record.tool_call_id.is_some()));
    let event_types = events.events();
    assert!(matches!(
        event_types.first(),
        Some(crate::reviewer_kernel::events::ReviewEvent::RunStarted { .. })
    ));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer_kernel::events::ReviewEvent::SessionStarted { session_id }
            if session_id.starts_with("review-orchestrator")
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer_kernel::events::ReviewEvent::ModelStarted { session_id, .. }
            if session_id.starts_with("review-orchestrator")
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer_kernel::events::ReviewEvent::ToolBatchStarted { count, .. } if *count > 0
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallCompleted {
            tool_id,
            ok: true,
            ..
        } if tool_id == "read_diff"
    )));
    let artifact_event_ids = event_types
        .iter()
        .filter_map(|event| match event {
            crate::reviewer_kernel::events::ReviewEvent::ArtifactCreated {
                artifact_id, ..
            } => Some(artifact_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!artifact_event_ids.is_empty());
    for artifact_id in artifact_event_ids {
        assert!(report.artifacts.get(artifact_id).is_some());
    }
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer_kernel::events::ReviewEvent::FindingRecorded { finding_id, .. }
            if !finding_id.is_empty()
    )));
    assert!(matches!(
        event_types.last(),
        Some(crate::reviewer_kernel::events::ReviewEvent::RunFinished { .. })
    ));
}

#[test]
fn public_reviewer_facade_emits_tool_denial_events() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-denied",
            "head-denied",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "denied-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run with read_diff denied.",
        public_budget(),
    )
    .deny_tool(crate::reviewer_kernel::kernel_types::ToolId::from(
        ToolName::ReadDiff,
    ));
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "denied-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _report = tokio.block_on(run.execute());

    let event_records = events.records();
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallDenied {
            tool_id,
            error_code: crate::reviewer_kernel::kernel_types::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if tool_id == "read_diff"
            && reason.contains("not allowed")
            && record.run_id.as_deref() == Some("denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
            && record.turn.is_some()
            && record.tool_call_id.is_some()
    )));
}

#[test]
fn public_reviewer_facade_cancelled_run_emits_review_events() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-cancel",
            "head-public-cancel",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-cancel-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run cancellation through the public reviewer facade.",
        public_budget(),
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-cancel-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let cancel = tokio_util::sync::CancellationToken::new();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(CancellingModel {
            parent_cancel: cancel.clone(),
            calls: Arc::clone(&model_calls),
        }))
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let report = tokio.block_on(run.execute_with_cancel(cancel));

    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.summary.sessions, 1);
    assert_eq!(report.summary.completed_sessions, 0);
    // The aborted call still counts: model_calls reports attempts made, not
    // turns completed.
    assert_eq!(report.summary.model_calls, 1);
    assert_eq!(report.summary.tool_calls, 0);
    let event_records = events.records();
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ModelStarted { session_id, .. }
            if session_id.starts_with("review-orchestrator")
                && record.run_id.as_deref() == Some("public-cancel-run")
                && record
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
                && record.turn.is_some()
    )));
    assert!(!event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ModelCompleted { .. }
    )));
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::SessionFinished { session_id, status }
            if session_id.starts_with("review-orchestrator")
                && (status == "cancelled" || status == "failed")
                && record.run_id.as_deref() == Some("public-cancel-run")
                && record
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
    )));
    assert!(matches!(
        event_records.last().map(|record| &record.event),
        Some(crate::reviewer_kernel::events::ReviewEvent::RunFinished { status }) if status == "partial"
    ));
}

#[test]
fn public_reviewer_facade_runs_multiple_snapshots() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    fs::write(first.path().join("README.md"), "needle in first\n").unwrap();
    fs::write(second.path().join("README.md"), "needle in second\n").unwrap();
    let first_id = crate::reviewer_kernel::kernel_types::SnapshotId("snapshot-first".to_string());
    let second_id = crate::reviewer_kernel::kernel_types::SnapshotId("snapshot-second".to_string());
    let first_snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        first.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-first",
            "head-first",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_snapshot_id(first_id.clone())
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let second_snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        second.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-second",
            "head-second",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_snapshot_id(second_id.clone())
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let sessions = vec![
        crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
            "first-session",
            crate::reviewer_kernel::review_contract::Role::Generalist,
            "Review first snapshot.",
            public_budget(),
        )
        .with_snapshot_id(first_id.clone()),
        crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
            "second-session",
            crate::reviewer_kernel::review_contract::Role::Generalist,
            "Review second snapshot.",
            public_budget(),
        )
        .with_snapshot_id(second_id.clone()),
    ];
    let spec = crate::reviewer_kernel::spec::RunSpec {
        run_id: "multi-snapshot-run".to_string(),
        snapshots: vec![first_snapshot, second_snapshot],
        sessions,
        limits: crate::reviewer_kernel::spec::ReviewRunLimits::standard(2, 64 * 1024, 20),
    };
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.snapshots.len(), 2);
    assert_eq!(report.summary.sessions, 4);
    assert_eq!(report.summary.completed_sessions, 4);
    assert_eq!(report.summary.findings, 2);
    assert_eq!(report.summary.snapshot_count, 2);
    assert_eq!(report.metrics.snapshot_metrics.len(), 2);
    assert!(report
        .metrics
        .snapshot_metrics
        .iter()
        .any(|metrics| metrics.snapshot_id == first_id && metrics.sessions == 2));
    assert!(report
        .metrics
        .snapshot_metrics
        .iter()
        .any(|metrics| metrics.snapshot_id == second_id && metrics.sessions == 2));
    let artifact_text = report
        .artifacts
        .list()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(artifact_text.contains("needle in first"));
    assert!(artifact_text.contains("needle in second"));
    let event_records = events.records();
    assert!(event_records
        .iter()
        .all(|record| record.run_id.as_deref() == Some("multi-snapshot-run")));
    let events = events.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                crate::reviewer_kernel::events::ReviewEvent::SnapshotStarted { .. }
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                crate::reviewer_kernel::events::ReviewEvent::SnapshotFinished { .. }
            ))
            .count(),
        2
    );
}

#[test]
fn public_reviewer_facade_runs_custom_tool_and_exports_metrics() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let custom_tool_id = registry
        .register_read_only_tool(
            "host_custom_check",
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
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-1",
            "head-1",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "custom-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run host custom check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool(custom_tool_id.clone());
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-custom-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(custom_tool_id.clone())))
        .review_tool_registry(registry)
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    let custom_metrics = &report.metrics.tool_metrics[&ToolMetricKey::in_process(&custom_tool_id)];
    assert_eq!(custom_metrics.calls, 1);
    assert_eq!(custom_metrics.successes, 1);
    assert_eq!(custom_metrics.artifacts, 1);
    assert!(custom_metrics.input_bytes > 0);
    assert!(custom_metrics.output_bytes > 0);
    assert!(custom_metrics.latency_ms >= custom_metrics.max_latency_ms);
    assert!(custom_metrics.max_latency_ms > 0);
    let in_process_health = report
        .metrics
        .provider_health
        .iter()
        .find(|health| health.provider_id == ToolProviderId::in_process())
        .unwrap();
    assert_eq!(
        in_process_health.state,
        crate::reviewer_kernel::kernel_types::ToolProviderHealthState::Healthy
    );
    assert_eq!(in_process_health.calls, 1);
    assert_eq!(in_process_health.errors, 0);
    let artifact_text = report
        .artifacts
        .list()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(artifact_text.contains("[REDACTED]"));
    assert!(!artifact_text.contains("AKIA1234567890ABCDEF"));
    let raw_artifact_text = report
        .artifacts
        .list_raw()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(raw_artifact_text.contains("AKIA1234567890ABCDEF"));
}

#[test]
fn public_reviewer_facade_passes_provider_resources_to_scoped_host_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let resource_id = ProviderResourceId::parse("github/org-a/repo-a").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let custom_tool_id = registry
        .register_scoped_read_only_tool(
            "host_resource_scoped_check",
            "Host custom check scoped to one external resource.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            false,
            vec![resource_id.clone()],
            Arc::new(ResourceScopedReviewTool {
                expected_provider_resources: vec![resource_id.clone()],
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-host-resource",
            "head-host-resource",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "host-resource-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run host custom check with a provider resource.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool_for_resources(custom_tool_id.clone(), vec![resource_id]);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-host-resource-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(custom_tool_id)))
        .review_tool_registry(registry)
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn public_reviewer_facade_denies_host_tool_outside_provider_resource_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let allowed_resource = ProviderResourceId::parse("github/org-a/repo-a").unwrap();
    let denied_resource = ProviderResourceId::parse("github/org-b/repo-b").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let custom_tool_id = registry
        .register_scoped_read_only_tool(
            "host_resource_denied_check",
            "Host custom check scoped to a different external resource.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            false,
            vec![allowed_resource.clone()],
            Arc::new(ResourceScopedReviewTool {
                expected_provider_resources: vec![allowed_resource],
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-host-resource-denied",
            "head-host-resource-denied",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "host-resource-denied-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run host custom check outside provider resource scope.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool_for_resources(custom_tool_id.clone(), vec![denied_resource]);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-host-resource-denied-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(custom_tool_id)))
        .review_tool_registry(registry)
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _report = tokio.block_on(run.execute());

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(events.records().iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer_kernel::kernel_types::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("provider resource")
            && record.run_id.as_deref() == Some("public-host-resource-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
    )));
}

#[test]
fn public_reviewer_facade_runs_scoped_jsonrpc_provider_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id =
        crate::reviewer_kernel::kernel_types::ToolProviderId::parse("public_jsonrpc_provider")
            .unwrap();
    let resource_id =
        crate::reviewer_kernel::kernel_types::ProviderResourceId::parse("github/org-a/repo-a")
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer_kernel::review_tools::ReviewJsonRpcReadOnlyToolRegistration {
                provider_id: provider_id.clone(),
                id: "public_jsonrpc_check".to_string(),
                description: "External JSON-RPC check scoped to one provider resource.".to_string(),
                parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
                }),
                cacheable: false,
                provider_resources: vec![resource_id.clone()],
                transport: Arc::new(PublicJsonRpcReviewTool {
                    provider_id: provider_id.clone(),
                    tool_id: "public_jsonrpc_check".to_string(),
                    expected_provider_resources: vec![resource_id.clone()],
                    calls: Arc::clone(&calls),
                }),
            },
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-jsonrpc",
            "head-public-jsonrpc",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run public JSON-RPC provider check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id],
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-jsonrpc-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(tool_id.clone())))
        .review_tool_registry(registry)
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let metric_key =
        crate::reviewer_kernel::kernel_types::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.successes, 1);
    let provider_health = report
        .metrics
        .provider_health
        .iter()
        .find(|health| health.provider_id == provider_id)
        .unwrap();
    assert_eq!(
        provider_health.state,
        crate::reviewer_kernel::kernel_types::ToolProviderHealthState::Healthy
    );
}

#[test]
fn public_reviewer_facade_runs_http_jsonrpc_provider_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id =
        crate::reviewer_kernel::kernel_types::ToolProviderId::parse("public_http_jsonrpc_provider")
            .unwrap();
    let resource_id =
        crate::reviewer_kernel::kernel_types::ProviderResourceId::parse("github/org-http/repo")
            .unwrap();
    let server = LoopbackJsonRpcToolServer::spawn();
    let transport =
        crate::reviewer_kernel::tool_engine::HttpJsonRpcToolTransport::new(server.endpoint())
            .unwrap();
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer_kernel::review_tools::ReviewJsonRpcReadOnlyToolRegistration {
                provider_id: provider_id.clone(),
                id: "public_http_jsonrpc_check".to_string(),
                description: "External HTTP JSON-RPC check scoped to one provider resource."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "value": { "type": "string" }
                    },
                    "required": ["value"],
                    "additionalProperties": false
                }),
                cacheable: false,
                provider_resources: vec![resource_id.clone()],
                transport: Arc::new(transport),
            },
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-http-jsonrpc",
            "head-public-http-jsonrpc",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-http-jsonrpc-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run public HTTP JSON-RPC provider check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id.clone()],
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-http-jsonrpc-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(tool_id.clone())))
        .review_tool_registry(registry)
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());
    let wire_request = server.join();

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(wire_request["jsonrpc"], "2.0");
    assert_eq!(wire_request["method"], "tool.call");
    assert_eq!(wire_request["params"]["providerId"], provider_id.as_str());
    assert_eq!(wire_request["params"]["toolId"], tool_id.as_str());
    assert_eq!(
        wire_request["params"]["providerResources"],
        serde_json::json!([resource_id.as_str()])
    );
    assert_eq!(wire_request["params"]["arguments"]["value"], "ok");
    let metric_key =
        crate::reviewer_kernel::kernel_types::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.successes, 1);
}

#[test]
fn public_reviewer_facade_runs_jsonrpc_network_read_tool_with_authority() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer_kernel::kernel_types::ToolProviderId::parse(
        "public_jsonrpc_network_provider",
    )
    .unwrap();
    let resource_id =
        crate::reviewer_kernel::kernel_types::ProviderResourceId::parse("github/org-network/repo")
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_network_read_tool(
            crate::reviewer_kernel::review_tools::ReviewJsonRpcNetworkReadToolRegistration {
                provider_id: provider_id.clone(),
                id: "public_jsonrpc_network_check".to_string(),
                description: "External JSON-RPC check that needs network read.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "value": { "type": "string" }
                    },
                    "required": ["value"],
                    "additionalProperties": false
                }),
                cacheable: false,
                provider_resources: vec![resource_id.clone()],
                transport: Arc::new(PublicJsonRpcReviewTool {
                    provider_id: provider_id.clone(),
                    tool_id: "public_jsonrpc_network_check".to_string(),
                    expected_provider_resources: vec![resource_id.clone()],
                    calls: Arc::clone(&calls),
                }),
            },
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-network",
            "head-public-jsonrpc-network",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-network-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run public JSON-RPC provider network check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_network_read_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id],
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-jsonrpc-network-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(tool_id.clone())))
        .review_tool_registry(registry)
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let metric_key =
        crate::reviewer_kernel::kernel_types::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.successes, 1);
}

#[test]
fn public_reviewer_facade_denies_jsonrpc_network_read_without_authority() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer_kernel::kernel_types::ToolProviderId::parse(
        "public_jsonrpc_network_denied",
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_jsonrpc_network_read_tool(
            provider_id.clone(),
            "public_jsonrpc_network_denied_check",
            "External JSON-RPC check that needs denied network read.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
            false,
            Arc::new(PublicJsonRpcReviewTool {
                provider_id: provider_id.clone(),
                tool_id: "public_jsonrpc_network_denied_check".to_string(),
                expected_provider_resources: Vec::new(),
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();
    let mut capabilities = crate::reviewer_kernel::kernel_types::CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        crate::reviewer_kernel::kernel_types::ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: crate::reviewer_kernel::kernel_types::ToolEffects {
                network_read: true,
                ..crate::reviewer_kernel::kernel_types::ToolEffects::review_read_only()
            },
        },
    );
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-network-denied",
            "head-public-jsonrpc-network-denied",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-network-denied-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run public JSON-RPC provider network check without network authority.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .with_capabilities(capabilities);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-jsonrpc-network-denied-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(tool_id.clone())))
        .review_tool_registry(registry)
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(events.records().iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer_kernel::kernel_types::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("network read")
            && record.run_id.as_deref() == Some("public-jsonrpc-network-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
    )));
    let metric_key =
        crate::reviewer_kernel::kernel_types::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.errors, 1);
}

#[test]
fn public_reviewer_facade_denies_jsonrpc_provider_resource_outside_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer_kernel::kernel_types::ToolProviderId::parse(
        "public_jsonrpc_denied_provider",
    )
    .unwrap();
    let allowed_resource =
        crate::reviewer_kernel::kernel_types::ProviderResourceId::parse("github/org-a/repo-a")
            .unwrap();
    let denied_resource =
        crate::reviewer_kernel::kernel_types::ProviderResourceId::parse("github/org-b/repo-b")
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry =
        crate::reviewer_kernel::review_tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer_kernel::review_tools::ReviewJsonRpcReadOnlyToolRegistration {
                provider_id: provider_id.clone(),
                id: "public_jsonrpc_denied_check".to_string(),
                description: "External JSON-RPC check scoped to another provider resource."
                    .to_string(),
                parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "required": ["value"],
                "additionalProperties": false
                }),
                cacheable: false,
                provider_resources: vec![allowed_resource.clone()],
                transport: Arc::new(PublicJsonRpcReviewTool {
                    provider_id: provider_id.clone(),
                    tool_id: "public_jsonrpc_denied_check".to_string(),
                    expected_provider_resources: vec![allowed_resource],
                    calls: Arc::clone(&calls),
                }),
            },
        )
        .unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-denied",
            "head-public-jsonrpc-denied",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-denied-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run public JSON-RPC provider check outside resource scope.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![denied_resource],
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "public-jsonrpc-denied-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer_kernel::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicCustomToolModel(tool_id.clone())))
        .review_tool_registry(registry)
        .review_event_sink(events.clone())
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());

    assert_eq!(report.summary.completed_sessions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(events.records().iter().any(|record| matches!(
        &record.event,
        crate::reviewer_kernel::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer_kernel::kernel_types::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("provider resource")
            && record.run_id.as_deref() == Some("public-jsonrpc-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("review-orchestrator"))
    )));
    let metric_key =
        crate::reviewer_kernel::kernel_types::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.errors, 1);
}

#[test]
fn public_bounded_event_sink_drops_after_capacity() {
    let sink = crate::reviewer_kernel::runtime_events::BoundedInMemoryEventSink::new(1);
    crate::reviewer_kernel::runtime_events::EventSink::emit(
        &sink,
        crate::reviewer_kernel::runtime_events::RuntimeEvent::JobStarted {
            snapshot_id: crate::reviewer_kernel::kernel_types::SnapshotId("snapshot".to_string()),
        },
    );
    crate::reviewer_kernel::runtime_events::EventSink::emit(
        &sink,
        crate::reviewer_kernel::runtime_events::RuntimeEvent::JobFinished {
            status: "done".to_string(),
        },
    );

    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, 1);
    assert_eq!(
        records[0].context.snapshot_id,
        Some(crate::reviewer_kernel::kernel_types::SnapshotId(
            "snapshot".to_string()
        ))
    );
    assert_eq!(sink.dropped_count(), 1);
    let event_log_dir = tempfile::tempdir().unwrap();
    let event_log = sink
        .export_jsonl(event_log_dir.path().join("bounded-events.jsonl"))
        .unwrap();
    assert_eq!(event_log.record_count, 1);
    assert_eq!(event_log.dropped_count, 1);
    assert_eq!(
        fs::read_to_string(&event_log.path).unwrap().lines().count(),
        1
    );

    let oldest = crate::reviewer_kernel::runtime_events::BoundedInMemoryEventSink::with_policy(
        1,
        crate::reviewer_kernel::runtime_events::EventBackpressurePolicy::DropOldest,
    );
    crate::reviewer_kernel::runtime_events::EventSink::emit(
        &oldest,
        crate::reviewer_kernel::runtime_events::RuntimeEvent::JobStarted {
            snapshot_id: crate::reviewer_kernel::kernel_types::SnapshotId("snapshot".to_string()),
        },
    );
    crate::reviewer_kernel::runtime_events::EventSink::emit(
        &oldest,
        crate::reviewer_kernel::runtime_events::RuntimeEvent::JobFinished {
            status: "done".to_string(),
        },
    );
    let records = oldest.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, 2);
    assert!(matches!(
        records[0].event,
        crate::reviewer_kernel::runtime_events::RuntimeEvent::JobFinished { .. }
    ));
    assert_eq!(oldest.dropped_count(), 1);
}
