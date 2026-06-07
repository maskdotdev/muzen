use super::prelude::*;
use super::support::*;

#[test]
fn public_reviewer_facade_runs_mock_review() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-1",
            "head-1",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    )
    .with_path_policy(crate::reviewer::snapshots::SnapshotPathPolicy::standard(
        64 * 1024,
        20,
    ));
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-session",
        crate::contracts::Role::Generalist,
        "Run through the public reviewer facade.",
        public_budget(),
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
    assert_eq!(report.summary.completed_sessions, 1);
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
    let redacted_artifact_policy = crate::reviewer::artifacts::ArtifactExportPolicy::redacted_all();
    let mut raw_artifact_capabilities =
        crate::reviewer::adapters::capabilities::CapabilitySet::review_read_only();
    raw_artifact_capabilities.artifact_access.read_raw = true;
    let raw_artifact_policy =
        crate::reviewer::artifacts::ArtifactExportPolicy::raw(&raw_artifact_capabilities).unwrap();
    let evidence_artifacts = report
        .finding_evidence_artifacts(&findings[0].id, redacted_artifact_policy.clone())
        .unwrap();
    assert_eq!(evidence_artifacts.len(), findings[0].evidence_count);
    for evidence_artifact in &evidence_artifacts {
        assert!(!evidence_artifact.evidence.evidence_id.is_empty());
        assert_eq!(
            evidence_artifact.evidence.artifact_id(),
            evidence_artifact.artifact.artifact_id()
        );
        assert_eq!(
            evidence_artifact.evidence.content_hash,
            evidence_artifact.artifact.content_hash
        );
        assert!(!evidence_artifact.artifact.content.is_empty());
    }
    if let Some(evidence_artifact) = evidence_artifacts.first() {
        let scoped_artifact_id = evidence_artifact.artifact_id().to_string();
        let scoped_artifact_policy =
            crate::reviewer::artifacts::ArtifactExportPolicy::redacted_artifacts([
                scoped_artifact_id.as_str(),
            ]);
        let scoped_evidence_artifacts = report
            .finding_evidence_artifacts(&findings[0].id, scoped_artifact_policy.clone())
            .unwrap();
        assert_eq!(scoped_evidence_artifacts.len(), 1);
        assert_eq!(
            scoped_evidence_artifacts[0].artifact.artifact_id(),
            scoped_artifact_id
        );
        let scoped_export = report
            .export_artifacts(scoped_artifact_policy.clone())
            .unwrap();
        assert_eq!(scoped_export.artifact_count, 1);
        assert_eq!(
            scoped_export.first_artifact_id(),
            Some(scoped_artifact_id.as_str())
        );
        let retained_scoped_artifact_policy = scoped_artifact_policy.clone().with_retention_policy(
            crate::reviewer::artifacts::ArtifactRetentionPolicy::max_artifacts(1),
        );
        let retained_scoped_export = report
            .export_artifacts(retained_scoped_artifact_policy.clone())
            .unwrap();
        assert_eq!(retained_scoped_export.artifact_count, 1);
        assert_eq!(
            retained_scoped_export.retention,
            crate::reviewer::artifacts::ArtifactRetentionPolicy::max_artifacts(1)
        );
        let retained_scoped_evidence = report
            .finding_evidence_artifacts(&findings[0].id, retained_scoped_artifact_policy)
            .unwrap();
        assert_eq!(retained_scoped_evidence.len(), 1);
        let memory_artifact_store =
            crate::reviewer::artifacts::InMemoryArtifactObjectStore::default();
        let retained_memory_policy = scoped_artifact_policy.clone().with_retention_policy(
            crate::reviewer::artifacts::ArtifactRetentionPolicy::max_artifacts(1),
        );
        let retained_memory_manifest = report
            .persist_artifacts(&memory_artifact_store, retained_memory_policy)
            .unwrap();
        assert_eq!(retained_memory_manifest.artifact_count, 1);
        assert_eq!(
            retained_memory_manifest.total_bytes,
            scoped_export.total_bytes
        );
        assert_eq!(
            retained_memory_manifest.retention,
            crate::reviewer::artifacts::ArtifactRetentionPolicy::max_artifacts(1)
        );
        assert_eq!(memory_artifact_store.object_count(), 1);
        let memory_object = &retained_memory_manifest.objects[0];
        assert_eq!(
            memory_object.view,
            crate::reviewer::artifacts::ArtifactViewMode::Redacted
        );
        assert_eq!(memory_object.artifact_id(), scoped_artifact_id);
        assert!(memory_object.path.is_none());
        assert_eq!(
            memory_artifact_store
                .read(&memory_object.uri)
                .unwrap()
                .len(),
            memory_object.bytes
        );
        let memory_validation = retained_memory_manifest
            .validate_storage(&memory_artifact_store)
            .unwrap();
        assert!(memory_validation.valid);
        assert_eq!(memory_validation.expected_objects, 1);
        assert_eq!(memory_validation.checked_objects, 1);
        assert_eq!(memory_validation.checked_bytes, memory_object.bytes);
        let memory_cleanup = retained_memory_manifest
            .cleanup_storage(&memory_artifact_store)
            .unwrap();
        assert_eq!(memory_cleanup.expected_objects, 1);
        assert_eq!(memory_cleanup.checked_objects, 1);
        assert_eq!(memory_cleanup.removed_objects.len(), 1);
        assert_eq!(memory_cleanup.removed_bytes, memory_object.bytes);
        assert!(memory_cleanup.missing_objects.is_empty());
        assert!(memory_cleanup.stale_objects.is_empty());
        assert_eq!(memory_artifact_store.object_count(), 0);
        assert!(
            !retained_memory_manifest
                .validate_storage(&memory_artifact_store)
                .unwrap()
                .valid
        );
    }
    assert!(!report.artifacts.list().is_empty());
    let export = report
        .export_artifacts(redacted_artifact_policy.clone())
        .unwrap();
    assert_eq!(
        export.view,
        crate::reviewer::artifacts::ArtifactViewMode::Redacted
    );
    assert_eq!(
        export.retention,
        crate::reviewer::artifacts::ArtifactRetentionPolicy::unlimited()
    );
    assert_eq!(export.artifact_count, report.artifacts.list().len());
    assert!(export.total_bytes > 0);
    let no_artifacts_policy = redacted_artifact_policy.clone().with_retention_policy(
        crate::reviewer::artifacts::ArtifactRetentionPolicy::max_artifacts(0),
    );
    assert!(matches!(
        report.export_artifacts(no_artifacts_policy.clone()),
        Err(
            crate::reviewer::adapters::runtime::RuntimeError::LimitExceeded {
                kind: "artifact_retention_artifacts"
            }
        )
    ));
    if findings[0].evidence_count > 0 {
        assert!(matches!(
            report.finding_evidence_artifacts(&findings[0].id, no_artifacts_policy.clone()),
            Err(
                crate::reviewer::adapters::runtime::RuntimeError::LimitExceeded {
                    kind: "artifact_retention_artifacts"
                }
            )
        ));
    }
    let too_few_bytes_policy = redacted_artifact_policy.clone().with_retention_policy(
        crate::reviewer::artifacts::ArtifactRetentionPolicy::max_bytes(export.total_bytes - 1),
    );
    assert!(matches!(
        report.export_artifacts(too_few_bytes_policy.clone()),
        Err(
            crate::reviewer::adapters::runtime::RuntimeError::LimitExceeded {
                kind: "artifact_retention_bytes"
            }
        )
    ));
    let rejected_memory_store = crate::reviewer::artifacts::InMemoryArtifactObjectStore::default();
    assert!(matches!(
        report.persist_artifacts(&rejected_memory_store, too_few_bytes_policy.clone()),
        Err(
            crate::reviewer::adapters::runtime::RuntimeError::LimitExceeded {
                kind: "artifact_retention_bytes"
            }
        )
    ));
    assert_eq!(rejected_memory_store.object_count(), 0);
    let rejected_bundle_dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        report.export_artifact_bundle(rejected_bundle_dir.path(), too_few_bytes_policy),
        Err(
            crate::reviewer::adapters::runtime::RuntimeError::LimitExceeded {
                kind: "artifact_retention_bytes"
            }
        )
    ));
    assert!(!rejected_bundle_dir.path().join("manifest.json").exists());
    assert!(!rejected_bundle_dir.path().join("artifacts").exists());
    assert!(crate::reviewer::artifacts::ArtifactExportPolicy::raw(
        &crate::reviewer::adapters::capabilities::CapabilitySet::review_read_only()
    )
    .is_err());
    let raw_export = report
        .export_artifacts(raw_artifact_policy.clone())
        .unwrap();
    assert_eq!(
        raw_export.view,
        crate::reviewer::artifacts::ArtifactViewMode::Raw
    );
    assert_eq!(raw_export.artifact_count, export.artifact_count);
    assert!(raw_export.total_bytes > 0);
    let local_artifact_dir = tempfile::tempdir().unwrap();
    let local_artifact_store = crate::reviewer::artifacts::LocalArtifactObjectStore::new(
        local_artifact_dir.path().to_path_buf(),
    );
    let local_manifest = report
        .persist_artifacts(&local_artifact_store, redacted_artifact_policy.clone())
        .unwrap();
    assert_eq!(
        local_manifest.view,
        crate::reviewer::artifacts::ArtifactViewMode::Redacted
    );
    assert_eq!(local_manifest.artifact_count, export.artifact_count);
    assert_eq!(local_manifest.total_bytes, export.total_bytes);
    for object in &local_manifest.objects {
        let path = object.path.as_ref().expect("local object path");
        assert!(path.starts_with(local_artifact_store.root()));
        assert!(path.exists());
        assert_eq!(fs::metadata(path).unwrap().len() as usize, object.bytes);
        let exported_artifact = export
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id() == object.artifact_id())
            .expect("persisted artifact in export manifest");
        assert_eq!(object.content_hash, exported_artifact.content_hash);
        assert_eq!(fs::read_to_string(path).unwrap(), exported_artifact.content);
    }
    let serialized_local_manifest = serde_json::to_string_pretty(&local_manifest).unwrap();
    let restored_local_manifest: crate::reviewer::artifacts::ArtifactPersistenceManifest =
        serde_json::from_str(&serialized_local_manifest).unwrap();
    let reopened_local_store = crate::reviewer::artifacts::LocalArtifactObjectStore::new(
        local_artifact_dir.path().to_path_buf(),
    );
    let local_validation = restored_local_manifest
        .validate_storage(&reopened_local_store)
        .unwrap();
    assert!(local_validation.valid);
    assert_eq!(
        local_validation.expected_objects,
        restored_local_manifest.artifact_count
    );
    assert_eq!(
        local_validation.expected_bytes,
        restored_local_manifest.total_bytes
    );
    assert_eq!(
        local_validation.checked_objects,
        restored_local_manifest.artifact_count
    );
    assert_eq!(
        local_validation.checked_bytes,
        restored_local_manifest.total_bytes
    );
    let first_object = restored_local_manifest.objects[0].clone();
    let first_path = first_object
        .path
        .as_ref()
        .expect("local object path")
        .clone();
    fs::write(&first_path, "stale artifact object").unwrap();
    let stale_validation = restored_local_manifest
        .validate_storage(&reopened_local_store)
        .unwrap();
    assert!(!stale_validation.valid);
    assert!(stale_validation.missing_objects.is_empty());
    assert!(stale_validation
        .stale_objects
        .iter()
        .any(|object| object.artifact_id() == first_object.artifact_id()));
    fs::remove_file(&first_path).unwrap();
    let missing_validation = restored_local_manifest
        .validate_storage(&reopened_local_store)
        .unwrap();
    assert!(!missing_validation.valid);
    assert!(missing_validation
        .missing_objects
        .iter()
        .any(|object| object.artifact_id() == first_object.artifact_id()));
    let local_cleanup = restored_local_manifest
        .cleanup_storage(&reopened_local_store)
        .unwrap();
    assert_eq!(
        local_cleanup.expected_objects,
        restored_local_manifest.artifact_count
    );
    assert_eq!(
        local_cleanup.expected_bytes,
        restored_local_manifest.total_bytes
    );
    assert!(local_cleanup
        .missing_objects
        .iter()
        .any(|object| object.artifact_id() == first_object.artifact_id()));
    assert!(!local_cleanup.removed_objects.is_empty());
    assert!(
        !restored_local_manifest
            .validate_storage(&reopened_local_store)
            .unwrap()
            .valid
    );
    let remote_artifact_client =
        Arc::new(crate::reviewer::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let remote_artifact_store = crate::reviewer::artifacts::RemoteArtifactObjectStore::new(
        "s3://muzen-test-artifacts/public-run/",
        remote_artifact_client.clone(),
    )
    .unwrap();
    let remote_manifest = report
        .persist_artifacts(&remote_artifact_store, redacted_artifact_policy.clone())
        .unwrap();
    assert_eq!(
        remote_manifest.view,
        crate::reviewer::artifacts::ArtifactViewMode::Redacted
    );
    assert_eq!(remote_manifest.artifact_count, export.artifact_count);
    assert_eq!(remote_manifest.total_bytes, export.total_bytes);
    assert_eq!(
        remote_artifact_client.object_count(),
        remote_manifest.artifact_count
    );
    for object in &remote_manifest.objects {
        assert!(object.path.is_none());
        assert!(object.uri.starts_with(&format!(
            "{}/artifacts/redacted/",
            remote_artifact_store.base_uri()
        )));
        assert!(object.uri.ends_with(".txt"));
        assert_eq!(
            remote_artifact_client.read(&object.uri).unwrap().len(),
            object.bytes
        );
    }
    let serialized_remote_manifest = serde_json::to_string_pretty(&remote_manifest).unwrap();
    let restored_remote_manifest: crate::reviewer::artifacts::ArtifactPersistenceManifest =
        serde_json::from_str(&serialized_remote_manifest).unwrap();
    let remote_validation = restored_remote_manifest
        .validate_storage(&remote_artifact_store)
        .unwrap();
    assert!(remote_validation.valid);
    assert_eq!(
        remote_validation.checked_objects,
        restored_remote_manifest.artifact_count
    );
    assert_eq!(
        remote_validation.checked_bytes,
        restored_remote_manifest.total_bytes
    );
    let first_remote_object = restored_remote_manifest.objects[0].clone();
    remote_artifact_client.write(
        first_remote_object.uri.clone(),
        b"stale remote artifact".to_vec(),
    );
    let stale_remote_validation = restored_remote_manifest
        .validate_storage(&remote_artifact_store)
        .unwrap();
    assert!(!stale_remote_validation.valid);
    assert!(stale_remote_validation.missing_objects.is_empty());
    assert!(stale_remote_validation
        .stale_objects
        .iter()
        .any(|object| object.artifact_id() == first_remote_object.artifact_id()));
    remote_artifact_client.remove(&first_remote_object.uri);
    let missing_remote_validation = restored_remote_manifest
        .validate_storage(&remote_artifact_store)
        .unwrap();
    assert!(!missing_remote_validation.valid);
    assert!(missing_remote_validation
        .missing_objects
        .iter()
        .any(|object| object.artifact_id() == first_remote_object.artifact_id()));
    let mut forged_remote_object = first_remote_object.clone();
    forged_remote_object.uri = forged_remote_object
        .uri
        .replace(remote_artifact_store.base_uri(), "s3://forged-bucket");
    assert!(matches!(
        crate::reviewer::artifacts::ArtifactObjectReader::read_artifact_object(
            &remote_artifact_store,
            &forged_remote_object
        ),
        Err(crate::reviewer::adapters::runtime::RuntimeError::RepoAccessDenied)
    ));
    let remote_cleanup = restored_remote_manifest
        .cleanup_storage(&remote_artifact_store)
        .unwrap();
    assert_eq!(
        remote_cleanup.expected_objects,
        restored_remote_manifest.artifact_count
    );
    assert!(remote_cleanup
        .missing_objects
        .iter()
        .any(|object| object.artifact_id() == first_remote_object.artifact_id()));
    assert_eq!(remote_artifact_client.object_count(), 0);
    assert!(
        !restored_remote_manifest
            .validate_storage(&remote_artifact_store)
            .unwrap()
            .valid
    );
    assert!(crate::reviewer::artifacts::RemoteArtifactObjectStore::new(
        "file:///tmp/muzen-artifacts",
        remote_artifact_client.clone()
    )
    .is_err());
    let bundle_dir = tempfile::tempdir().unwrap();
    let bundle = report
        .export_artifact_bundle(bundle_dir.path(), redacted_artifact_policy)
        .unwrap();
    assert_eq!(
        bundle.view,
        crate::reviewer::artifacts::ArtifactViewMode::Redacted
    );
    assert_eq!(bundle.artifact_count, export.artifact_count);
    assert_eq!(bundle.total_bytes, export.total_bytes);
    assert_eq!(bundle.root, bundle_dir.path());
    assert_eq!(
        bundle.retention,
        crate::reviewer::artifacts::ArtifactRetentionPolicy::unlimited()
    );
    assert!(bundle.manifest_path.exists());
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&bundle.manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest["artifactCount"].as_u64(),
        Some(export.artifact_count as u64)
    );
    assert_eq!(manifest["view"].as_str(), Some("redacted"));
    assert_eq!(
        manifest["retention"]["maxArtifacts"],
        serde_json::Value::Null
    );
    assert_eq!(manifest["retention"]["maxBytes"], serde_json::Value::Null);
    for entry in &bundle.artifacts {
        let artifact_path = bundle.root.join(&entry.relative_path);
        assert!(artifact_path.exists());
        assert_eq!(
            fs::read_to_string(artifact_path).unwrap().len(),
            entry.bytes
        );
    }
    let bundle_validation = bundle.validate_storage().unwrap();
    assert!(bundle_validation.valid);
    assert!(bundle_validation.manifest_present);
    assert_eq!(
        bundle_validation.retention,
        crate::reviewer::artifacts::ArtifactRetentionPolicy::unlimited()
    );
    assert_eq!(bundle_validation.checked_artifacts, bundle.artifact_count);
    assert_eq!(bundle_validation.checked_bytes, bundle.total_bytes);
    assert_eq!(
        bundle_validation.checked_objects.len(),
        bundle.artifact_count
    );
    assert!(bundle_validation.missing_artifacts.is_empty());
    assert!(bundle_validation.stale_artifacts.is_empty());
    let first_bundle_path = bundle.root.join(&bundle.artifacts[0].relative_path);
    let first_bundle_bytes = fs::read(&first_bundle_path).unwrap();
    fs::write(&first_bundle_path, b"corrupted artifact bundle").unwrap();
    let stale_bundle = bundle.validate_storage().unwrap();
    assert!(!stale_bundle.valid);
    assert!(stale_bundle.missing_artifacts.is_empty());
    assert_eq!(stale_bundle.stale_artifacts.len(), 1);
    assert_eq!(stale_bundle.stale_artifacts[0].path, first_bundle_path);
    fs::write(&first_bundle_path, first_bundle_bytes).unwrap();
    assert!(bundle.validate_storage().unwrap().valid);
    let raw_bundle = report
        .export_artifact_bundle(bundle_dir.path().join("raw"), raw_artifact_policy)
        .unwrap();
    assert_eq!(
        raw_bundle.view,
        crate::reviewer::artifacts::ArtifactViewMode::Raw
    );
    let raw_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&raw_bundle.manifest_path).unwrap()).unwrap();
    assert_eq!(raw_manifest["view"].as_str(), Some("raw"));
    let cleanup = bundle.cleanup_storage().unwrap();
    assert_eq!(cleanup.root, bundle.root);
    assert_eq!(
        cleanup.view,
        crate::reviewer::artifacts::ArtifactViewMode::Redacted
    );
    assert_eq!(
        cleanup.retention,
        crate::reviewer::artifacts::ArtifactRetentionPolicy::unlimited()
    );
    assert_eq!(cleanup.removed_artifacts, bundle.artifact_count);
    assert_eq!(cleanup.removed_bytes, bundle.total_bytes);
    assert_eq!(cleanup.removed_objects.len(), bundle.artifact_count);
    assert!(cleanup.missing_artifacts.is_empty());
    assert!(cleanup.removed_manifest);
    assert_eq!(cleanup.pruned_empty_directories, 1);
    assert!(!bundle.manifest_path.exists());
    for entry in &bundle.artifacts {
        assert!(!bundle.root.join(&entry.relative_path).exists());
    }
    assert!(raw_bundle.manifest_path.exists());
    let after_cleanup = bundle.validate_storage().unwrap();
    assert!(!after_cleanup.valid);
    assert!(!after_cleanup.manifest_present);
    assert_eq!(after_cleanup.missing_artifacts.len(), bundle.artifact_count);
    let event_records = events.records();
    assert!(!event_records.is_empty());
    for (index, record) in event_records.iter().enumerate() {
        assert_eq!(record.seq, index as u64 + 1);
        assert!(!record.timestamp_utc.is_empty());
        assert_eq!(record.run_id.as_deref(), Some("public-run"));
    }
    let review_event_log_dir = tempfile::tempdir().unwrap();
    let review_event_log = crate::reviewer::events::export_review_event_records_jsonl(
        review_event_log_dir.path().join("review-events.jsonl"),
        &event_records,
    )
    .unwrap();
    assert_eq!(
        review_event_log.schema_version,
        crate::reviewer::events::REVIEW_EVENT_LOG_SCHEMA_VERSION
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
        Some(crate::reviewer::events::REVIEW_EVENT_LOG_SCHEMA_VERSION)
    );
    assert_eq!(
        first_review_event_line["runId"].as_str(),
        Some("public-run")
    );
    let loaded_review_event_log =
        crate::reviewer::events::load_review_event_records_jsonl(&review_event_log.path).unwrap();
    assert_eq!(loaded_review_event_log.path, review_event_log.path);
    assert_eq!(
        loaded_review_event_log.schema_version,
        crate::reviewer::events::REVIEW_EVENT_LOG_SCHEMA_VERSION
    );
    assert_eq!(loaded_review_event_log.record_count, event_records.len());
    assert_eq!(loaded_review_event_log.records, event_records);
    assert_eq!(
        event_records[0].snapshot_id,
        Some(report.snapshot.snapshot_id.clone())
    );
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer::events::ReviewEvent::ToolCallCompleted {
            ok: true,
            error_code: None,
            ..
        }
    ) && record.snapshot_id
        == Some(report.snapshot.snapshot_id.clone())
        && record
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id.starts_with("unit-"))
        && record.turn.is_some()
        && record.tool_call_id.is_some()));
    let event_types = events.events();
    assert!(matches!(
        event_types.first(),
        Some(crate::reviewer::events::ReviewEvent::RunStarted { .. })
    ));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer::events::ReviewEvent::SessionStarted { session_id }
            if session_id.starts_with("unit-")
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer::events::ReviewEvent::ModelStarted { session_id, .. }
            if session_id.starts_with("unit-")
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer::events::ReviewEvent::ToolBatchStarted { count, .. } if *count == 3
    )));
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer::events::ReviewEvent::SearchBatchCompleted { searched_files, .. }
            if *searched_files > 0
    )));
    let artifact_event_ids = event_types
        .iter()
        .filter_map(|event| match event {
            crate::reviewer::events::ReviewEvent::ArtifactCreated { artifact_id, .. } => {
                Some(artifact_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!artifact_event_ids.is_empty());
    for artifact_id in artifact_event_ids {
        assert!(report.artifacts.get(artifact_id).is_some());
    }
    assert!(event_types.iter().any(|event| matches!(
        event,
        crate::reviewer::events::ReviewEvent::FindingRecorded { finding_id, .. }
            if !finding_id.is_empty()
    )));
    assert!(matches!(
        event_types.last(),
        Some(crate::reviewer::events::ReviewEvent::RunFinished { .. })
    ));
}

#[test]
fn public_reviewer_facade_emits_tool_denial_events() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-denied",
            "head-denied",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    )
    .with_path_policy(crate::reviewer::snapshots::SnapshotPathPolicy::standard(
        64 * 1024,
        20,
    ));
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "denied-session",
        crate::contracts::Role::Generalist,
        "Run with read_diff denied.",
        public_budget(),
    )
    .deny_tool(crate::reviewer::adapters::ids::ToolId::from(
        ToolName::ReadDiff,
    ));
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "denied-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::events::ReviewEvent::ToolCallDenied {
            tool_id,
            error_code: crate::reviewer::adapters::tool_adapters::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if tool_id == "read_diff"
            && reason.contains("not allowed")
            && record.run_id.as_deref() == Some("denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("unit-"))
            && record.turn.is_some()
            && record.tool_call_id.is_some()
    )));
}

