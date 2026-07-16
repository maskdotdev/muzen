use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::agent_runtime::{IdempotencyKey, MuzenError, PutSecretInput, SecretRef};

/// Secret bytes returned by a credential resolver.
///
/// Deliberately omits `Debug`, `Display`, and serialization so credential
/// material cannot accidentally enter logs, errors, or runtime events.
pub struct ResolvedSecret(Vec<u8>);

impl ResolvedSecret {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Resolves opaque secret references at the model-provider boundary.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(&self, secret: &SecretRef) -> Option<ResolvedSecret>;
}

struct StoredSecret {
    value: Vec<u8>,
}

struct Replay {
    digest: blake3::Hash,
    secret: SecretRef,
}

#[derive(Default)]
struct SecretState {
    values: BTreeMap<SecretRef, StoredSecret>,
    replays: BTreeMap<IdempotencyKey, Replay>,
}

/// Process-local ephemeral secret storage.
///
/// This v1 adapter intentionally never writes secrets or replay metadata to
/// SQLite. Reopening a local runtime therefore makes old `SecretRef`s
/// unavailable, and later provider calls fail with `secret_unavailable`.
#[derive(Default)]
pub(crate) struct LocalSecretStore {
    state: Mutex<SecretState>,
}

impl fmt::Debug for LocalSecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalSecretStore(<redacted>)")
    }
}

impl LocalSecretStore {
    pub(crate) fn put(&self, input: PutSecretInput) -> Result<SecretRef, MuzenError> {
        let value = base64::engine::general_purpose::STANDARD
            .decode(input.value)
            .map_err(|_| MuzenError::invalid_input("secret value must be valid padded base64"))?;
        let digest = blake3::hash(&value);
        let mut state = self.state.lock();
        if let Some(key) = input.idempotency_key.as_ref() {
            if let Some(replay) = state.replays.get(key) {
                return if replay.digest == digest {
                    Ok(replay.secret.clone())
                } else {
                    Err(MuzenError::conflict(
                        "idempotency key was already used with a different secret value",
                    ))
                };
            }
        }
        let secret = SecretRef::new(format!("secret_{}", Uuid::new_v4().simple()))
            .map_err(MuzenError::internal)?;
        state.values.insert(secret.clone(), StoredSecret { value });
        if let Some(key) = input.idempotency_key {
            state.replays.insert(
                key,
                Replay {
                    digest,
                    secret: secret.clone(),
                },
            );
        }
        Ok(secret)
    }

    pub(crate) fn delete(&self, secret: &SecretRef) {
        self.state.lock().values.remove(secret);
    }
}

#[async_trait]
impl CredentialResolver for LocalSecretStore {
    async fn resolve(&self, secret: &SecretRef) -> Option<ResolvedSecret> {
        self.state
            .lock()
            .values
            .get(secret)
            .map(|stored| ResolvedSecret(stored.value.clone()))
    }
}
