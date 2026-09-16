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
/// XDG ScreenCast session restore token. The compositor issues it after
/// the user approves screen sharing; presenting it on later captures
/// restores the same source without another OS dialog, until the user
/// revokes the grant in system settings.
pub const ACCOUNT_PORTAL_RESTORE_TOKEN: &str = "portal.screencast.restore_token";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret '{0}' not found")]
    NotFound(String),
    #[error("secret backend failed: {0}")]
    Backend(String),
}

/// Blocking secret contract: every method performs synchronous OS IPC
/// (Secret Service on Linux, Keychain on macOS, Credential Manager on
/// Windows). `KeyringStore` hops each call to a dedicated plain OS thread
/// internally, so it is safe from any caller — including Tokio async
/// workers and `spawn_blocking` closures, which both still count as
/// "inside" a runtime for the nested keyring D-Bus client. Async callers
/// should still prefer `tokio::task::spawn_blocking` so a slow or locked
/// keychain stalls the blocking pool instead of an executor worker.
/// Custom `SecretStore` implementations that block must do the same.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError>;
    fn delete(&self, account: &str) -> Result<(), SecretError>;
}

/// Platform keychain binding under one service namespace (`utsuwa`).
pub struct KeyringStore {
    service: String,
}

/// Run blocking keychain IPC on a plain OS thread.
///
/// The `keyring` crate's synchronous API drives D-Bus through a nested
/// Tokio runtime (`zbus::utils::block_on`), which panics with
/// `Cannot start a runtime from within a runtime` on any thread that
/// holds a Tokio runtime context — including `spawn_blocking` workers,
/// which still count as "inside" the runtime. A fresh OS thread never
/// holds such context, so the same call succeeds (or fails gracefully)
/// there. Secret operations are rare (once per turn / capture), so one
/// thread spawn each is negligible.
fn on_keyring_thread<F, R>(op: F) -> Result<R, SecretError>
where
    F: FnOnce() -> Result<R, SecretError> + Send + 'static,
    R: Send + 'static,
{
    std::thread::Builder::new()
        .name("utsuwa-keyring".to_string())
        .spawn(op)
        .map_err(|error| SecretError::Backend(format!("keyring thread: {error}")))?
        .join()
        .unwrap_or_else(|_| {
            Err(SecretError::Backend(
                "keyring operation panicked".to_string(),
            ))
        })
}

