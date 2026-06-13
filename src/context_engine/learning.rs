use super::ContextLearning;

pub(crate) fn learning_is_expired(learning: &ContextLearning) -> bool {
    let Some(expires_at) = &learning.expires_at_utc else {
        return false;
    };
    let Ok(expires_at) = expires_at.parse::<u64>() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    expires_at <= now
}
