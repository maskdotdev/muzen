use super::prelude::*;
use super::support::*;

#[test]
fn public_remote_snapshot_object_store_canary_proves_integrity_and_cleanup() {
    let remote_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let evidence = crate::operational_proof::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        remote_client.as_ref(),
    );

    evidence.require_passed().expect("snapshot canary passed");
    assert_eq!(
        evidence.schema_version,
        crate::operational_proof::canaries::REMOTE_OBJECT_STORE_CANARY_SCHEMA_VERSION
    );
    assert_eq!(
        evidence.target,
        crate::operational_proof::canaries::RemoteObjectStoreCanaryTarget::Snapshot
    );
    assert!(evidence.cleanup_supported);
    assert_eq!(evidence.gate.passed, 4);
    assert_eq!(evidence.gate.failed, 0);
    assert_eq!(evidence.gate.skipped, 0);
    assert_eq!(remote_client.object_count(), 0);

    let evidence_dir = tempfile::tempdir().unwrap();
    let evidence_path = evidence_dir
        .path()
        .join("canaries")
        .join("remote-snapshot-object-store.json");
    let export = crate::operational_proof::canaries::export_remote_object_store_canary_evidence(
        &evidence_path,
        &evidence,
    )
    .unwrap();
    assert!(export.valid);
    assert_eq!(export.path, evidence_path);
    let serialized = fs::read_to_string(&export.path).unwrap();
    assert!(serialized.ends_with('\n'));
    let loaded: crate::operational_proof::canaries::RemoteObjectStoreCanaryEvidence =
        serde_json::from_str(&serialized).unwrap();
    loaded.require_passed().expect("loaded canary passed");

    let mut forged = loaded.clone();
    forged.steps.pop();
    forged.gate.valid = true;
    forged.gate.failures.clear();
    let error = forged.require_passed().unwrap_err().to_string();
    assert!(error.contains("stored remote object-store canary gate does not match steps"));
    assert!(error.contains("missing ReadAfterRemove canary step"));
}

#[test]
fn public_remote_artifact_object_store_canary_proves_integrity_and_cleanup() {
    let remote_client =
        Arc::new(crate::reviewer_kernel::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let evidence = crate::operational_proof::canaries::run_remote_artifact_object_store_canary(
        "s3://muzen-test-artifacts/canary",
        remote_client.as_ref(),
    );

    evidence.require_passed().expect("artifact canary passed");
    assert_eq!(
        evidence.target,
        crate::operational_proof::canaries::RemoteObjectStoreCanaryTarget::Artifact
    );
    assert!(evidence.cleanup_supported);
    assert_eq!(evidence.gate.passed, 4);
    assert_eq!(evidence.gate.failed, 0);
    assert_eq!(evidence.gate.skipped, 0);
    assert_eq!(remote_client.object_count(), 0);
    let object_uri = evidence.object_uri.as_ref().expect("object uri");
    assert!(object_uri.starts_with("s3://muzen-test-artifacts/canary/artifacts/redacted/"));
    assert!(remote_client.read(object_uri).is_none());
    assert!(evidence.steps.iter().all(|step| {
        step.status == crate::operational_proof::canaries::RemoteObjectStoreCanaryStatus::Passed
    }));
}

#[test]
fn public_canary_evidence_manifest_gates_provider_and_remote_store_evidence() {
    let model_provider = passing_model_provider_canary_evidence();
    let snapshot_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let artifact_client =
        Arc::new(crate::reviewer_kernel::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let snapshot = crate::operational_proof::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        snapshot_client.as_ref(),
    );
    let artifact = crate::operational_proof::canaries::run_remote_artifact_object_store_canary(
        "s3://muzen-test-artifacts/canary",
        artifact_client.as_ref(),
    );
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "manifest-time",
        Some(model_provider.clone()),
        vec![snapshot.clone(), artifact.clone()],
    );

    manifest.require_passed().expect("manifest passed");
    assert_eq!(
        manifest.schema_version,
        crate::operational_proof::canaries::CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION
    );
    assert_eq!(
        manifest.gate.passed,
        model_provider.gate.passed + snapshot.gate.passed + artifact.gate.passed
    );
    assert_eq!(manifest.gate.failed, 0);
    assert_eq!(manifest.gate.skipped, 0);

    let evidence_dir = tempfile::tempdir().unwrap();
    let evidence_path = evidence_dir.path().join("canaries").join("manifest.json");
    let export = crate::operational_proof::canaries::export_canary_evidence_manifest(
        &evidence_path,
        &manifest,
    )
    .unwrap();
    assert!(export.valid);
    assert_eq!(export.path, evidence_path);
    let serialized = fs::read_to_string(&export.path).unwrap();
    assert!(serialized.ends_with('\n'));
    let loaded: crate::operational_proof::canaries::CanaryEvidenceManifest =
        serde_json::from_str(&serialized).unwrap();
    loaded.require_passed().expect("loaded manifest passed");

    let missing_model =
        crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
            "manifest-time",
            None,
            vec![snapshot.clone(), artifact.clone()],
        );
    let error = missing_model.require_passed().unwrap_err().to_string();
    assert!(error.contains("missing model provider canary evidence"));

    let duplicate_snapshot =
        crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
            "manifest-time",
            Some(model_provider.clone()),
            vec![snapshot.clone(), snapshot.clone(), artifact.clone()],
        );
    let error = duplicate_snapshot.require_passed().unwrap_err().to_string();
    assert!(error.contains("duplicate snapshot remote object-store canary evidence: 2"));

    let mut forged = loaded;
    forged.remote_object_stores.pop();
    forged.gate.valid = true;
    forged.gate.failures.clear();
    let error = forged.require_passed().unwrap_err().to_string();
    assert!(error.contains("stored canary evidence manifest gate does not match evidence"));
    assert!(error.contains("missing artifact remote object-store canary evidence"));
}

