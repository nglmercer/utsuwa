use crate::error::{TaskCoreError, TaskCoreResult};
use crate::model::{
    Ms, NewTask, Task, TaskEvent, TaskStatus, TaskStep, TaskStepStatus, TaskStepType,
    VerificationSpec,
};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const SCHEMA_VERSION: i32 = 1;

/// Durable task storage. All scheduling and execution state lives here so a
/// restart can recover running work instead of losing it.
#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create(&self, task: NewTask, now: Ms) -> TaskCoreResult<Task>;
    async fn get(&self, task_id: &str) -> TaskCoreResult<Option<Task>>;
    async fn update(&self, task: &Task, now: Ms) -> TaskCoreResult<()>;
    async fn delete(&self, task_id: &str) -> TaskCoreResult<bool>;
    async fn list(&self, status: Option<TaskStatus>, limit: i64) -> TaskCoreResult<Vec<Task>>;
    async fn count(&self, status: Option<TaskStatus>) -> TaskCoreResult<i64>;
    async fn ready_batch(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>>;
    async fn stale_running(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>>;
    async fn record_event(&self, event: TaskEvent) -> TaskCoreResult<()>;
    async fn list_events(&self, task_id: &str, limit: i64) -> TaskCoreResult<Vec<TaskEvent>>;
    async fn pending_wait_expired(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>>;
    async fn find_waiter(
        &self,
        event_type: &str,
        correlation_id: Option<&str>,
        now: Ms,
    ) -> TaskCoreResult<Option<Task>>;
}

#[derive(Debug)]
struct StoredTask {
    id: String,
    title: String,
    instruction: String,
    status: String,
    priority: i32,
    created_at: Ms,
    updated_at: Ms,
    scheduled_at: Option<Ms>,
    started_at: Option<Ms>,
    finished_at: Option<Ms>,
    attempts: i32,
    max_attempts: i32,
    lease_until: Option<Ms>,
    next_attempt_at: Option<Ms>,
    wait_for: Option<String>,
    current_step_index: i64,
    result: Option<String>,
    last_error: Option<String>,
    verification: Option<String>,
    parent_task_id: Option<String>,
}

struct StoredStep {
    id: String,
    step_type: String,
    status: String,
    input: String,
    result: Option<String>,
    attempts: i32,
    max_attempts: i32,
    started_at: Option<Ms>,
    finished_at: Option<Ms>,
    error: Option<String>,
}

fn materialize(conn: &Connection, stored: StoredTask) -> rusqlite::Result<Task> {
    let StoredTask {
        id,
        title,
        instruction,
        status,
        priority,
        created_at,
        updated_at,
        scheduled_at,
        started_at,
        finished_at,
        attempts,
        max_attempts,
        lease_until,
        next_attempt_at,
        wait_for,
        current_step_index,
        result,
        last_error,
        verification,
        parent_task_id,
    } = stored;
    let mut stmt = conn.prepare(
        "SELECT id, step_index, step_type, status, input, result, attempts, max_attempts, started_at, finished_at, error
         FROM task_steps WHERE task_id = ?1 ORDER BY step_index",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok(StoredStep {
            id: row.get(0)?,
            step_type: row.get(2)?,
            status: row.get(3)?,
            input: row.get(4)?,
            result: row.get(5)?,
            attempts: row.get(6)?,
            max_attempts: row.get(7)?,
            started_at: row.get(8)?,
            finished_at: row.get(9)?,
            error: row.get(10)?,
        })
    })?;
    let mut steps = Vec::new();
    for row in rows {
        let row = row?;
        steps.push(TaskStep {
            id: row.id,
            step_type: TaskStepType::parse(&row.step_type).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("unknown task_step_type: {}", row.step_type).into(),
                )
            })?,
            status: TaskStepStatus::parse(&row.status).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    format!("unknown task_step_status: {}", row.status).into(),
                )
            })?,
            input: serde_json::from_str(&row.input).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
            result: row
                .result
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            started_at: row.started_at,
            finished_at: row.finished_at,
            error: row.error,
        });
    }
    Ok(Task {
        id,
        title,
        instruction,
        status: TaskStatus::parse(&status).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("unknown task_status: {}", status).into(),
            )
        })?,
        priority,
        created_at,
        updated_at,
        scheduled_at,
        started_at,
        finished_at,
        attempts,
        max_attempts,
        lease_until,
        next_attempt_at,
        wait_for: wait_for
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        steps,
        current_step_index: usize::try_from(current_step_index.max(0)).unwrap_or(0),
        result: result
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        last_error: last_error
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    15,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        verification: verification
            .as_deref()
            .map(serde_json::from_str::<VerificationSpec>)
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    16,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        parent_task_id,
    })
}

