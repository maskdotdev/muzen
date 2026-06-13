use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

pub(crate) const SCHEMA_VERSION: &str = "heimdaal.review-run.v1";
pub(crate) const DEFAULT_MODEL: &str = "gpt-4.1-nano";

pub(crate) fn timestamp_utc() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    format!("{}.{:09}Z", now.as_secs(), now.subsec_nanos())
}

pub(crate) fn redact_known_secrets(text: &str, secrets: &[&str]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    redacted
}

pub(crate) fn resolve_credential_ref(ref_name: &str) -> Result<String> {
    if let Some(name) = ref_name.strip_prefix("env:") {
        return env::var(name).with_context(|| format!("{name} is required"));
    }
    bail!("unsupported credentialRef; MVP supports env:NAME refs only")
}

#[cfg(test)]
mod tests {
    use super::resolve_credential_ref;

    #[test]
    fn openai_credential_ref_uses_exact_env_name() {
        let saved_openai = std::env::var("OPENAI_API_KEY").ok();
        let saved_oai = std::env::var("OAI_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", "sk-openai");
        std::env::set_var("OAI_API_KEY", "sk-oai");

        assert_eq!(
            resolve_credential_ref("env:OPENAI_API_KEY").expect("credential should resolve"),
            "sk-openai"
        );

        std::env::remove_var("OPENAI_API_KEY");
        assert!(resolve_credential_ref("env:OPENAI_API_KEY").is_err());

        restore_env("OPENAI_API_KEY", saved_openai);
        restore_env("OAI_API_KEY", saved_oai);
    }

    fn restore_env(name: &str, value: Option<String>) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