#[test]
fn public_canary_evidence_manifest_freshness_policy_rejects_stale_and_future_evidence() {
    let model_provider = passing_model_provider_canary_evidence_at("1000.000000000Z");
    let snapshot_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let artifact_client =
        Arc::new(crate::reviewer_kernel::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let mut snapshot = crate::operational_proof::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        snapshot_client.as_ref(),
    );
    let mut artifact = crate::operational_proof::canaries::run_remote_artifact_object_store_canary(
        "s3://muzen-test-artifacts/canary",
        artifact_client.as_ref(),
    );
    snapshot.generated_at_utc = "1000.000000000Z".to_string();
    artifact.generated_at_utc = "1000.000000000Z".to_string();
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "1000.000000000Z",
        Some(model_provider),
        vec![snapshot, artifact],
    );
    let fresh = crate::operational_proof::canaries::CanaryEvidenceFreshnessPolicy::at(
        "1100.000000000Z",
        120,
    );
    manifest
        .require_passed_with_freshness(&fresh)
        .expect("fresh manifest passed");

    let stale = crate::operational_proof::canaries::CanaryEvidenceFreshnessPolicy::at(
        "1300.000000000Z",
        120,
    );
    let error = manifest
        .require_passed_with_freshness(&stale)
        .unwrap_err()
        .to_string();
    assert!(error.contains("canary evidence manifest is stale"));
    assert!(error.contains("model provider canary evidence is stale"));
    assert!(error.contains("snapshot remote object-store canary evidence is stale"));
    assert!(error.contains("artifact remote object-store canary evidence is stale"));

    let future = crate::operational_proof::canaries::CanaryEvidenceFreshnessPolicy::at(
        "900.000000000Z",
        120,
    );
    let error = manifest
        .require_passed_with_freshness(&future)
        .unwrap_err()
        .to_string();
    assert!(error.contains("generatedAtUtc is in the future"));
}