/// Blocking SQLite store wrapped in an async trait. SQLite calls are fast
/// local operations; callers run the runtime on threads so this does not
/// need a dedicated blocking pool.
#[derive(Clone)]
pub struct SqliteTaskStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteTaskStore {
    pub fn open(path: &Path) -> TaskCoreResult<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> TaskCoreResult<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    pub fn from_connection(conn: Connection) -> TaskCoreResult<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> TaskCoreResult<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| TaskCoreError::Storage("task store lock poisoned".to_string()))?;
        f(&conn).map_err(TaskCoreError::from)
    }

    fn migrate(&self) -> TaskCoreResult<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS tasks (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    instruction TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    priority INTEGER NOT NULL DEFAULT 50,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    scheduled_at INTEGER,
                    started_at INTEGER,
                    finished_at INTEGER,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    max_attempts INTEGER NOT NULL DEFAULT 3,
                    lease_until INTEGER,
                    next_attempt_at INTEGER,
                    wait_for TEXT,
                    current_step_index INTEGER NOT NULL DEFAULT 0,
                    result TEXT,
                    last_error TEXT,
                    verification TEXT,
                    parent_task_id TEXT
                );
                CREATE TABLE IF NOT EXISTS task_steps (
                    id TEXT PRIMARY KEY,
                    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                    step_index INTEGER NOT NULL,
                    step_type TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    input TEXT NOT NULL DEFAULT '{}',
                    result TEXT,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    max_attempts INTEGER NOT NULL DEFAULT 1,
                    started_at INTEGER,
                    finished_at INTEGER,
                    error TEXT,
                    UNIQUE(task_id, step_index)
                );
                CREATE TABLE IF NOT EXISTS task_events (
                    id TEXT PRIMARY KEY,
                    event_type TEXT NOT NULL,
                    task_id TEXT,
                    step_id TEXT,
                    correlation_id TEXT,
                    payload TEXT NOT NULL DEFAULT '{}',
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
                CREATE INDEX IF NOT EXISTS idx_tasks_ready ON tasks(status, next_attempt_at, scheduled_at, priority DESC);
                CREATE INDEX IF NOT EXISTS idx_tasks_wait ON tasks(status, wait_for);
                CREATE INDEX IF NOT EXISTS idx_task_steps_task ON task_steps(task_id, step_index);
                CREATE INDEX IF NOT EXISTS idx_task_events_task ON task_events(task_id, created_at);",
            )?;
            let version: i32 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap_or(0);
            if version < SCHEMA_VERSION {
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            Ok(())
        })
    }

    fn read_task(conn: &Connection, task_id: &str) -> rusqlite::Result<Option<Task>> {
        let stored: Option<StoredTask> = conn
            .query_row(
                "SELECT id, title, instruction, status, priority, created_at, updated_at,
                        scheduled_at, started_at, finished_at, attempts, max_attempts,
                        lease_until, next_attempt_at, wait_for, current_step_index,
                        result, last_error, verification, parent_task_id
                 FROM tasks WHERE id = ?1",
                params![task_id],
                |row| {
                    Ok(StoredTask {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        instruction: row.get(2)?,
                        status: row.get(3)?,
                        priority: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                        scheduled_at: row.get(7)?,
                        started_at: row.get(8)?,
                        finished_at: row.get(9)?,
                        attempts: row.get(10)?,
                        max_attempts: row.get(11)?,
                        lease_until: row.get(12)?,
                        next_attempt_at: row.get(13)?,
                        wait_for: row.get(14)?,
                        current_step_index: row.get(15)?,
                        result: row.get(16)?,
                        last_error: row.get(17)?,
                        verification: row.get(18)?,
                        parent_task_id: row.get(19)?,
                    })
                },
            )
            .optional()?;
        stored.map(|stored| materialize(conn, stored)).transpose()
    }

    fn read_tasks(
        conn: &Connection,
        sql: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> rusqlite::Result<Vec<Task>> {
        let mut stmt = conn.prepare(sql)?;
        let ids: Vec<String> = stmt
            .query_map(args, |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(task) = Self::read_task(conn, &id)? {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }
}

#[async_trait]
impl TaskStore for SqliteTaskStore {
    async fn create(&self, task: NewTask, now: Ms) -> TaskCoreResult<Task> {
        if task.title.trim().is_empty() {
            return Err(TaskCoreError::Validation(
                "task title must not be empty".to_string(),
            ));
        }
        if task.steps.is_empty() {
            return Err(TaskCoreError::Validation(
                "task must contain at least one step".to_string(),
            ));
        }
        let status = if task.scheduled_at.is_some_and(|at| at > now) {
            TaskStatus::Scheduled
        } else {
            TaskStatus::Pending
        };
        let priority = task.priority.unwrap_or(crate::model::priority::NORMAL);
        let max_attempts = task.max_attempts.unwrap_or(3).max(1);
        let verification = task
            .verification
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        let steps: Vec<(String, TaskStepType, String, i32)> = task
            .steps
            .iter()
            .map(|step| {
                serde_json::to_string(&step.input)
                    .map_err(|err| TaskCoreError::Serialization(err.to_string()))
                    .map(|input| {
                        (
                            uuid::Uuid::new_v4().to_string(),
                            step.step_type,
                            input,
                            step.max_attempts.unwrap_or(1).max(1),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let task_id = uuid::Uuid::new_v4().to_string();
        let title = task.title.clone();
        let instruction = task.instruction.clone();
        let scheduled_at = task.scheduled_at;
        let parent_task_id = task.parent_task_id.clone();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO tasks (id, title, instruction, status, priority, created_at, updated_at,
                                    scheduled_at, attempts, max_attempts,
                                    current_step_index, verification, parent_task_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 0, ?8, 0, ?9, ?10)",
                params![
                    task_id,
                    title,
                    instruction,
                    status.as_str(),
                    priority,
                    now,
                    scheduled_at,
                    max_attempts,
                    verification,
                    parent_task_id,
                ],
            )?;
            for (index, step) in steps.iter().enumerate() {
                conn.execute(
                    "INSERT INTO task_steps (id, task_id, step_index, step_type, status, input,
                                            attempts, max_attempts)
                     VALUES (?1, ?2, ?3, ?4, 'pending', ?5, 0, ?6)",
                    params![step.0, task_id, index as i64, step.1.as_str(), step.2, step.3,],
                )?;
            }
            Ok(())
        })?;
        self.get(&task_id)
            .await?
            .ok_or_else(|| TaskCoreError::Storage("created task missing".to_string()))
    }

    async fn get(&self, task_id: &str) -> TaskCoreResult<Option<Task>> {
        self.with_conn(|conn| Self::read_task(conn, task_id))
    }

    async fn update(&self, task: &Task, now: Ms) -> TaskCoreResult<()> {
        let current = self
            .get(&task.id)
            .await?
            .ok_or_else(|| TaskCoreError::NotFound(task.id.clone()))?;
        if current.status != task.status && !current.status.can_transition_to(task.status) {
            return Err(TaskCoreError::IllegalTransition {
                task_id: task.id.clone(),
                from: current.status,
                to: task.status,
            });
        }
        let result = task
            .result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        let last_error = task
            .last_error
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        let verification = task
            .verification
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        let wait_for = task
            .wait_for
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        // Serialize every step field explicitly; verbose on purpose so a
        // schema drift shows up here as a compile error, not lost data.
        let mut serialized_steps = Vec::with_capacity(task.steps.len());
        for step in &task.steps {
            serialized_steps.push((
                step.id.clone(),
                step.step_type.as_str().to_string(),
                step.status.as_str().to_string(),
                serde_json::to_string(&step.input)
                    .map_err(|err| TaskCoreError::Serialization(err.to_string()))?,
                step.result
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|err| TaskCoreError::Serialization(err.to_string()))?,
                step.attempts,
                step.max_attempts,
                step.started_at,
                step.finished_at,
                step.error.clone(),
            ));
        }
        let task_clone = task.clone();
        self.with_conn(move |conn| {
            let updated = conn.execute(
                "UPDATE tasks SET title = ?1, instruction = ?2, status = ?3, priority = ?4,
                                  updated_at = ?5, scheduled_at = ?6, started_at = ?7, finished_at = ?8,
                                  attempts = ?9, max_attempts = ?10, lease_until = ?11,
                                  next_attempt_at = ?12, wait_for = ?13, current_step_index = ?14,
                                  result = ?15, last_error = ?16, verification = ?17, parent_task_id = ?18
                 WHERE id = ?19",
                params![
                    task_clone.title,
                    task_clone.instruction,
                    task_clone.status.as_str(),
                    task_clone.priority,
                    now,
                    task_clone.scheduled_at,
                    task_clone.started_at,
                    task_clone.finished_at,
                    task_clone.attempts,
                    task_clone.max_attempts,
                    task_clone.lease_until,
                    task_clone.next_attempt_at,
                    wait_for,
                    task_clone.current_step_index as i64,
                    result,
                    last_error,
                    verification,
                    task_clone.parent_task_id,
                    task_clone.id,
                ],
            )?;
            if updated != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            conn.execute("DELETE FROM task_steps WHERE task_id = ?1", params![task_clone.id])?;
            for (index, step) in serialized_steps.iter().enumerate() {
                conn.execute(
                    "INSERT INTO task_steps (id, task_id, step_index, step_type, status, input, result,
                                            attempts, max_attempts, started_at, finished_at, error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        step.0,
                        task_clone.id,
                        index as i64,
                        step.1,
                        step.2,
                        step.3,
                        step.4,
                        step.5,
                        step.6,
                        step.7,
                        step.8,
                        step.9,
                    ],
                )?;
            }
            Ok(())
        })
    }

    async fn delete(&self, task_id: &str) -> TaskCoreResult<bool> {
        let removed = self
            .with_conn(|conn| conn.execute("DELETE FROM tasks WHERE id = ?1", params![task_id]))?;
        Ok(removed > 0)
    }

    async fn list(&self, status: Option<TaskStatus>, limit: i64) -> TaskCoreResult<Vec<Task>> {
        self.with_conn(|conn| match status {
            Some(status) => Self::read_tasks(
                conn,
                "SELECT id FROM tasks WHERE status = ?1 ORDER BY updated_at DESC LIMIT ?2",
                &[&status.as_str(), &limit],
            ),
            None => Self::read_tasks(
                conn,
                "SELECT id FROM tasks ORDER BY updated_at DESC LIMIT ?1",
                &[&limit],
            ),
        })
    }

    async fn count(&self, status: Option<TaskStatus>) -> TaskCoreResult<i64> {
        self.with_conn(|conn| match status {
            Some(status) => conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = ?1",
                params![status.as_str()],
                |row| row.get(0),
            ),
            None => conn.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0)),
        })
    }

    async fn ready_batch(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>> {
        self.with_conn(|conn| {
            Self::read_tasks(
                conn,
                "SELECT id FROM tasks
                 WHERE status IN ('pending', 'scheduled', 'ready')
                   AND (scheduled_at IS NULL OR scheduled_at <= ?1)
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                   AND (lease_until IS NULL OR lease_until <= ?1)
                 ORDER BY priority DESC, created_at ASC
                 LIMIT ?2",
                &[&now, &limit],
            )
        })
    }

    async fn stale_running(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>> {
        self.with_conn(|conn| {
            Self::read_tasks(
                conn,
                "SELECT id FROM tasks
                 WHERE status = 'running'
                   AND lease_until IS NOT NULL
                   AND lease_until <= ?1
                 ORDER BY lease_until ASC
                 LIMIT ?2",
                &[&now, &limit],
            )
        })
    }

    async fn record_event(&self, event: TaskEvent) -> TaskCoreResult<()> {
        let payload = serde_json::to_string(&event.payload)
            .map_err(|err| TaskCoreError::Serialization(err.to_string()))?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO task_events (id, event_type, task_id, step_id, correlation_id, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    event.id,
                    event.event_type,
                    event.task_id,
                    event.step_id,
                    event.correlation_id,
                    payload,
                    event.created_at,
                ],
            )?;
            Ok(())
        })
    }

    async fn list_events(&self, task_id: &str, limit: i64) -> TaskCoreResult<Vec<TaskEvent>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, event_type, task_id, step_id, correlation_id, payload, created_at
                 FROM task_events WHERE task_id = ?1 ORDER BY created_at ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![task_id, limit], |row| {
                let payload: String = row.get(5)?;
                Ok(TaskEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    task_id: row.get(2)?,
                    step_id: row.get(3)?,
                    correlation_id: row.get(4)?,
                    payload: serde_json::from_str(&payload).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })?,
                    created_at: row.get(6)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
    }

    async fn pending_wait_expired(&self, now: Ms, limit: i64) -> TaskCoreResult<Vec<Task>> {
        let waiting = self.with_conn(|conn| {
            Self::read_tasks(
                conn,
                "SELECT id FROM tasks WHERE status = 'waiting' ORDER BY updated_at ASC LIMIT ?1",
                &[&limit],
            )
        })?;
        Ok(waiting
            .into_iter()
            .filter(|task| {
                task.wait_for
                    .as_ref()
                    .and_then(|wait| wait.timeout_at)
                    .is_some_and(|timeout| timeout <= now)
            })
            .collect())
    }

    async fn find_waiter(
        &self,
        event_type: &str,
        correlation_id: Option<&str>,
        _now: Ms,
    ) -> TaskCoreResult<Option<Task>> {
        let waiting = self.with_conn(|conn| {
            Self::read_tasks(
                conn,
                "SELECT id FROM tasks WHERE status = 'waiting' ORDER BY updated_at ASC LIMIT 100",
                &[],
            )
        })?;
        Ok(waiting.into_iter().find(|task| {
            task.wait_for.as_ref().is_some_and(|wait| {
                wait.event_type.as_deref() == Some(event_type)
                    && match (&wait.correlation_id, correlation_id) {
                        (Some(expected), Some(actual)) => expected == actual,
                        (None, _) => true,
                        (Some(_), None) => false,
                    }
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewTaskStep, TaskStepType};

    fn sample_task() -> NewTask {
        NewTask {
            title: "sample".to_string(),
            instruction: "do the thing".to_string(),
            scheduled_at: None,
            priority: None,
            max_attempts: None,
            verification: None,
            parent_task_id: None,
            steps: vec![NewTaskStep {
                step_type: TaskStepType::Wait,
                input: serde_json::json!({ "duration_ms": 10 }),
                max_attempts: None,
            }],
        }
    }

    #[tokio::test]
    async fn create_and_get_round_trip() {
        let store = SqliteTaskStore::open_in_memory().expect("in-memory store");
        let created = store.create(sample_task(), 100).await.expect("create");
        assert_eq!(created.status, TaskStatus::Pending);
        assert_eq!(created.steps.len(), 1);
        let loaded = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(loaded.id, created.id);
        assert_eq!(loaded.title, "sample");
    }

    #[tokio::test]
    async fn update_rejects_illegal_transitions() {
        let store = SqliteTaskStore::open_in_memory().expect("in-memory store");
        let mut task = store.create(sample_task(), 100).await.expect("create");
        task.status = TaskStatus::Running;
        let err = store.update(&task, 101).await.expect_err("must reject");
        assert!(matches!(err, TaskCoreError::IllegalTransition { .. }));
        task.status = TaskStatus::Ready;
        store.update(&task, 101).await.expect("legal transition");
    }

    #[tokio::test]
    async fn ready_batch_and_stale_running_filters_match() {
        let store = SqliteTaskStore::open_in_memory().expect("in-memory store");
        let mut future = sample_task();
        future.scheduled_at = Some(10_000);
        let future = store.create(future, 100).await.expect("future");
        assert_eq!(future.status, TaskStatus::Scheduled);
        let ready = store.ready_batch(200, 10).await.expect("ready");
        assert!(ready.is_empty());

        let mut task = store.create(sample_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 150).await.expect("ready");
        let ready = store.ready_batch(200, 10).await.expect("ready");
        assert_eq!(ready.len(), 1);

        task.status = TaskStatus::Running;
        task.lease_until = Some(50);
        store.update(&task, 160).await.expect("running");
        let stale = store.stale_running(200, 10).await.expect("stale");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, task.id);
    }
}
