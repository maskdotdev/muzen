use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

pub(crate) mod canaries;

use self::canaries::{
    export_canary_evidence_manifest, load_canary_evidence_manifest,
    load_model_provider_canary_evidence, load_remote_object_store_canary_evidence,
    CanaryEvidenceFreshnessPolicy, CanaryEvidenceManifest,
};

#[derive(Parser, Debug, Clone)]
pub(crate) struct ProofArgs {
    #[command(subcommand)]
    pub(crate) command: ProofCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ProofCommand {
    /// Compose and validate aggregate canary evidence.
    Manifest(ProofManifestArgs),
    /// Validate a previously published aggregate canary evidence manifest.
    Verify(ProofVerifyArgs),
    /// Print structured status for a previously published canary evidence manifest.
    Status(ProofStatusArgs),
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ProofManifestArgs {
    /// Schema-versioned ModelProviderCanaryEvidence JSON.
    #[arg(long)]
    pub(crate) provider_evidence: PathBuf,

    /// Schema-versioned RemoteObjectStoreCanaryEvidence JSON. Pass once for snapshot and once for artifact evidence.
    #[arg(long = "remote-object-store-evidence", required = true)]
    pub(crate) remote_object_store_evidence: Vec<PathBuf>,

    /// Write aggregate CanaryEvidenceManifest JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ProofVerifyArgs {
    /// Schema-versioned CanaryEvidenceManifest JSON.
    #[arg(long)]
    pub(crate) manifest: PathBuf,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct ProofStatusArgs {
    /// Schema-versioned CanaryEvidenceManifest JSON.
    #[arg(long)]
    pub(crate) manifest: PathBuf,

    /// Write structured status JSON to this path. Prints to stdout when omitted.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,

    /// Reject evidence older than this many seconds. Defaults to 24 hours.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) max_evidence_age_seconds: u64,
}

pub(crate) fn run_proof(args: ProofArgs) -> Result<i32> {
    match args.command {
        ProofCommand::Manifest(args) => run_manifest(args),
        ProofCommand::Verify(args) => run_verify(args),
        ProofCommand::Status(args) => run_status(args),
    }
}

pub(crate) fn run_manifest(args: ProofManifestArgs) -> Result<i32> {
    let provider = load_model_provider_canary_evidence(&args.provider_evidence)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut remote_evidence = Vec::with_capacity(args.remote_object_store_evidence.len());
    for path in &args.remote_object_store_evidence {
        remote_evidence.push(
            load_remote_object_store_canary_evidence(path)
                .map_err(|error| anyhow::anyhow!("{error}"))?,
        );
    }

    let manifest = CanaryEvidenceManifest::from_evidence(Some(provider), remote_evidence);
    if let Some(path) = &args.output {
        let export = export_canary_evidence_manifest(path, &manifest)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        eprintln!(
            "wrote canary evidence manifest to {} ({} bytes)",
            export.path.display(),
            export.bytes
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    }

    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    manifest
        .require_passed_with_freshness(&freshness)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(0)
}

pub(crate) fn run_verify(args: ProofVerifyArgs) -> Result<i32> {
    let manifest = load_canary_evidence_manifest(&args.manifest)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    manifest
        .require_passed_with_freshness(&freshness)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(0)
}

pub(crate) fn run_status(args: ProofStatusArgs) -> Result<i32> {
    let manifest = load_canary_evidence_manifest(&args.manifest)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let freshness = CanaryEvidenceFreshnessPolicy::current(args.max_evidence_age_seconds);
    let report = manifest.status_report(&freshness);
    if let Some(path) = &args.output {
        let status_bytes = write_status_report(path, &report)?;
        eprintln!(
            "wrote canary evidence status to {} ({} bytes)",
            path.display(),
            status_bytes
        );
    } else {
        let status_json = status_report_json(&report)?;
        print!("{}", String::from_utf8(status_json)?);
    }
    if report.ok {
        return Ok(0);
    }
    bail!(
        "canary evidence manifest status failed: {}",
        report.failures().join("; ")
    );
}

fn status_report_json(report: &canaries::CanaryEvidenceStatusReport) -> Result<Vec<u8>> {
    let mut status_json = serde_json::to_vec_pretty(report)?;
    status_json.push(b'\n');
    Ok(status_json)
}

fn write_status_report(
    path: &Path,
    report: &canaries::CanaryEvidenceStatusReport,
) -> Result<usize> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create proof status directory {}",
                parent.display()
            )
        })?;
    }
    let status_json = status_report_json(report)?;
    fs::write(path, &status_json)
        .with_context(|| format!("failed to write proof status {}", path.display()))?;
    Ok(status_json.len())
}
