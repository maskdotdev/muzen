use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCapturePolicy {
    pub max_captured_text_bytes: usize,
}

impl SnapshotCapturePolicy {
    pub fn new(max_captured_text_bytes: usize) -> Self {
        Self {
            max_captured_text_bytes,
        }
    }
}

impl Default for SnapshotCapturePolicy {
    fn default() -> Self {
        Self::new(64 * 1024 * 1024)
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCaptureStatus {
    Captured,
    SkippedMemoryLimit,
    SkippedUnreadable,
    NotTextCandidate,
}