#[test]
fn public_reviewer_facade_cancelled_run_emits_review_events() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-cancel",
            "head-public-cancel",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    )
    .with_path_policy(crate::reviewer::snapshots::SnapshotPathPolicy::standard(
        64 * 1024,
        20,
    ));
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-cancel-session",
        crate::contracts::Role::Generalist,
        "Run cancellation through the public reviewer facade.",
        public_budget(),
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-cancel-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let cancel = crate::reviewer::adapters::Cancellation::new();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let run = crate::reviewer::run::Run::builder(spec)
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
    assert_eq!(report.summary.model_calls, 0);
    assert_eq!(report.summary.tool_calls, 0);
    let event_records = events.records();
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer::events::ReviewEvent::ModelStarted { session_id, .. }
            if session_id.starts_with("unit-")
                && record.run_id.as_deref() == Some("public-cancel-run")
                && record
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| session_id.starts_with("unit-"))
                && record.turn.is_some()
    )));
    assert!(!event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer::events::ReviewEvent::ModelCompleted { .. }
            | crate::reviewer::events::ReviewEvent::ToolBatchStarted { .. }
            | crate::reviewer::events::ReviewEvent::ToolCallCompleted { .. }
    )));
    assert!(event_records.iter().any(|record| matches!(
        &record.event,
        crate::reviewer::events::ReviewEvent::SessionFinished { session_id, status }
            if session_id.starts_with("unit-")
                && status == "partial"
                && record.run_id.as_deref() == Some("public-cancel-run")
                && record
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| session_id.starts_with("unit-"))
    )));
    assert!(matches!(
        event_records.last().map(|record| &record.event),
        Some(crate::reviewer::events::ReviewEvent::RunFinished { status }) if status == "partial"
    ));
}