impl KeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Direct entry construction. Must run on a plain OS thread (see
    /// [`on_keyring_thread`]): `Entry::new` lazily initializes the
    /// credential store, which blocks on D-Bus. There is deliberately no
    /// same-thread `entry(&self)` helper: every public entry point hops
    /// threads first, so no caller can accidentally trip the
    /// nested-runtime panic.
    fn entry_direct(service: &str, account: &str) -> Result<keyring::Entry, SecretError> {
        if account.is_empty() || account.len() > 256 || account.contains('\0') {
            return Err(SecretError::Backend("invalid secret account".to_string()));
        }
        keyring::Entry::new(service, account).map_err(|e| SecretError::Backend(e.to_string()))
    }

    /// Probe whether the platform store actually works here (headless
    /// containers often have no secret service). The whole probe runs on
    /// a plain OS thread so it never trips the nested-runtime panic,
    /// returning `false` (memory fallback) instead of crashing the caller.
    pub fn probe(&self) -> bool {
        const PROBE_ACCOUNT: &str = "utsuwa.probe";
        let service = self.service.clone();
        on_keyring_thread(move || {
            let entry = match Self::entry_direct(&service, PROBE_ACCOUNT) {
                Err(_) => return Ok(false),
                Ok(entry) => entry,
            };
            if entry.set_password("probe").is_err() {
                return Ok(false);
            }
            let ok = entry.get_password().ok().as_deref() == Some("probe");
            let _ = entry.delete_credential();
            Ok(ok)
        })
        .unwrap_or(false)
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, SecretError> {
        let service = self.service.clone();
        let account = account.to_string();
        on_keyring_thread(move || {
            let entry = Self::entry_direct(&service, &account)?;
            match entry.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(SecretError::Backend(e.to_string())),
            }
        })
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), SecretError> {
        if secret.len() > 16 * 1024 {
            return Err(SecretError::Backend("secret exceeds 16 KiB".to_string()));
        }
        let service = self.service.clone();
        let account = account.to_string();
        let secret = secret.to_string();
        on_keyring_thread(move || {
            Self::entry_direct(&service, &account)?
                .set_password(&secret)
                .map_err(|e| SecretError::Backend(e.to_string()))
        })
    }

    fn delete(&self, account: &str) -> Result<(), SecretError> {
        let service = self.service.clone();
        let account = account.to_string();
        on_keyring_thread(move || {
            match Self::entry_direct(&service, &account)?.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(SecretError::Backend(e.to_string())),
            }
        })
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
        Ok(self
            .inner
            .lock()
            .map_err(|_| SecretError::Backend("lock failed".to_string()))?
            .get(account)
            .cloned())
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
/// in-process memory otherwise (warned, never silent). The probe hops to
/// a plain OS thread internally, so this is safe from any caller; the
/// `catch_unwind` stays as a last-resort guard that fails closed to
/// memory rather than crashing the host.
pub fn system(service: &str) -> Arc<dyn SecretStore> {
    let keychain = KeyringStore::new(service);
    let probe_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| keychain.probe()))
        .unwrap_or(false);
    if probe_ok {
        Arc::new(keychain)
    } else {
        tracing::warn!("no working OS keychain; secrets stay in process memory only");
        Arc::new(MemoryStore::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ACCOUNT_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique keyring account per test: the OS keychain is shared across
    /// test threads (and across workspace test binaries running in parallel),
    /// so a fixed account name turns set/get/delete round-trips into a race.
    fn unique_account(prefix: &str) -> String {
        let n = ACCOUNT_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}.{}.{n}", std::process::id())
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::default();
        assert_eq!(store.get("model.api_key").unwrap(), None);
        store.set("model.api_key", "sk-test").unwrap();
        assert_eq!(
            store.get("model.api_key").unwrap(),
            Some("sk-test".to_string())
        );
        store.delete("model.api_key").unwrap();
        assert_eq!(store.get("model.api_key").unwrap(), None);
        // Deleting a missing secret is a no-op, not an error.
        store.delete("model.api_key").unwrap();
    }

    #[test]
    fn system_always_returns_a_working_store() {
        let store = system("utsuwa-test");
        let account = unique_account("probe.account");
        store.set(&account, "v").unwrap();
        assert_eq!(store.get(&account).unwrap(), Some("v".to_string()));
        store.delete(&account).unwrap();
    }

    #[test]
    fn keyring_rejects_bad_accounts_without_touching_the_backend() {
        let store = KeyringStore::new("utsuwa-test");
        assert!(store.get("").is_err());
        assert!(store.get("has\0nul").is_err());
    }

    // The dangerous execution context: `system()` must never panic when
    // (mis)called from inside an async runtime. The keyring client blocks
    // on D-Bus through a nested runtime, which panics with `Cannot start
    // a runtime from within a runtime` on any thread holding a Tokio
    // context; the probe hops to a plain OS thread first, so it either
    // uses the real keychain or fails closed to a working memory store.
    // A regression test in `app-host` covers the same pattern through a
    // stand-in nested-runtime backend.
    #[tokio::test]
    async fn system_inside_async_runtime_falls_back_without_panicking() {
        // Direct call on the async worker (no spawn_blocking): this is
        // exactly the context that used to panic.
        let store = system("utsuwa-test");
        let account = unique_account("probe.account");
        store.set(&account, "v").unwrap();
        assert_eq!(store.get(&account).unwrap(), Some("v".to_string()));
        store.delete(&account).unwrap();
    }

    // Keyring reads/writes from inside an async runtime hop threads
    // internally instead of panicking. Hits the real keychain when one
    // exists; without one they fail closed with a `Backend` error.
    #[tokio::test]
    async fn keyring_ops_inside_async_runtime_do_not_panic() {
        let store = KeyringStore::new("utsuwa-test");
        // Must not panic regardless of backend availability.
        let probed = store.probe();
        if probed {
            let account = unique_account("test.runtime");
            store.set(&account, "s3cr3t").unwrap();
            assert_eq!(store.get(&account).unwrap(), Some("s3cr3t".to_string()));
            store.delete(&account).unwrap();
        } else {
            eprintln!("no working keychain; probe-only coverage");
        }
        // Invalid accounts fail fast without touching the backend.
        assert!(store.get("").is_err());
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
        let account = unique_account("test.roundtrip");
        store.set(&account, "s3cr3t").unwrap();
        assert_eq!(store.get(&account).unwrap(), Some("s3cr3t".to_string()));
        store.delete(&account).unwrap();
        assert_eq!(store.get(&account).unwrap(), None);
    }
}
