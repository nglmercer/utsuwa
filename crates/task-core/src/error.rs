use crate::model::TaskStatus;
use thiserror::Error;

pub type TaskCoreResult<T> = Result<T, TaskCoreError>;

#[derive(Debug, Error)]
pub enum TaskCoreError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("illegal task transition for {task_id}: {from:?} -> {to:?}")]
    IllegalTransition {
        task_id: String,
        from: TaskStatus,
        to: TaskStatus,
    },
    #[error("invalid task: {0}")]
    Validation(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("step execution failed: {0}")]
    Step(String),
    #[error("task storage error: {0}")]
    Storage(String),
    #[error("task serialization error: {0}")]
    Serialization(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}