#[test]
fn public_reviewer_facade_runs_multiple_snapshots() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    fs::write(first.path().join("README.md"), "needle in first\n").unwrap();
    fs::write(second.path().join("README.md"), "needle in second\n").unwrap();
    let first_id = crate::reviewer::adapters::ids::SnapshotId("snapshot-first".to_string());
    let second_id = crate::reviewer::adapters::ids::SnapshotId("snapshot-second".to_string());
    let first_snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        first.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-first",
            "head-first",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    )
    .with_snapshot_id(first_id.clone())
    .with_path_policy(crate::reviewer::snapshots::SnapshotPathPolicy::standard(
        64 * 1024,
        20,
    ));
    let second_snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        second.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-second",
            "head-second",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    )
    .with_snapshot_id(second_id.clone())
    .with_path_policy(crate::reviewer::snapshots::SnapshotPathPolicy::standard(
        64 * 1024,
        20,
    ));
    let sessions = vec![
        crate::reviewer::spec::ReviewSessionSpec::review_read_only(
            "first-session",
            crate::contracts::Role::Generalist,
            "Review first snapshot.",
            public_budget(),
        )
        .with_snapshot_id(first_id.clone()),
        crate::reviewer::spec::ReviewSessionSpec::review_read_only(
            "second-session",
            crate::contracts::Role::Generalist,
            "Review second snapshot.",
            public_budget(),
        )
        .with_snapshot_id(second_id.clone()),
    ];
    let spec = crate::reviewer::spec::RunSpec {
        run_id: "multi-snapshot-run".to_string(),
        snapshots: vec![first_snapshot, second_snapshot],
        sessions,
        limits: crate::reviewer::spec::ReviewRunLimits::standard(2, 64 * 1024, 20),
    };
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
    assert_eq!(report.summary.sessions, 2);
    assert_eq!(report.summary.completed_sessions, 2);
    assert_eq!(report.summary.findings, 2);
    assert_eq!(report.summary.snapshot_count, 2);
    assert_eq!(report.metrics.snapshot_metrics.len(), 2);
    assert!(report
        .metrics
        .snapshot_metrics
        .iter()
        .any(|metrics| metrics.snapshot_id == first_id && metrics.sessions == 1));
    assert!(report
        .metrics
        .snapshot_metrics
        .iter()
        .any(|metrics| metrics.snapshot_id == second_id && metrics.sessions == 1));
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
                crate::reviewer::events::ReviewEvent::SnapshotStarted { .. }
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                crate::reviewer::events::ReviewEvent::SnapshotFinished { .. }
            ))
            .count(),
        2
    );
}

