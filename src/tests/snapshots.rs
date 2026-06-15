use super::prelude::*;
use super::support::*;

#[test]
fn public_snapshot_storage_policy_reports_memory_envelope_skips() {
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
    .with_memory_storage_limit(0);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "storage-run",
        snapshot,
        Vec::new(),
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
    let manifest = report.snapshot_manifests().pop().unwrap();

    assert_eq!(manifest.max_captured_text_bytes(), 0);
    assert!(!manifest.storage_policy_hash.is_empty());
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
fn public_snapshot_content_addressed_store_serves_captured_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "persistent needle\n").unwrap();
    let store_root = store.path().join("snapshots");
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-content-addressed",
            "head-content-addressed",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    )
    .with_content_addressed_storage(store_root.clone(), 64 * 1024);
    let session = crate::reviewer_kernel::spec::ReviewSessionSpec::review_read_only(
        "content-addressed-session",
        crate::reviewer_kernel::review_contract::Role::Generalist,
        "Run content-addressed snapshot evidence through public facade.",
        public_budget(),
    );
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "content-addressed-run",
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
    fs::write(temp.path().join("README.md"), "mutated needle\n").unwrap();

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());
    let manifest = report.snapshot_manifests().pop().unwrap();
    assert!(manifest.uses_content_addressed_storage());
    let readme = manifest
        .files
        .iter()
        .find(|file| file.path.display() == "README.md")
        .expect("README.md in content-addressed snapshot manifest");
    let content_hash = readme.content_hash.clone().unwrap();
    let blob_path = store_root
        .join(content_hash.get(..2).unwrap_or("00"))
        .join(&content_hash);
    assert_eq!(
        fs::read_to_string(blob_path).unwrap(),
        "persistent needle\n"
    );

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

#[test]
fn public_snapshot_content_addressed_store_lifecycle_validates_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "lifecycle needle\n").unwrap();
    let store_root = store.path().join("snapshots");
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-storage-lifecycle",
            "head-storage-lifecycle",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    )
    .with_content_addressed_storage(store_root.clone(), 64 * 1024);
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "storage-lifecycle-run",
        snapshot,
        Vec::new(),
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
    let reader = report
        .snapshot_reader(&report.snapshot.snapshot_id)
        .unwrap();

    let validation = reader.validate_storage().unwrap();
    assert_eq!(validation.snapshot_id, report.snapshot.snapshot_id);
    assert!(validation.uses_content_addressed_storage());
    assert!(validation.valid);
    assert_eq!(validation.checked_files, 1);
    assert_eq!(validation.checked_bytes, "lifecycle needle\n".len());
    assert_eq!(validation.checked_objects.len(), 1);
    assert!(validation.missing_files.is_empty());
    assert!(validation.stale_files.is_empty());
    let store_path = validation.checked_objects[0]
        .store_path
        .as_ref()
        .expect("validated content-addressed object path")
        .clone();
    assert!(store_path.exists());

    fs::write(&store_path, "corrupted lifecycle\n").unwrap();
    let stale = reader.validate_storage().unwrap();
    assert!(!stale.valid);
    assert!(stale.missing_files.is_empty());
    assert_eq!(stale.stale_files.len(), 1);
    assert_eq!(stale.stale_files[0].store_path.as_ref(), Some(&store_path));
    assert!(matches!(
        reader.read_text_path("README.md", 64 * 1024),
        Err(crate::reviewer_kernel::kernel_types::RuntimeError::SnapshotStale { path }) if path == "README.md"
    ));
    fs::write(&store_path, "lifecycle needle\n").unwrap();
    assert!(reader.validate_storage().unwrap().valid);

    let cleanup = reader.cleanup_storage().unwrap();
    assert_eq!(cleanup.snapshot_id, report.snapshot.snapshot_id);
    assert_eq!(cleanup.removed_files, 1);
    assert_eq!(cleanup.removed_bytes, "lifecycle needle\n".len());
    assert_eq!(cleanup.removed_objects.len(), 1);
    assert!(cleanup.missing_files.is_empty());
    assert_eq!(cleanup.pruned_empty_directories, 1);
    let removed_path = cleanup.removed_objects[0]
        .store_path
        .as_ref()
        .expect("removed content-addressed object path")
        .clone();
    assert_eq!(removed_path, store_path);
    assert!(!removed_path.exists());
    assert!(store_root.exists());

    let after_cleanup = reader.validate_storage().unwrap();
    assert!(!after_cleanup.valid);
    assert_eq!(after_cleanup.missing_files.len(), 1);
    assert_eq!(after_cleanup.missing_files[0].path.display(), "README.md");
    assert!(matches!(
        reader.read_text_path("README.md", 64 * 1024),
        Err(crate::reviewer_kernel::kernel_types::RuntimeError::RepoUnavailable(_))
    ));
}

