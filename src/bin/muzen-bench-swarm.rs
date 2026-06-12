//! Concurrent-vs-sequential swarm benchmark over the public reviewer facade.
//!
//! Uses a deterministic latency-injecting mock model, so it runs without
//! network access or API keys and is safe for CI. Prints a markdown summary;
//! redirect stdout to bench/results-concurrent-compare/summary.md to record
//! results.
//!
//! Usage: cargo run --release --bin muzen-bench-swarm

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use muzen::reviewer::adapters::runtime::RuntimeResult;
use muzen::reviewer::adapters::{Cancellation, TokenUsage};
use muzen::reviewer::model::{ReviewModel, ReviewModelRequest, ReviewModelTurn};
use muzen::reviewer::run::Run;
use muzen::reviewer::snapshots::{ChangeSpec, ChangedFileSpec, SnapshotSpec};
use muzen::reviewer::spec::{ReviewRunLimits, RunSpec};

const MODEL_LATENCY_MS: u64 = 25;
const FILES_PER_UNIT: usize = 4;
const UNIT_LADDER: [usize; 3] = [10, 50, 100];
const CONCURRENT_MAX_ACTIVE: usize = 16;

/// Simulates provider latency, then reports a clean unit result.
struct LatencyModel {
    latency: Duration,
}

#[async_trait]
impl ReviewModel for LatencyModel {
    async fn complete_review(
        &self,
        _request: ReviewModelRequest,
        _cancel: Cancellation,
    ) -> RuntimeResult<ReviewModelTurn> {
        tokio::time::sleep(self.latency).await;
        Ok(ReviewModelTurn::Text {
            content: serde_json::json!({
                "summary": "clean",
                "fileVerdicts": [],
                "findings": []
            })
            .to_string(),
            usage: TokenUsage::default(),
        })
    }
}

struct CaseResult {
    elapsed_ms: u64,
    sessions: usize,
    completed_sessions: usize,
    model_calls: usize,
}

async fn run_case(units: usize, max_active: usize) -> CaseResult {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("src")).expect("create src dir");
    let mut changed_files = Vec::new();
    for index in 0..units * FILES_PER_UNIT {
        let path = format!("src/file_{index}.rs");
        std::fs::write(
            temp.path().join(&path),
            format!("fn handler_{index}() -> usize {{ {index} }}\n"),
        )
        .expect("write bench file");
        changed_files.push(ChangedFileSpec::modified(&path));
    }
    let snapshot = SnapshotSpec::new(
        temp.path(),
        ChangeSpec::local("bench-change", "head", changed_files),
    );
    let spec = RunSpec::single_snapshot(
        format!("bench-swarm-{units}x{max_active}"),
        snapshot,
        Vec::new(),
        ReviewRunLimits::standard(max_active, 64 * 1024, 50),
    );
    let run = Run::builder(spec)
        .review_model(Arc::new(LatencyModel {
            latency: Duration::from_millis(MODEL_LATENCY_MS),
        }))
        .build()
        .expect("build run");
    let started = Instant::now();
    let report = run.execute().await;
    CaseResult {
        elapsed_ms: started.elapsed().as_millis().max(1) as u64,
        sessions: report.summary.sessions,
        completed_sessions: report.summary.completed_sessions,
        model_calls: report.summary.model_calls,
    }
}

#[tokio::main]
async fn main() {
    println!("# Concurrent vs sequential swarm benchmark");
    println!();
    println!(
        "Mock model latency: {MODEL_LATENCY_MS}ms per call. {FILES_PER_UNIT} files per unit. \
         Sequential = max_active_sessions 1; concurrent = max_active_sessions \
         {CONCURRENT_MAX_ACTIVE}."
    );
    println!();
    println!(
        "| Units | Sessions | Model calls | Sync ms | Concurrent ms | Speedup | Completed |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for units in UNIT_LADDER {
        let sync = run_case(units, 1).await;
        let concurrent = run_case(units, CONCURRENT_MAX_ACTIVE).await;
        assert_eq!(
            sync.completed_sessions, sync.sessions,
            "sequential run left incomplete sessions"
        );
        assert_eq!(
            concurrent.completed_sessions, concurrent.sessions,
            "concurrent run left incomplete sessions"
        );
        println!(
            "| {} | {} | {} | {} | {} | {:.2}x | {}/{} |",
            units,
            concurrent.sessions,
            concurrent.model_calls,
            sync.elapsed_ms,
            concurrent.elapsed_ms,
            sync.elapsed_ms as f64 / concurrent.elapsed_ms as f64,
            concurrent.completed_sessions,
            concurrent.sessions,
        );
    }
}