#[test]
fn public_reviewer_facade_runs_custom_tool_and_exports_metrics() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-1",
            "head-1",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "custom-session",
        crate::contracts::Role::Generalist,
        "Run host custom check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool(custom_tool_id.clone());
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-custom-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::adapters::tool_adapters::ToolProviderHealthState::Healthy
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
    let mut raw_artifact_capabilities =
        crate::reviewer::adapters::capabilities::CapabilitySet::review_read_only();
    raw_artifact_capabilities.artifact_access.read_raw = true;
    let raw_artifact_text = report
        .export_artifacts(
            crate::reviewer::artifacts::ArtifactExportPolicy::raw(&raw_artifact_capabilities)
                .unwrap(),
        )
        .unwrap()
        .artifacts
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
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-host-resource",
            "head-host-resource",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "host-resource-session",
        crate::contracts::Role::Generalist,
        "Run host custom check with a provider resource.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool_for_resources(custom_tool_id.clone(), vec![resource_id]);
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-host-resource-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer::run::Run::builder(spec)
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
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-host-resource-denied",
            "head-host-resource-denied",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "host-resource-denied-session",
        crate::contracts::Role::Generalist,
        "Run host custom check outside provider resource scope.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_custom_read_only_tool_for_resources(custom_tool_id.clone(), vec![denied_resource]);
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-host-resource-denied-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer::adapters::tool_adapters::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("provider resource")
            && record.run_id.as_deref() == Some("public-host-resource-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("unit-"))
    )));
}