#[test]
fn public_canary_evidence_status_report_separates_gate_and_freshness_failures() {
    let model_provider = passing_model_provider_canary_evidence_at("1000.000000000Z");
    let snapshot_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let mut snapshot = crate::operational_proof::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        snapshot_client.as_ref(),
    );
    snapshot.generated_at_utc = "1000.000000000Z".to_string();
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "1000.000000000Z",
        Some(model_provider),
        vec![snapshot],
    );

    let report = manifest.status_report(
        &crate::operational_proof::canaries::CanaryEvidenceFreshnessPolicy::at(
            "1300.000000000Z",
            120,
        ),
    );

    assert!(!report.ok);
    assert_eq!(
        report.manifest_schema_version,
        crate::operational_proof::canaries::CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION
    );
    assert_eq!(report.generated_at_utc, "1000.000000000Z");
    assert_eq!(report.freshness_checked_at_utc, "1300.000000000Z");
    assert_eq!(report.max_evidence_age_seconds, 120);
    assert!(report
        .validation_failures
        .iter()
        .any(|failure| failure.contains("missing artifact remote object-store canary evidence")));
    assert!(report
        .freshness_failures
        .iter()
        .any(|failure| failure.contains("canary evidence manifest is stale")));
    let failures = report.failures();
    assert!(failures
        .iter()
        .any(|failure| failure.contains("missing artifact remote object-store canary evidence")));
    assert!(failures
        .iter()
        .any(|failure| failure.contains("model provider canary evidence is stale")));
}