#[test]
fn public_snapshot_remote_object_store_serves_and_validates_captured_evidence() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "remote lifecycle needle\n").unwrap();
    let remote_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let remote_base = "s3://muzen-test-snapshots/storage-lifecycle-run";
    let remote_store = Arc::new(
        crate::reviewer_kernel::snapshots::RemoteSnapshotObjectStore::new(
            remote_base,
            remote_client.clone(),
        )
        .unwrap(),
    );
    let snapshot = crate::reviewer_kernel::snapshots::SnapshotSpec::new(
        temp.path().to_path_buf(),
        crate::reviewer_kernel::snapshots::ChangeSpec::local(
            "change-remote-storage-lifecycle",
            "head-remote-storage-lifecycle",
            vec![crate::reviewer_kernel::snapshots::ChangedFileSpec::modified("README.md")],
        ),
    )
    .with_path_policy(
        crate::reviewer_kernel::snapshots::SnapshotPathPolicy::standard(64 * 1024, 20),
    )
    .with_remote_object_storage(remote_base, 64 * 1024, remote_store.clone())
    .unwrap();
    let spec = crate::reviewer_kernel::spec::RunSpec::single_snapshot(
        "remote-storage-lifecycle-run",
        snapshot,
        Vec::new(),
        crate::reviewer_kernel::spec::ReviewRunLimits::standard(1, 64 * 1024, 20),
    );
    let run = crate::reviewer_kernel::kernel::Run::builder(spec)
        .review_model(Arc::new(PublicFacadeModel {
            path: "README.md".to_string(),
            query: "needle".to_string(),
        }))
        .build()
        .unwrap();
    fs::write(temp.path().join("README.md"), "mutated remote needle\n").unwrap();

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = tokio.block_on(run.execute());
    let manifest = report.snapshot_manifests().pop().unwrap();
    assert!(manifest.uses_remote_object_storage());
    assert_eq!(remote_client.object_count(), 1);
    let reader = report
        .snapshot_reader(&report.snapshot.snapshot_id)
        .unwrap();
    let snapshot_text = reader.read_text_path("README.md", 64 * 1024).unwrap();
    assert!(snapshot_text.content.contains("remote lifecycle needle"));
    assert!(!snapshot_text.content.contains("mutated remote needle"));

    let validation = reader.validate_storage().unwrap();
    assert!(validation.uses_remote_object_storage());
    assert!(validation.valid);
    assert_eq!(validation.checked_files, 1);
    assert_eq!(validation.checked_bytes, "remote lifecycle needle\n".len());
    let object = validation.checked_objects[0].clone();
    assert!(object.store_path.is_none());
    let store_uri = object
        .store_uri
        .as_ref()
        .expect("remote snapshot object uri");
    assert!(store_uri.starts_with(&format!("{remote_base}/snapshots/")));
    assert_eq!(remote_client.read(store_uri).unwrap().len(), object.bytes);

    remote_client.write(store_uri.clone(), b"stale remote snapshot".to_vec());
    let stale = reader.validate_storage().unwrap();
    assert!(!stale.valid);
    assert!(stale.missing_files.is_empty());
    assert_eq!(stale.stale_files.len(), 1);
    assert_eq!(stale.stale_files[0].store_uri.as_ref(), Some(store_uri));
    assert!(matches!(
        reader.read_text_path("README.md", 64 * 1024),
        Err(crate::reviewer_kernel::kernel_types::RuntimeError::SnapshotStale { path }) if path == "README.md"
    ));

    remote_client.write(store_uri.clone(), b"remote lifecycle needle\n".to_vec());
    assert!(reader.validate_storage().unwrap().valid);

    let forged_uri = store_uri.replace(remote_base, "s3://forged-snapshots");
    assert!(matches!(
        crate::reviewer_kernel::kernel_types::SnapshotObjectStore::read_snapshot_object(
            remote_store.as_ref(),
            &forged_uri
        ),
        Err(crate::reviewer_kernel::kernel_types::RuntimeError::RepoAccessDenied)
    ));
    assert!(
        crate::reviewer_kernel::snapshots::RemoteSnapshotObjectStore::new(
            "file:///tmp/muzen-snapshots",
            remote_client.clone()
        )
        .is_err()
    );

    let cleanup = reader.cleanup_storage().unwrap();
    assert_eq!(cleanup.snapshot_id, report.snapshot.snapshot_id);
    assert_eq!(cleanup.removed_files, 1);
    assert_eq!(cleanup.removed_bytes, "remote lifecycle needle\n".len());
    assert_eq!(cleanup.removed_objects.len(), 1);
    assert_eq!(
        cleanup.removed_objects[0].store_uri.as_ref(),
        Some(store_uri)
    );
    assert!(cleanup.removed_objects[0].store_path.is_none());
    assert!(cleanup.missing_files.is_empty());
    assert_eq!(cleanup.pruned_empty_directories, 0);
    assert!(remote_client.read(store_uri).is_none());

    let after_cleanup = reader.validate_storage().unwrap();
    assert!(!after_cleanup.valid);
    assert_eq!(after_cleanup.missing_files.len(), 1);
    assert_eq!(
        after_cleanup.missing_files[0].store_uri.as_ref(),
        Some(store_uri)
    );
    assert!(matches!(
        reader.read_text_path("README.md", 64 * 1024),
        Err(crate::reviewer_kernel::kernel_types::RuntimeError::RepoUnavailable(_))
    ));
}