#[test]
fn public_reviewer_facade_runs_scoped_jsonrpc_provider_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id =
        crate::reviewer::adapters::tool_adapters::ToolProviderId::parse("public_jsonrpc_provider")
            .unwrap();
    let resource_id =
        crate::reviewer::adapters::tool_adapters::ProviderResourceId::parse("github/org-a/repo-a")
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer::tools::ReviewJsonRpcReadOnlyToolRegistration {
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-jsonrpc",
            "head-public-jsonrpc",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-session",
        crate::contracts::Role::Generalist,
        "Run public JSON-RPC provider check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id],
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-jsonrpc-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::adapters::tool_adapters::ToolMetricKey::new(&provider_id, &tool_id);
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
        crate::reviewer::adapters::tool_adapters::ToolProviderHealthState::Healthy
    );
}

#[test]
fn public_reviewer_facade_runs_http_jsonrpc_provider_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer::adapters::tool_adapters::ToolProviderId::parse(
        "public_http_jsonrpc_provider",
    )
    .unwrap();
    let resource_id =
        crate::reviewer::adapters::tool_adapters::ProviderResourceId::parse("github/org-http/repo")
            .unwrap();
    let server = LoopbackJsonRpcToolServer::spawn();
    let transport =
        crate::reviewer::adapters::tool_adapters::HttpJsonRpcToolTransport::new(server.endpoint())
            .unwrap();
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer::tools::ReviewJsonRpcReadOnlyToolRegistration {
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-http-jsonrpc",
            "head-public-http-jsonrpc",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-http-jsonrpc-session",
        crate::contracts::Role::Generalist,
        "Run public HTTP JSON-RPC provider check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id.clone()],
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-http-jsonrpc-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::adapters::tool_adapters::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.successes, 1);
}

