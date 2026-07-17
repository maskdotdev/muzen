use super::prelude::*;
use super::support::*;

#[test]
fn public_snapshot_capture_policy_reports_memory_envelope_skips() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-storage",
            "head-storage",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    )
    .with_capture_limit(0);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "storage-run",
        snapshot,
        Vec::new(),
        crate::reviewer_kernel::kernel_types::RuntimeLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .model_client(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .build()
        .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());
    let manifest = report.snapshot_manifests().pop().unwrap();

    assert_eq!(manifest.max_captured_text_bytes(), 0);
    assert!(!manifest.capture_policy_hash.is_empty());
    assert_eq!(manifest.captured_text_file_count, 0);
    assert_eq!(manifest.captured_text_bytes, 0);
    assert_eq!(manifest.capture_skipped_file_count, 1);
    assert!(manifest.capture_skipped_bytes > 0);
    assert!(manifest.files[0].capture_skipped_memory_limit());
    let text = report
        .snapshot_reader(&report.snapshot.snapshot_id)
        .unwrap()
        .read_text_path("README.md", 64 * 1024)
        .unwrap();
    assert_eq!(text.content, "needle\n");
}

#[test]
fn disk_backed_snapshot_content_is_readable_and_searchable() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("README.md"),
        "disk-backed-needle\nsecond line\n",
    )
    .unwrap();
    let change = test_change_with_file("README.md");
    let policy = PathPolicyV1::bench(64 * 1024, 20);
    let snapshot = RepoSnapshot::build_with_capture_policy(
        temp.path(),
        &policy,
        &change,
        crate::reviewer_kernel::kernel_types::SnapshotCapturePolicy::new(0),
    )
    .unwrap();
    let manifest_file = &snapshot.manifest.files[0];
    assert!(manifest_file.is_text_candidate);
    assert!(manifest_file.content_ref.is_some());
    assert!(
        manifest_file.capture_status
            == crate::reviewer_kernel::kernel_types::SnapshotCaptureStatus::SkippedMemoryLimit
    );

    let engine = ToolEngine::new(
        snapshot,
        Arc::new(RuntimeLimits::standard(1, 64 * 1024, 20)),
    )
    .unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let results = tokio.block_on(engine.execute_batch(
        test_scope("disk-backed-session"),
        TurnId(0),
        vec![
            ModelToolCall {
                call_id: ToolCallId("read-disk-backed".to_string()),
                index: 0,
                name: ToolId::from(ToolName::ReadFile),
                raw_arguments: serde_json::json!({ "path": "README.md" }).to_string(),
            },
            ModelToolCall {
                call_id: ToolCallId("search-disk-backed".to_string()),
                index: 1,
                name: ToolId::from(ToolName::SearchText),
                raw_arguments: serde_json::json!({ "query": "disk-backed-needle" }).to_string(),
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    ));

    assert_eq!(results.len(), 2);
    assert!(results[0].ok);
    assert!(results[0]
        .data
        .as_ref()
        .unwrap()
        .get("content")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("disk-backed-needle"));
    assert!(results[1].ok);
    assert_eq!(
        results[1]
            .data
            .as_ref()
            .unwrap()
            .get("returnedMatches")
            .unwrap()
            .as_u64()
            .unwrap(),
        1
    );
}

#[test]
fn public_snapshot_capture_serves_stable_evidence() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "persistent needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-captured-evidence",
            "head-captured-evidence",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "captured-evidence-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run captured snapshot evidence through public facade.",
        public_budget(),
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "captured-evidence-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::kernel_types::RuntimeLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .model_client(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .build()
        .unwrap();
    fs::write(temp.path().join("README.md"), "mutated needle\n").unwrap();

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());
    let snapshot_text = report
        .snapshot_reader(&report.snapshot.snapshot_id)
        .unwrap()
        .read_text_path("README.md", 64 * 1024)
        .unwrap();
    assert!(snapshot_text.content.contains("persistent needle"));
    assert!(!snapshot_text.content.contains("mutated needle"));

    let artifact_text = report
        .artifacts
        .list()
        .into_iter()
        .map(|artifact| artifact.content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(artifact_text.contains("persistent needle"));
    assert!(!artifact_text.contains("mutated needle"));
}
