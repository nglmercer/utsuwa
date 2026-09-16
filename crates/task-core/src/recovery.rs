//! Crash recovery: requeue work whose lease expired so no task is ever
//! stuck in `running` after a host restart.

use crate::error::TaskCoreResult;
use crate::model::{Ms, Task, TaskStatus, TaskStepStatus};
use crate::store::TaskStore;

/// Default lease for a running task: 5 minutes.
pub const DEFAULT_LEASE_MS: Ms = 5 * 60 * 1000;

/// Maximum running tasks recovered in one pass.
pub const RECOVERY_BATCH_LIMIT: i64 = 100;

pub struct RecoveryReport {
    pub recovered: Vec<String>,
    pub exhausted: Vec<String>,
}

/// Requeue stale running tasks (lease expired) back to `ready`, or fail them
/// when attempts are exhausted. Tasks with no lease at all are left alone:
/// only the executor sets leases, so a missing lease means the task never
/// actually started.
pub async fn recover_stale<S: TaskStore>(
    store: &S,
    now: Ms,
    lease_ms: Ms,
) -> TaskCoreResult<RecoveryReport> {
    let _ = lease_ms;
    let stale = store.stale_running(now, RECOVERY_BATCH_LIMIT).await?;
    let mut report = RecoveryReport {
        recovered: Vec::new(),
        exhausted: Vec::new(),
    };
    for mut task in stale {
        if task.status != TaskStatus::Running {
            continue;
        }
        reset_running_step(&mut task);
        if task.attempts >= task.max_attempts {
            task.status = TaskStatus::Failed;
            task.finished_at = Some(now);
            task.lease_until = None;
            task.last_error = Some(crate::model::TaskError {
                message: "recovered from crash but attempts exhausted".to_string(),
                retryable: false,
                timestamp: now,
            });
            store.update(&task, now).await?;
            report.exhausted.push(task.id);
        } else {
            task.status = TaskStatus::Ready;
            task.lease_until = None;
            task.next_attempt_at = Some(now);
            store.update(&task, now).await?;
            report.recovered.push(task.id);
        }
    }
    Ok(report)
}

fn reset_running_step(task: &mut Task) {
    if let Some(step) = task.steps.get_mut(task.current_step_index) {
        if step.status == TaskStepStatus::Running {
            step.status = TaskStepStatus::Pending;
            step.started_at = None;
        }
    }
}

/// Expire `waiting` tasks whose wait timeout passed.
/// - Pure duration waits (no event) are DONE when they expire: the step is
///   completed and the task requeued at the next step.
/// - Event waits that time out are retried (fresh wait, bounded by
///   max_attempts) or failed when attempts are exhausted.
pub async fn expire_waits<S: TaskStore>(store: &S, now: Ms) -> TaskCoreResult<Vec<Task>> {
    let expired = store
        .pending_wait_expired(now, RECOVERY_BATCH_LIMIT)
        .await?;
    let mut released = Vec::with_capacity(expired.len());
    for mut task in expired {
        let had_event = task
            .wait_for
            .as_ref()
            .and_then(|wait| wait.event_type.clone());
        task.wait_for = None;
        if had_event.is_none() {
            complete_current_wait_step(&mut task, now);
            task.status = TaskStatus::Ready;
            task.next_attempt_at = Some(now);
            store.update(&task, now).await?;
        } else if task.attempts >= task.max_attempts {
            // Waiting -> Failed is the administrative exhaustion edge.
            task.status = TaskStatus::Failed;
            task.finished_at = Some(now);
            task.last_error = Some(crate::model::TaskError {
                message: "wait timed out and attempts exhausted".to_string(),
                retryable: false,
                timestamp: now,
            });
            store.update(&task, now).await?;
        } else {
            task.status = TaskStatus::Ready;
            task.next_attempt_at = Some(now);
            store.update(&task, now).await?;
        }
        released.push(task);
    }
    Ok(released)
}

fn complete_current_wait_step(task: &mut Task, now: Ms) {
    if let Some(step) = task.steps.get_mut(task.current_step_index) {
        if step.step_type == crate::model::TaskStepType::Wait {
            step.status = TaskStepStatus::Completed;
            step.result = Some(serde_json::json!({ "expired": true }));
            step.finished_at = Some(now);
            task.current_step_index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{priority, NewTask, NewTaskStep, TaskStepType, TaskWait};
    use crate::store::SqliteTaskStore;

    fn wait_task() -> NewTask {
        NewTask {
            title: "wait".to_string(),
            instruction: "wait".to_string(),
            scheduled_at: None,
            priority: Some(priority::NORMAL),
            max_attempts: Some(2),
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
    async fn stale_running_task_is_requeued() {
        let store = SqliteTaskStore::open_in_memory().expect("store");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        task.attempts = 1;
        task.lease_until = Some(50);
        task.steps[0].status = TaskStepStatus::Running;
        store.update(&task, 120).await.expect("running");

        let report = recover_stale(&store, 1_000, DEFAULT_LEASE_MS)
            .await
            .expect("recover");
        assert_eq!(report.recovered, vec![task.id.clone()]);
        assert!(report.exhausted.is_empty());
        let loaded = store.get(&task.id).await.expect("get").expect("task");
        assert_eq!(loaded.status, TaskStatus::Ready);
        assert_eq!(loaded.steps[0].status, TaskStepStatus::Pending);
    }

    #[tokio::test]
    async fn exhausted_stale_task_fails() {
        let store = SqliteTaskStore::open_in_memory().expect("store");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        task.attempts = 2;
        task.lease_until = Some(50);
        store.update(&task, 120).await.expect("running");

        let report = recover_stale(&store, 1_000, DEFAULT_LEASE_MS)
            .await
            .expect("recover");
        assert!(report.recovered.is_empty());
        assert_eq!(report.exhausted, vec![task.id.clone()]);
        let loaded = store.get(&task.id).await.expect("get").expect("task");
        assert_eq!(loaded.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn expired_duration_wait_completes_the_step() {
        let store = SqliteTaskStore::open_in_memory().expect("store");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        store.update(&task, 120).await.expect("running");
        task.status = TaskStatus::Waiting;
        task.wait_for = Some(TaskWait {
            event_type: None,
            correlation_id: None,
            timeout_at: Some(200),
        });
        store.update(&task, 130).await.expect("waiting");

        let released = expire_waits(&store, 500).await.expect("expire");
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].status, TaskStatus::Ready);
        assert_eq!(released[0].steps[0].status, TaskStepStatus::Completed);
        assert_eq!(released[0].current_step_index, 1);
    }

    #[tokio::test]
    async fn expired_wait_is_released() {
        let store = SqliteTaskStore::open_in_memory().expect("store");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        store.update(&task, 120).await.expect("running");
        task.status = TaskStatus::Waiting;
        task.wait_for = Some(TaskWait {
            event_type: Some("avatar.routine.completed".to_string()),
            correlation_id: Some(task.id.clone()),
            timeout_at: Some(200),
        });
        store.update(&task, 130).await.expect("waiting");

        let released = expire_waits(&store, 500).await.expect("expire");
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].status, TaskStatus::Ready);
    }
}