#[test]
fn public_reviewer_facade_runs_jsonrpc_network_read_tool_with_authority() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer::adapters::tool_adapters::ToolProviderId::parse(
        "public_jsonrpc_network_provider",
    )
    .unwrap();
    let resource_id = crate::reviewer::adapters::tool_adapters::ProviderResourceId::parse(
        "github/org-network/repo",
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_network_read_tool(
            crate::reviewer::tools::ReviewJsonRpcNetworkReadToolRegistration {
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-network",
            "head-public-jsonrpc-network",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-network-session",
        crate::contracts::Role::Generalist,
        "Run public JSON-RPC provider network check.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_network_read_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![resource_id],
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-jsonrpc-network-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::adapters::tool_adapters::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.successes, 1);
}

#[test]
fn public_reviewer_facade_denies_jsonrpc_network_read_without_authority() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer::adapters::tool_adapters::ToolProviderId::parse(
        "public_jsonrpc_network_denied",
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
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
    let mut capabilities =
        crate::reviewer::adapters::capabilities::CapabilitySet::review_read_only();
    capabilities.grant_tool(
        tool_id.clone(),
        crate::reviewer::adapters::capabilities::ToolGrant {
            allow: true,
            max_calls: None,
            effects_allowed: crate::reviewer::adapters::capabilities::ToolEffects {
                network_read: true,
                ..crate::reviewer::adapters::capabilities::ToolEffects::review_read_only()
            },
        },
    );
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-network-denied",
            "head-public-jsonrpc-network-denied",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-network-denied-session",
        crate::contracts::Role::Generalist,
        "Run public JSON-RPC provider network check without network authority.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .with_capabilities(capabilities);
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-jsonrpc-network-denied-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer::adapters::tool_adapters::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("network read")
            && record.run_id.as_deref() == Some("public-jsonrpc-network-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("unit-"))
    )));
    let metric_key =
        crate::reviewer::adapters::tool_adapters::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.errors, 1);
}

