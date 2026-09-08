//! Secret storage (plan Phase 33): API keys and credentials live in the
//! OS keychain, never in frontend storage and never in the SQLite
//! settings database.
//!
//! [`SecretStore`] is the abstraction; [`KeyringStore`] binds it to the
//! platform credential store, [`MemoryStore`] keeps secrets in-process
//! (tests, and hosts with no usable keychain — loudly logged, never
//! silent). [`system`] picks the keychain when it works.
//!
//! Account naming: dotted paths like `model.api_key`,
//! `mcp.<server>.auth`, `plugin.<id>.oauth`. There is deliberately no
//! list/iteration API — nothing may dump all secrets at once, so neither
//! the model context nor a plugin can sweep them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Well-known accounts. Services must request exactly the account they
/// need; the store never hands out more.
pub const ACCOUNT_MODEL_API_KEY: &str = "model.api_key";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret '{0}' not found")]
    NotFound(String),
    #[error("secret backend failed: {0}")]
    Backend(String),
}

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError>;
    fn delete(&self, account: &str) -> Result<(), SecretError>;
}

/// Platform keychain binding under one service namespace (`utsuwa`).
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, SecretError> {
        if account.is_empty() || account.len() > 256 || account.contains('\0') {
            return Err(SecretError::Backend("invalid secret account".to_string()));
        }
        keyring::Entry::new(&self.service, account)
            .map_err(|e| SecretError::Backend(e.to_string()))
    }

    /// Probe whether the platform store actually works here (headless
    /// containers often have no secret service).
    pub fn probe(&self) -> bool {
        const PROBE_ACCOUNT: &str = "utsuwa.probe";
        match self.entry(PROBE_ACCOUNT) {
            Err(_) => false,
            Ok(entry) => {
                if entry.set_password("probe").is_err() {
                    return false;
                }
                let ok = entry.get_password().ok().as_deref() == Some("probe");
                let _ = entry.delete_credential();
                ok
            }
        }
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        let entry = self.entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        if secret.len() > 16 * 1024 {
            return Err(SecretError::Backend("secret exceeds 16 KiB".to_string()));
        }
        self.entry(account)?
            .set_password(secret)
            .map_err(|e| SecretError::Backend(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<(), SecretError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

/// In-process secrets. Test seam, and last-resort fallback where no
/// platform store exists (secrets die with the process — by design, so a
/// missing keychain can never silently persist secrets somewhere weaker).
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        Ok(self.inner.lock().map_err(|_| SecretError::Backend("lock failed".to_string()))?.get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .map_err(|_| SecretError::Backend("lock failed".to_string()))?
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .map_err(|_| SecretError::Backend("lock failed".to_string()))?
            .remove(account);
        Ok(())
    }
}

/// The host secret store: platform keychain when it probes working,
/// in-process memory otherwise (warned, never silent).
pub fn system(service: &str) -> Arc<dyn SecretStore> {
    let keychain = KeyringStore::new(service);
    if keychain.probe() {
        Arc::new(keychain)
    } else {
        tracing::warn!("no working OS keychain; secrets stay in process memory only");
        Arc::new(MemoryStore::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::default();
        assert_eq!(store.get("model.api_key").unwrap(), None);
        store.set("model.api_key", "sk-test").unwrap();
        assert_eq!(store.get("model.api_key").unwrap(), Some("sk-test".to_string()));
        store.delete("model.api_key").unwrap();
        assert_eq!(store.get("model.api_key").unwrap(), None);
        // Deleting a missing secret is a no-op, not an error.
        store.delete("model.api_key").unwrap();
    }

    #[test]
    fn system_always_returns_a_working_store() {
        let store = system("utsuwa-test");
        store.set("probe.account", "v").unwrap();
        assert_eq!(store.get("probe.account").unwrap(), Some("v".to_string()));
        store.delete("probe.account").unwrap();
    }

    #[test]
    fn keyring_rejects_bad_accounts_without_touching_the_backend() {
        let store = KeyringStore::new("utsuwa-test");
        assert!(store.get("").is_err());
        assert!(store.get("has\0nul").is_err());
    }

    // Hits the real platform keychain when one exists; ignored otherwise
    // so headless CI stays green.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let store = KeyringStore::new("utsuwa-test");
        if !store.probe() {
            eprintln!("no working keychain; skipping");
            return;
        }
        store.set("test.roundtrip", "s3cr3t").unwrap();
        assert_eq!(
            store.get("test.roundtrip").unwrap(),
            Some("s3cr3t".to_string())
        );
        store.delete("test.roundtrip").unwrap();
        assert_eq!(store.get("test.roundtrip").unwrap(), None);
    }
}