#[test]
fn operational_proof_manifest_composes_and_gates_evidence_files() {
    let evidence_dir = tempfile::tempdir().unwrap();
    let provider_path = evidence_dir.path().join("provider.json");
    let snapshot_path = evidence_dir.path().join("snapshot.json");
    let artifact_path = evidence_dir.path().join("artifact.json");
    let manifest_path = evidence_dir.path().join("manifest.json");
    let invalid_manifest_path = evidence_dir.path().join("invalid-manifest.json");
    let provider = passing_model_provider_canary_evidence();
    crate::operational_proof::canaries::export_model_provider_canary_evidence(
        &provider_path,
        &provider,
    )
    .unwrap();

    let snapshot_client =
        Arc::new(crate::reviewer_kernel::snapshots::InMemoryRemoteSnapshotObjectClient::default());
    let artifact_client =
        Arc::new(crate::reviewer_kernel::artifacts::InMemoryRemoteArtifactObjectClient::default());
    let snapshot = crate::operational_proof::canaries::run_remote_snapshot_object_store_canary(
        "s3://muzen-test-snapshots/canary",
        snapshot_client.as_ref(),
    );
    let artifact = crate::operational_proof::canaries::run_remote_artifact_object_store_canary(
        "s3://muzen-test-artifacts/canary",
        artifact_client.as_ref(),
    );
    crate::operational_proof::canaries::export_remote_object_store_canary_evidence(
        &snapshot_path,
        &snapshot,
    )
    .unwrap();
    crate::operational_proof::canaries::export_remote_object_store_canary_evidence(
        &artifact_path,
        &artifact,
    )
    .unwrap();

    let code = crate::operational_proof::run_manifest(ProofManifestArgs {
        provider_evidence: provider_path.clone(),
        remote_object_store_evidence: vec![snapshot_path.clone(), artifact_path.clone()],
        output: Some(manifest_path.clone()),
        max_evidence_age_seconds: 86_400,
    })
    .unwrap();
    assert_eq!(code, 0);
    let loaded = crate::operational_proof::canaries::load_canary_evidence_manifest(&manifest_path)
        .expect("load manifest");
    loaded.require_passed().expect("manifest passed");

    let error = crate::operational_proof::run_manifest(ProofManifestArgs {
        provider_evidence: provider_path,
        remote_object_store_evidence: vec![snapshot_path],
        output: Some(invalid_manifest_path.clone()),
        max_evidence_age_seconds: 86_400,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing artifact remote object-store canary evidence"));
    let invalid =
        crate::operational_proof::canaries::load_canary_evidence_manifest(&invalid_manifest_path)
            .expect("load invalid manifest");
    assert!(!invalid.gate.valid);
}

#[test]
fn operational_proof_verify_gates_published_manifest_files() {
    let evidence_dir = tempfile::tempdir().unwrap();
    let manifest_path = evidence_dir.path().join("manifest.json");
    let stale_manifest_path = evidence_dir.path().join("stale-manifest.json");
    let fresh_manifest = current_passing_canary_manifest();
    crate::operational_proof::canaries::export_canary_evidence_manifest(
        &manifest_path,
        &fresh_manifest,
    )
    .unwrap();

    let code = crate::operational_proof::run_verify(ProofVerifyArgs {
        manifest: manifest_path,
        max_evidence_age_seconds: 86_400,
    })
    .unwrap();
    assert_eq!(code, 0);

    let stale_manifest = passing_canary_manifest_at("1000.000000000Z");
    crate::operational_proof::canaries::export_canary_evidence_manifest(
        &stale_manifest_path,
        &stale_manifest,
    )
    .unwrap();
    let error = crate::operational_proof::run_verify(ProofVerifyArgs {
        manifest: stale_manifest_path,
        max_evidence_age_seconds: 1,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("canary evidence manifest is stale"));
}

#[test]
fn operational_proof_status_reports_published_manifest_state() {
    let evidence_dir = tempfile::tempdir().unwrap();
    let manifest_path = evidence_dir.path().join("manifest.json");
    let status_path = evidence_dir.path().join("status.json");
    let stale_manifest_path = evidence_dir.path().join("stale-manifest.json");
    let stale_status_path = evidence_dir.path().join("stale-status.json");
    let fresh_manifest = current_passing_canary_manifest();
    crate::operational_proof::canaries::export_canary_evidence_manifest(
        &manifest_path,
        &fresh_manifest,
    )
    .unwrap();

    let code = crate::operational_proof::run_status(crate::operational_proof::ProofStatusArgs {
        manifest: manifest_path,
        output: Some(status_path.clone()),
        max_evidence_age_seconds: 86_400,
    })
    .unwrap();
    assert_eq!(code, 0);
    let status: crate::operational_proof::canaries::CanaryEvidenceStatusReport =
        serde_json::from_str(&fs::read_to_string(&status_path).unwrap()).unwrap();
    assert!(status.ok);
    assert!(status.evidence.model_provider.present);
    assert_eq!(
        status.evidence.model_provider.required_protocols,
        crate::operational_proof::canaries::openai_provider_canary_protocols().to_vec()
    );
    assert_eq!(
        status.evidence.model_provider.passed_protocols,
        crate::operational_proof::canaries::openai_provider_canary_protocols().to_vec()
    );
    let snapshot_status = status
        .evidence
        .remote_object_stores
        .iter()
        .find(|evidence| {
            evidence.target
                == crate::operational_proof::canaries::RemoteObjectStoreCanaryTarget::Snapshot
        })
        .expect("snapshot evidence status");
    assert_eq!(snapshot_status.evidence_count, 1);
    assert!(snapshot_status.gate.as_ref().expect("snapshot gate").valid);
    assert!(snapshot_status
        .base_uri
        .as_deref()
        .expect("snapshot base uri")
        .contains("snapshots"));
    let artifact_status = status
        .evidence
        .remote_object_stores
        .iter()
        .find(|evidence| {
            evidence.target
                == crate::operational_proof::canaries::RemoteObjectStoreCanaryTarget::Artifact
        })
        .expect("artifact evidence status");
    assert_eq!(artifact_status.evidence_count, 1);
    assert!(artifact_status.gate.as_ref().expect("artifact gate").valid);

    let stale_manifest = passing_canary_manifest_at("1000.000000000Z");
    crate::operational_proof::canaries::export_canary_evidence_manifest(
        &stale_manifest_path,
        &stale_manifest,
    )
    .unwrap();
    let error = crate::operational_proof::run_status(crate::operational_proof::ProofStatusArgs {
        manifest: stale_manifest_path,
        output: Some(stale_status_path.clone()),
        max_evidence_age_seconds: 1,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("canary evidence manifest status failed"));
    assert!(error.contains("canary evidence manifest is stale"));
    let stale_status: crate::operational_proof::canaries::CanaryEvidenceStatusReport =
        serde_json::from_str(&fs::read_to_string(&stale_status_path).unwrap()).unwrap();
    assert!(!stale_status.ok);
    assert!(stale_status
        .freshness_failures
        .iter()
        .any(|failure| failure.contains("canary evidence manifest is stale")));
}