#[test]
fn public_reviewer_facade_denies_jsonrpc_provider_resource_outside_scope() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "hello\n").unwrap();
    let provider_id = crate::reviewer::adapters::tool_adapters::ToolProviderId::parse(
        "public_jsonrpc_denied_provider",
    )
    .unwrap();
    let allowed_resource =
        crate::reviewer::adapters::tool_adapters::ProviderResourceId::parse("github/org-a/repo-a")
            .unwrap();
    let denied_resource =
        crate::reviewer::adapters::tool_adapters::ProviderResourceId::parse("github/org-b/repo-b")
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = crate::reviewer::tools::ReviewToolRegistry::review_defaults().unwrap();
    let tool_id = registry
        .register_scoped_jsonrpc_read_only_tool(
            crate::reviewer::tools::ReviewJsonRpcReadOnlyToolRegistration {
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
    let snapshot = crate::reviewer::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer::snapshots::ChangeSpec::local(
            "change-public-jsonrpc-denied",
            "head-public-jsonrpc-denied",
            vec![crate::reviewer::snapshots::ChangedFileSpec::modified(
                "README.md",
            )],
        ),
    );
    let session = crate::reviewer::spec::ReviewSessionSpec::review_read_only(
        "public-jsonrpc-denied-session",
        crate::contracts::Role::Generalist,
        "Run public JSON-RPC provider check outside resource scope.",
        public_budget(),
    )
    .with_model_profile_id("mock")
    .grant_provider_read_only_tool_for_resources(
        provider_id.clone(),
        tool_id.clone(),
        vec![denied_resource],
    );
    let spec = crate::reviewer::spec::RunSpec::single_snapshot(
        "public-jsonrpc-denied-run",
        snapshot,
        vec![session],
        crate::reviewer::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let events = Arc::new(crate::reviewer::events::InMemoryReviewEventSink::default());
    let run = crate::reviewer::run::Run::builder(spec)
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
        crate::reviewer::events::ReviewEvent::ToolCallDenied {
            error_code: crate::reviewer::adapters::tool_adapters::ToolErrorCode::ToolNotAllowed,
            reason,
            ..
        } if reason.contains("provider resource")
            && record.run_id.as_deref() == Some("public-jsonrpc-denied-run")
            && record
                .session_id
                .as_deref()
                .is_some_and(|session_id| session_id.starts_with("unit-"))
    )));
    let metric_key =
        crate::reviewer::adapters::tool_adapters::ToolMetricKey::new(&provider_id, &tool_id);
    let metrics = &report.metrics.tool_metrics[&metric_key];
    assert_eq!(metrics.calls, 1);
    assert_eq!(metrics.errors, 1);
}

