//! Persistent state: settings key-value store and persistent user grants.
//!
//! Backed by SQLite (`state.db`). Only [`GrantLifetime::Persistent`] grants
//! are stored here — `Once`/`Task`/`Session` grants live in memory and die
//! with the process, so a restart never resurrects narrower authority.
//!
//! Secrets are intentionally out of scope: API keys live in the OS keychain
//! (plan Phase 33); this crate stores only non-secret settings and grants.

use policy_core::{GrantLifetime, GrantedScope};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

const SCHEMA_VERSION: i32 = 2;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings value is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("only Persistent grants may be stored; got {0:?}")]
    NonPersistentGrant(GrantLifetime),
    #[error("grant id {0} not found")]
    GrantNotFound(i64),
}

/// SQLite-backed application state.
pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Open (creating parents and schema as needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StorageError> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS grants (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     principal_kind TEXT NOT NULL,
                     capability TEXT NOT NULL,
                     scope_json TEXT NOT NULL,
                     lifetime TEXT NOT NULL DEFAULT 'Persistent'
                 );",
            )?;
        }
        if version < 2 {
            // v1 grants did not retain their in-memory identity/lifetime
            // metadata. Existing rows are persistent by definition; use the
            // SQLite row id as a stable fallback identity.
            self.conn.execute_batch(
                "ALTER TABLE grants ADD COLUMN grant_id TEXT;
                 ALTER TABLE grants ADD COLUMN task_id TEXT;
                 ALTER TABLE grants ADD COLUMN turn_id TEXT;
                 ALTER TABLE grants ADD COLUMN created_at INTEGER;
                 ALTER TABLE grants ADD COLUMN expires_at INTEGER;
                 ALTER TABLE grants ADD COLUMN uses_remaining INTEGER;
                 UPDATE grants SET grant_id = CAST(id AS TEXT) WHERE grant_id IS NULL;",
            )?;
        }
        if version < SCHEMA_VERSION {
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }
        Ok(())
    }

    // -- settings --------------------------------------------------------

    /// Read a setting; `None` when the key was never written.
    pub fn get_setting(&self, key: &str) -> Result<Option<Value>, StorageError> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        raw.map(|s| serde_json::from_str(&s).map_err(StorageError::from))
            .transpose()
    }

    /// Write (insert or replace) a setting value as JSON.
    pub fn set_setting(&self, key: &str, value: &Value) -> Result<(), StorageError> {
        let raw = serde_json::to_string(value)?;
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, raw],
        )?;
        Ok(())
    }

    /// Delete a setting. Returns true when a row existed.
    pub fn delete_setting(&self, key: &str) -> Result<bool, StorageError> {
        let n = self
            .conn
            .execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(n > 0)
    }

    // -- grants ----------------------------------------------------------

    /// Persist a user-approved grant. Rejects non-persistent lifetimes:
    /// narrowing `Task`/`Session`/`Once` approvals must never survive a
    /// restart and widen future authority.
    pub fn save_grant(&self, grant: &GrantedScope) -> Result<i64, StorageError> {
        if grant.lifetime != GrantLifetime::Persistent {
            return Err(StorageError::NonPersistentGrant(grant.lifetime));
        }
        let kind = serde_json::to_string(&grant.principal_kind)?;
        let cap = serde_json::to_string(&grant.capability)?;
        let scope = serde_json::to_string(&grant.scope)?;
        self.conn.execute(
            "INSERT INTO grants
                (principal_kind, capability, scope_json, lifetime, grant_id,
                 task_id, turn_id, created_at, expires_at, uses_remaining)
             VALUES (?1, ?2, ?3, 'Persistent', ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                kind,
                cap,
                scope,
                grant.id,
                grant.task_id,
                grant.turn_id,
                grant.created_at as i64,
                grant.expires_at.map(|value| value as i64),
                grant.uses_remaining.map(|value| value as i64),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Load all stored persistent grants, oldest first.
    pub fn load_grants(&self) -> Result<Vec<StoredGrant>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, principal_kind, capability, scope_json, grant_id,
                    task_id, turn_id, created_at, expires_at, uses_remaining
             FROM grants
             WHERE lifetime = 'Persistent' ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let cap: String = row.get(2)?;
            let scope: String = row.get(3)?;
            let grant_id: Option<String> = row.get(4)?;
            let task_id: Option<String> = row.get(5)?;
            let turn_id: Option<String> = row.get(6)?;
            let created_at: Option<i64> = row.get(7)?;
            let expires_at: Option<i64> = row.get(8)?;
            let uses_remaining: Option<i64> = row.get(9)?;
            Ok((
                id,
                kind,
                cap,
                scope,
                grant_id,
                task_id,
                turn_id,
                created_at,
                expires_at,
                uses_remaining,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                cap,
                scope,
                grant_id,
                task_id,
                turn_id,
                created_at,
                expires_at,
                uses_remaining,
            ) = row?;
            let principal_kind = serde_json::from_str(&kind).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let capability = serde_json::from_str(&cap).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let scope = serde_json::from_str(&scope).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let mut grant = GrantedScope::new(
                principal_kind,
                capability,
                scope,
                GrantLifetime::Persistent,
                None,
                None,
            );
            // The SQLite row id remains the durable identity exposed by
            // StoredGrant; keep it stable in the in-memory grant as well.
            grant.id = grant_id.unwrap_or_else(|| id.to_string());
            grant.task_id = task_id;
            grant.turn_id = turn_id;
            grant.created_at = created_at.unwrap_or_default().max(0) as u64;
            grant.expires_at = expires_at.map(|value| value.max(0) as u64);
            grant.uses_remaining = uses_remaining.map(|value| value.max(0) as u32);
            out.push(StoredGrant { id, grant });
        }
        Ok(out)
    }

    /// Revoke a stored grant by row id.
    pub fn delete_grant(&self, id: i64) -> Result<(), StorageError> {
        let n = self
            .conn
            .execute("DELETE FROM grants WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(StorageError::GrantNotFound(id));
        }
        Ok(())
    }
}

/// A persistent grant with its storage row id (for revocation).
#[derive(Debug, Clone)]
pub struct StoredGrant {
    pub id: i64,
    pub grant: GrantedScope,
}

/// Resolve the platform state directory for `state.db` without extra deps:
/// XDG data home (or `~/.local/share`) on Linux, `~/Library/Application
/// Support` on macOS, `%APPDATA%` on Windows.
pub fn default_state_dir(app_name: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join(app_name);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library/Application Support")
                .join(app_name);
        }
    }
    if cfg!(target_os = "windows") {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join(app_name);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share").join(app_name);
    }
    PathBuf::from(".").join(app_name)
}

/// Full default path to `state.db` for the app.
pub fn default_db_path(app_name: &str) -> PathBuf {
    default_state_dir(app_name).join("state.db")
}

/// Build the [`policy_core::ApprovalQueue`] persistence hook around a
/// shared store: each newly approved `Persistent` grant is written to
/// SQLite before it takes effect. Failures surface as
/// [`policy_core::QueueError::Persist`] so the dialog can report them.
pub fn persistent_grant_hook(
    store: Arc<Mutex<Storage>>,
) -> Arc<dyn Fn(&GrantedScope) -> Result<(), String> + Send + Sync> {
    Arc::new(move |grant: &GrantedScope| {
        let store = store
            .lock()
            .map_err(|_| "storage lock failed".to_string())?;
        store
            .save_grant(grant)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}
