use super::prelude::*;
use super::support::*;

#[test]
fn public_canary_evidence_manifest_gates_provider_evidence() {
    let model_provider = passing_model_provider_canary_evidence();
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "manifest-time",
        Some(model_provider.clone()),
    );

    manifest.require_passed().expect("manifest passed");
    assert_eq!(
        manifest.schema_version,
        crate::operational_proof::canaries::CANARY_EVIDENCE_MANIFEST_SCHEMA_VERSION
    );
    assert_eq!(manifest.gate.passed, model_provider.gate.passed);
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
        );
    let error = missing_model.require_passed().unwrap_err().to_string();
    assert!(error.contains("missing model provider canary evidence"));
}

#[test]
fn public_canary_evidence_manifest_freshness_policy_rejects_stale_and_future_evidence() {
    let model_provider = passing_model_provider_canary_evidence_at("1000.000000000Z");
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "1000.000000000Z",
        Some(model_provider),
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
    let manifest = crate::operational_proof::canaries::CanaryEvidenceManifest::with_generated_at(
        "1000.000000000Z",
        Some(model_provider),
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
    assert!(report.validation_failures.is_empty());
    assert!(report
        .freshness_failures
        .iter()
        .any(|failure| failure.contains("canary evidence manifest is stale")));
    let failures = report.failures();
    assert!(failures
        .iter()
        .any(|failure| failure.contains("model provider canary evidence is stale")));
}

#[test]
fn operational_proof_manifest_composes_and_gates_provider_evidence_file() {
    let evidence_dir = tempfile::tempdir().unwrap();
    let provider_path = evidence_dir.path().join("provider.json");
    let manifest_path = evidence_dir.path().join("manifest.json");
    let provider = passing_model_provider_canary_evidence();
    crate::operational_proof::canaries::export_model_provider_canary_evidence(
        &provider_path,
        &provider,
    )
    .unwrap();

    let code =
        crate::operational_proof::run_manifest(crate::operational_proof::ProofManifestArgs {
            provider_evidence: provider_path,
            output: Some(manifest_path.clone()),
            max_evidence_age_seconds: 86_400,
        })
        .unwrap();
    assert_eq!(code, 0);
    let loaded = crate::operational_proof::canaries::load_canary_evidence_manifest(&manifest_path)
        .expect("load manifest");
    loaded.require_passed().expect("manifest passed");
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

    let code = crate::operational_proof::run_verify(crate::operational_proof::ProofVerifyArgs {
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
    let error = crate::operational_proof::run_verify(crate::operational_proof::ProofVerifyArgs {
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
