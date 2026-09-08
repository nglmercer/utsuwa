//! Durable agent memory (plan Phase 32): plain SQLite entries the agent
//! can remember and recall across sessions.
//!
//! Deliberately simple: keyword recall over text + tags, ranked by
//! importance then recency. No vector database (the plan defers semantic
//! retrieval). Secrets do not belong here — entries are recalled into
//! model context, so anything secret stored here leaks by design; the
//! API takes plain text and callers must keep credentials out.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// Longest storable entry: memory is a cue, not a filesystem.
pub const MAX_ENTRY_CHARS: usize = 4_000;
/// Most tags per entry.
pub const MAX_TAGS: usize = 8;
/// Longest single tag.
pub const MAX_TAG_CHARS: usize = 64;
/// Hard cap on recall results per call.
pub const MAX_RECALL_LIMIT: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory store unavailable: {0}")]
    Store(String),
    #[error("invalid memory entry: {0}")]
    Invalid(String),
}

impl From<rusqlite::Error> for MemoryError {
    fn from(e: rusqlite::Error) -> Self {
        MemoryError::Store(e.to_string())
    }
}

/// One remembered fact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub text: String,
    pub tags: Vec<String>,
    pub importance: i64,
    pub created_ms: u64,
    pub updated_ms: u64,
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn row_to_entry(row: &rusqlite::Row) -> Result<MemoryEntry, rusqlite::Error> {
    let tags_json: String = row.get(2)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(MemoryEntry {
        id: row.get(0)?,
        text: row.get(1)?,
        tags,
        importance: row.get(3)?,
        created_ms: row.get::<_, i64>(4)? as u64,
        updated_ms: row.get::<_, i64>(5)? as u64,
    })
}

