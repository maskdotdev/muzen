use super::prelude::*;
use super::support::*;

#[test]
fn public_artifact_workflow_facade_persists_and_validates_without_low_level_ids() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "needle\n").unwrap();
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-artifacts",
            "head-artifacts",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    );
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "artifact-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Gather artifact evidence.",
        public_budget(),
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "artifact-run",
        snapshot,
        vec![session],
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
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
    let finding = report.findings().into_iter().next().unwrap();
    let export = report.redacted_artifacts().export().unwrap();
    let artifact_id = export.first_artifact_id().unwrap().to_string();
    let artifacts = report
        .redacted_artifacts()
        .only_artifacts([artifact_id.as_str()])
        .with_retention_policy(
            crate::reviewer_kernel::artifacts::ArtifactRetentionPolicy::max_artifacts(1),
        );

    let evidence_artifacts = artifacts.finding_evidence(&finding.id).unwrap();
    if !evidence_artifacts.is_empty() {
        assert_eq!(evidence_artifacts.len(), 1);
        assert_eq!(evidence_artifacts[0].artifact_id(), artifact_id);
    }

    let memory_store = crate::reviewer_kernel::artifacts::InMemoryArtifactObjectStore::default();
    let memory_manifest = artifacts.persist_to(&memory_store).unwrap();
    assert!(memory_manifest.contains_artifact_id(&artifact_id));
    assert_eq!(memory_manifest.object_refs().len(), 1);
    let memory_object = memory_manifest.first_object_ref().unwrap();
    assert_eq!(
        memory_object.view(),
        crate::reviewer_kernel::artifacts::ArtifactViewMode::Redacted
    );
    assert!(!memory_object.has_local_path());
    assert!(memory_object.path().is_none());
    assert!(memory_object
        .uri()
        .starts_with("memory://artifacts/redacted/"));
    assert_eq!(memory_object.content_hash().len(), 64);
    assert_eq!(
        memory_store
            .read_object(memory_object)
            .unwrap()
            .unwrap()
            .len(),
        memory_object.bytes()
    );
    let memory_validation = memory_manifest.validate_storage(&memory_store).unwrap();
    assert!(memory_validation.valid);
    assert!(!memory_validation.has_missing_artifact(&artifact_id));
    assert!(!memory_validation.has_stale_artifact(&artifact_id));
    let memory_cleanup = memory_manifest.cleanup_storage(&memory_store).unwrap();
    assert!(memory_cleanup.has_removed_artifact(&artifact_id));
    assert!(!memory_cleanup.has_missing_artifact(&artifact_id));
    assert_eq!(memory_store.object_count(), 0);
    let missing_after_cleanup = memory_manifest.validate_storage(&memory_store).unwrap();
    assert!(!missing_after_cleanup.valid);
    assert!(missing_after_cleanup.has_missing_artifact(&artifact_id));

    let local_dir = tempfile::tempdir().unwrap();
    let local_store =
        crate::reviewer_kernel::artifacts::LocalArtifactObjectStore::new(local_dir.path());
    let local_manifest = report
        .redacted_artifacts()
        .only_artifacts([artifact_id.as_str()])
        .persist_to(&local_store)
        .unwrap();
    let local_object = local_manifest.first_object_ref().unwrap();
    assert!(local_object.has_local_path());
    assert!(local_object.path().unwrap().starts_with(local_store.root()));
    assert_eq!(
        fs::metadata(local_object.path().unwrap()).unwrap().len() as usize,
        local_object.bytes()
    );
}

#[test]
fn public_artifact_bundle_lifecycle_rejects_unsafe_relative_paths() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = crate::reviewer_kernel::artifacts::ArtifactBundleManifest::new(
        crate::reviewer_kernel::artifacts::ArtifactViewMode::Redacted,
        temp.path(),
        crate::reviewer_kernel::artifacts::ArtifactRetentionPolicy::unlimited(),
        vec![crate::reviewer_kernel::artifacts::ArtifactBundleEntry::new(
            "unsafe",
            0,
            "hash",
            "../outside.txt",
        )],
    );

    assert!(matches!(
        bundle.validate_storage(),
        Err(crate::reviewer_kernel::adapters::runtime::RuntimeError::RepoAccessDenied)
    ));
    assert!(matches!(
        bundle.cleanup_storage(),
        Err(crate::reviewer_kernel::adapters::runtime::RuntimeError::RepoAccessDenied)
    ));

    let forged_manifest = crate::reviewer_kernel::artifacts::ArtifactBundleManifest::new(
        crate::reviewer_kernel::artifacts::ArtifactViewMode::Redacted,
        temp.path(),
        crate::reviewer_kernel::artifacts::ArtifactRetentionPolicy::unlimited(),
        Vec::new(),
    )
    .with_manifest_path(temp.path().join("outside-manifest.json"));
    assert!(matches!(
        forged_manifest.validate_storage(),
        Err(crate::reviewer_kernel::adapters::runtime::RuntimeError::RepoAccessDenied)
    ));
    assert!(matches!(
        forged_manifest.cleanup_storage(),
        Err(crate::reviewer_kernel::adapters::runtime::RuntimeError::RepoAccessDenied)
    ));
}
