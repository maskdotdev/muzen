use crate::review_plan::{ReviewPlan, ReviewPlanFileMode};
use crate::runtime::contracts::RepoPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewUnitPlan {
    pub(crate) counts: ReviewUnitPlanCounts,
    pub(crate) units: Vec<PlannedReviewUnit>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReviewUnitPlanCounts {
    pub(crate) total_units: usize,
    pub(crate) full_units: usize,
    pub(crate) oversized_units: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedReviewUnit {
    pub(crate) id: String,
    pub(crate) file_paths: Vec<RepoPath>,
    pub(crate) score_min: u8,
    pub(crate) score_max: u8,
    pub(crate) estimated_bytes: u64,
    pub(crate) file_count: usize,
    pub(crate) requires_further_split: bool,
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct ReviewUnitOptions {
    pub(crate) max_files: usize,
    pub(crate) max_estimated_bytes: u64,
    pub(crate) isolate_score_at: u8,
}

impl Default for ReviewUnitOptions {
    fn default() -> Self {
        Self {
            max_files: 4,
            max_estimated_bytes: 80 * 1024,
            isolate_score_at: 80,
        }
    }
}

pub(crate) fn build_review_unit_plan(
    plan: &ReviewPlan,
    options: ReviewUnitOptions,
) -> ReviewUnitPlan {
    let mut files = plan
        .files
        .iter()
        .filter(|file| file.mode == ReviewPlanFileMode::Full)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.display().cmp(&right.path.display()))
    });

    let mut units = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0u64;
    for file in files {
        let file_bytes = file
            .estimated_bytes
            .unwrap_or(options.max_estimated_bytes)
            .max(1);
        let isolate = file.score >= options.isolate_score_at;
        if !current.is_empty()
            && (isolate
                || current.len() >= options.max_files.max(1)
                || current_bytes.saturating_add(file_bytes) > options.max_estimated_bytes.max(1))
        {
            units.push(finalize_unit(
                units.len() + 1,
                &current,
                current_bytes,
                options,
            ));
            current.clear();
            current_bytes = 0;
        }
        current.push(file);
        current_bytes = current_bytes.saturating_add(file_bytes);
        if isolate {
            units.push(finalize_unit(
                units.len() + 1,
                &current,
                current_bytes,
                options,
            ));
            current.clear();
            current_bytes = 0;
        }
    }
    if !current.is_empty() {
        units.push(finalize_unit(
            units.len() + 1,
            &current,
            current_bytes,
            options,
        ));
    }
    ReviewUnitPlan {
        counts: ReviewUnitPlanCounts {
            total_units: units.len(),
            full_units: units.len(),
            oversized_units: units
                .iter()
                .filter(|unit| unit.requires_further_split)
                .count(),
        },
        units,
    }
}

fn finalize_unit(
    index: usize,
    files: &[&crate::review_plan::PlannedReviewFile],
    estimated_bytes: u64,
    options: ReviewUnitOptions,
) -> PlannedReviewUnit {
    let score_min = files.iter().map(|file| file.score).min().unwrap_or(0);
    let score_max = files.iter().map(|file| file.score).max().unwrap_or(0);
    PlannedReviewUnit {
        id: format!("unit-{index:03}"),
        file_paths: files.iter().map(|file| file.path.clone()).collect(),
        score_min,
        score_max,
        estimated_bytes,
        file_count: files.len(),
        requires_further_split: files
            .iter()
            .any(|file| file.estimated_bytes.unwrap_or(0) > options.max_estimated_bytes),
    }
}