/// Thread-safe SQLite memory store. `Connection` is `Send` but not
/// `Sync`; the mutex makes shared cross-thread use sound.
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    fn init(conn: &Connection) -> Result<(), MemoryError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                text TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                importance INTEGER NOT NULL DEFAULT 0,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memory_importance ON memory_entries(importance DESC, created_ms DESC);",
        )?;
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Self, MemoryError> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MemoryError> {
        self.conn
            .lock()
            .map_err(|_| MemoryError::Store("memory store lock failed".to_string()))
    }

    fn check_text(text: &str) -> Result<String, MemoryError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MemoryError::Invalid("memory text must not be empty".to_string()));
        }
        if trimmed.chars().count() > MAX_ENTRY_CHARS {
            return Err(MemoryError::Invalid(format!(
                "memory text exceeds {MAX_ENTRY_CHARS} chars"
            )));
        }
        Ok(trimmed.to_string())
    }

    fn check_tags(tags: &[String]) -> Result<Vec<String>, MemoryError> {
        if tags.len() > MAX_TAGS {
            return Err(MemoryError::Invalid(format!("at most {MAX_TAGS} tags")));
        }
        let mut out = Vec::with_capacity(tags.len());
        for tag in tags {
            let trimmed = tag.trim();
            if trimmed.is_empty() {
                return Err(MemoryError::Invalid("tags must not be empty".to_string()));
            }
            if trimmed.chars().count() > MAX_TAG_CHARS {
                return Err(MemoryError::Invalid(format!(
                    "tag exceeds {MAX_TAG_CHARS} chars"
                )));
            }
            if !out.contains(&trimmed.to_string()) {
                out.push(trimmed.to_string());
            }
        }
        Ok(out)
    }

    /// Remember one fact. Returns the entry id.
    pub fn remember(
        &self,
        text: &str,
        tags: &[String],
        importance: i64,
    ) -> Result<i64, MemoryError> {
        let text = Self::check_text(text)?;
        let tags = Self::check_tags(tags)?;
        let importance = importance.clamp(0, 10);
        let now = unix_millis() as i64;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO memory_entries (text, tags, importance, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![text, serde_json::to_string(&tags).unwrap_or_default(), importance, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Recall entries matching every query term (case-insensitive
    /// substring over text and tags). Ranked by importance, then recency.
    /// An empty query returns recent entries — the "what do I know" path.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let limit = limit.clamp(1, MAX_RECALL_LIMIT) as i64;
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let conn = self.lock()?;
        // LIKE with ESCAPE: user terms may contain % _ and backslash.
        let mut sql = String::from(
            "SELECT id, text, tags, importance, created_ms, updated_ms
             FROM memory_entries",
        );
        let mut args: Vec<String> = Vec::new();
        if !terms.is_empty() {
            sql.push_str(" WHERE ");
            let clauses: Vec<String> = terms
                .iter()
                .map(|_| "(LOWER(text) LIKE ? ESCAPE '\\' OR LOWER(tags) LIKE ? ESCAPE '\\')".to_string())
                .collect();
            sql.push_str(&clauses.join(" AND "));
            for term in &terms {
                let pattern = format!("%{}%", escape_like(term));
                args.push(pattern.clone());
                args.push(pattern);
            }
        }
        // `id` breaks same-millisecond ties: AUTOINCREMENT is monotonic,
        // so larger ids are strictly newer.
        sql.push_str(" ORDER BY importance DESC, created_ms DESC, id DESC LIMIT ?");
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = args
            .iter()
            .map(|a| a as &dyn rusqlite::ToSql)
            .chain(std::iter::once(&limit as &dyn rusqlite::ToSql))
            .collect();
        let entries = stmt
            .query_map(params.as_slice(), row_to_entry)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// Forget one entry. Returns false when the id never existed.
    pub fn forget(&self, id: i64) -> Result<bool, MemoryError> {
        let conn = self.lock()?;
        let changed = conn.execute("DELETE FROM memory_entries WHERE id = ?1", params![id])?;
        Ok(changed == 1)
    }

    /// Update an entry's text, tags, or importance. `None` keeps the
    /// current value. Returns false when the id never existed.
    pub fn update(
        &self,
        id: i64,
        text: Option<&str>,
        tags: Option<&[String]>,
        importance: Option<i64>,
    ) -> Result<bool, MemoryError> {
        let conn = self.lock()?;
        let current: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT text, tags, importance FROM memory_entries WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((cur_text, cur_tags, cur_importance)) = current else {
            return Ok(false);
        };
        let text = match text {
            Some(t) => Self::check_text(t)?,
            None => cur_text,
        };
        let tags = match tags {
            Some(t) => serde_json::to_string(&Self::check_tags(t)?).unwrap_or_default(),
            None => cur_tags,
        };
        let importance = importance.map(|i| i.clamp(0, 10)).unwrap_or(cur_importance);
        let changed = conn.execute(
            "UPDATE memory_entries SET text = ?1, tags = ?2, importance = ?3, updated_ms = ?4
             WHERE id = ?5",
            params![text, tags, importance, unix_millis() as i64, id],
        )?;
        Ok(changed == 1)
    }

    pub fn count(&self) -> Result<i64, MemoryError> {
        let conn = self.lock()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM memory_entries", [], |row| row.get(0))?)
    }
}

/// Agent-facing memory tools (`memory.remember` / `memory.recall` /
/// `memory.forget`). Pure tools (`required_capability` is `None`): the
/// notebook lives in the host's own state dir, which the host already
/// owns — no OS authority is exercised, so there is nothing to ticket.
/// Reads and writes are still audit-logged as `Executed` like any tool.
pub mod tools {
    use super::{MemoryError, MemoryStore, MAX_ENTRY_CHARS, MAX_RECALL_LIMIT};
    use std::sync::Arc;
    use tool_core::{Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};

    fn invalid(message: String) -> ToolError {
        ToolError::InvalidArgs {
            tool: "memory".to_string(),
            message,
        }
    }

    fn failed(message: String) -> ToolError {
        ToolError::Failed {
            tool: "memory".to_string(),
            message,
        }
    }

    fn store_error(e: MemoryError) -> ToolError {
        failed(e.to_string())
    }

    /// Remember one fact for later turns and sessions.
    pub struct RememberTool {
        pub store: Arc<MemoryStore>,
    }

    #[async_trait::async_trait]
    impl Tool for RememberTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("memory.remember"),
                description: "Remember a fact for later turns and sessions. Keep it short; never store secrets or credentials here — entries are recalled into model context.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "importance": { "type": "integer", "minimum": 0, "maximum": 10 },
                    },
                    "required": ["text"],
                }),
                effects: vec![],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("missing string 'text'".to_string()))?;
            let tags: Vec<String> = args
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|v| {
                            v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                invalid("every 'tags' entry must be a string".to_string())
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let importance = args
                .get("importance")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let id = self
                .store
                .remember(text, &tags, importance)
                .map_err(|e| match e {
                    MemoryError::Invalid(message) => invalid(message),
                    other => store_error(other),
                })?;
            Ok(ToolOutput::new(serde_json::json!({ "id": id })))
        }
    }

    /// Recall remembered facts by keyword.
    pub struct RecallTool {
        pub store: Arc<MemoryStore>,
    }

    #[async_trait::async_trait]
    impl Tool for RecallTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("memory.recall"),
                description: "Recall remembered facts by keyword. Empty query lists recent entries.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RECALL_LIMIT as i64 },
                    },
                }),
                effects: vec![],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if query.chars().count() > MAX_ENTRY_CHARS {
                return Err(invalid("query is too long".to_string()));
            }
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .clamp(1, MAX_RECALL_LIMIT as u64) as usize;
            let entries = self.store.recall(query, limit).map_err(store_error)?;
            Ok(ToolOutput::new(serde_json::json!({
                "entries": entries
                    .iter()
                    .map(|e| serde_json::json!({
                        "id": e.id,
                        "text": e.text,
                        "tags": e.tags,
                        "importance": e.importance,
                    }))
                    .collect::<Vec<_>>(),
            })))
        }
    }

    /// Forget one remembered fact by id.
    pub struct ForgetTool {
        pub store: Arc<MemoryStore>,
    }

    #[async_trait::async_trait]
    impl Tool for ForgetTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("memory.forget"),
                description: "Forget one remembered fact by id.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"],
                }),
                effects: vec![],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let id = args
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| invalid("missing integer 'id'".to_string()))?;
            let forgotten = self.store.forget(id).map_err(store_error)?;
            Ok(ToolOutput::new(serde_json::json!({ "forgotten": forgotten })))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use capability_core::Principal;

        fn store() -> Arc<MemoryStore> {
            Arc::new(MemoryStore::open_in_memory().unwrap())
        }

        fn ctx() -> ToolContext {
            ToolContext::new(Principal::User)
        }

        #[tokio::test]
        async fn remember_recall_forget_roundtrip() {
            let store = store();
            let remember = RememberTool { store: store.clone() };
            let out = remember
                .invoke(
                    ctx(),
                    serde_json::json!({"text": "  prefers dark mode  ", "tags": ["ui"], "importance": 6}),
                )
                .await
                .unwrap();
            // Text is trimmed before storing.
            assert_eq!(out.content["id"].as_i64().unwrap(), 1);

            let recall = RecallTool { store: store.clone() };
            let out = recall
                .invoke(ctx(), serde_json::json!({"query": "dark"}))
                .await
                .unwrap();
            let entries = out.content["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["text"], "prefers dark mode");
            assert_eq!(entries[0]["importance"], 6);

            // Pure tools: no ticket required.
            assert!(remember.required_capability(&serde_json::json!({})).is_none());

            let forget = ForgetTool { store };
            let out = forget
                .invoke(ctx(), serde_json::json!({"id": 1}))
                .await
                .unwrap();
            assert_eq!(out.content["forgotten"], true);
            let out = forget
                .invoke(ctx(), serde_json::json!({"id": 1}))
                .await
                .unwrap();
            assert_eq!(out.content["forgotten"], false);
        }

        #[tokio::test]
        async fn invalid_args_rejected() {
            let store = store();
            let remember = RememberTool { store };
            assert!(remember.invoke(ctx(), serde_json::json!({})).await.is_err());
            assert!(remember
                .invoke(ctx(), serde_json::json!({"text": "   "}))
                .await
                .is_err());
            assert!(remember
                .invoke(ctx(), serde_json::json!({"text": "x", "tags": [42]}))
                .await
                .is_err());
        }
    }
}

fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryStore {
        MemoryStore::open_in_memory().unwrap()
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn remember_and_recall_by_keyword() {
        let db = store();
        let id = db
            .remember("Hana prefers concise summaries", &tags(&["user", "style"]), 5)
            .unwrap();
        assert!(id > 0);
        db.remember("Deploy checklist lives in /ops/runbook.md", &tags(&["ops"]), 3)
            .unwrap();

        // Case-insensitive substring over text and tags.
        let hits = db.recall("HANA", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "Hana prefers concise summaries");
        assert_eq!(hits[0].tags, tags(&["user", "style"]));
        assert_eq!(hits[0].importance, 5);

        let hits = db.recall("ops", 10).unwrap();
        assert_eq!(hits.len(), 1);

        // Every term must match (AND).
        assert!(db.recall("hana ops", 10).unwrap().is_empty());
        // No match, no rows.
        assert!(db.recall("nonexistent-thing", 10).unwrap().is_empty());
    }

    #[test]
    fn recall_ranks_importance_then_recency() {
        let db = store();
        db.remember("older important", &[], 9).unwrap();
        db.remember("newer trivial", &[], 1).unwrap();
        db.remember("newer important", &[], 9).unwrap();

        let hits = db.recall("", 10).unwrap();
        assert_eq!(hits.len(), 3);
        // Importance first, then newest first within a tier.
        assert_eq!(hits[0].text, "newer important");
        assert_eq!(hits[1].text, "older important");
        assert_eq!(hits[2].text, "newer trivial");
    }

    #[test]
    fn like_wildcards_are_literal() {
        let db = store();
        db.remember("100% coverage_ guaranteed", &[], 0).unwrap();
        db.remember("something else entirely", &[], 0).unwrap();
        // % and _ in the query must not act as wildcards.
        assert_eq!(db.recall("100% coverage_", 10).unwrap().len(), 1);
        assert!(db.recall("coverag", 10).unwrap().len() == 1);
    }

    #[test]
    fn validation_rejects_empty_oversize_and_bad_tags() {
        let db = store();
        assert!(db.remember("   ", &[], 0).is_err());
        assert!(db.remember(&"x".repeat(MAX_ENTRY_CHARS + 1), &[], 0).is_err());
        assert!(db.remember("ok", &["".to_string()], 0).is_err());
        assert!(db
            .remember("ok", &vec!["t".to_string(); MAX_TAGS + 1], 0)
            .is_err());
        assert_eq!(db.count().unwrap(), 0);
        // Importance clamps to 0..=10.
        let id = db.remember("clamped", &[], 99).unwrap();
        assert_eq!(db.recall("clamped", 1).unwrap()[0].importance, 10);
        let _ = id;
    }

    #[test]
    fn forget_and_update() {
        let db = store();
        let id = db.remember("draft", &tags(&["w"]), 2).unwrap();
        assert!(!db.forget(id + 999).unwrap());
        assert!(db.update(id, Some("final"), None, Some(7)).unwrap());
        let hits = db.recall("final", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tags, tags(&["w"]));
        assert_eq!(hits[0].importance, 7);
        assert!(db.forget(id).unwrap());
        assert_eq!(db.count().unwrap(), 0);
        assert!(!db.update(id, Some("gone"), None, None).unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("utsuwa-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.db");
        let id = MemoryStore::open(&path)
            .unwrap()
            .remember("across restarts", &tags(&["durable"]), 4)
            .unwrap();
        let reopened = MemoryStore::open(&path).unwrap();
        let hits = reopened.recall("restarts", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].tags, tags(&["durable"]));
        std::fs::remove_dir_all(&dir).ok();
    }
}