#[test]
fn public_bounded_event_sink_drops_after_capacity() {
    let sink = crate::reviewer::runtime_events::BoundedInMemoryEventSink::new(1);
    crate::reviewer::runtime_events::EventSink::emit(
        &sink,
        crate::reviewer::runtime_events::RuntimeEvent::JobStarted {
            snapshot_id: crate::reviewer::adapters::ids::SnapshotId("snapshot".to_string()),
        },
    );
    crate::reviewer::runtime_events::EventSink::emit(
        &sink,
        crate::reviewer::runtime_events::RuntimeEvent::JobFinished {
            status: "done".to_string(),
        },
    );

    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, 1);
    assert_eq!(
        records[0].context.snapshot_id,
        Some(crate::reviewer::adapters::ids::SnapshotId(
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

    let oldest = crate::reviewer::runtime_events::BoundedInMemoryEventSink::with_policy(
        1,
        crate::reviewer::runtime_events::EventBackpressurePolicy::DropOldest,
    );
    crate::reviewer::runtime_events::EventSink::emit(
        &oldest,
        crate::reviewer::runtime_events::RuntimeEvent::JobStarted {
            snapshot_id: crate::reviewer::adapters::ids::SnapshotId("snapshot".to_string()),
        },
    );
    crate::reviewer::runtime_events::EventSink::emit(
        &oldest,
        crate::reviewer::runtime_events::RuntimeEvent::JobFinished {
            status: "done".to_string(),
        },
    );
    let records = oldest.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, 2);
    assert!(matches!(
        records[0].event,
        crate::reviewer::runtime_events::RuntimeEvent::JobFinished { .. }
    ));
    assert_eq!(oldest.dropped_count(), 1);
}
