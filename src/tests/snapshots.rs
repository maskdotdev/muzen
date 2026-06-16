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
        .review_model(Arc::new(PublicFacadeModel {
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
    assert!(matches!(
        report
            .snapshot_reader(&report.snapshot.snapshot_id)
            .unwrap()
            .read_text_path("README.md", 64 * 1024),
        Err(
            crate::reviewer_kernel::kernel_types::RuntimeError::LimitExceeded {
                kind: "snapshot_capture_bytes"
            }
        )
    ));
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
        .review_model(Arc::new(PublicFacadeModel {
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
