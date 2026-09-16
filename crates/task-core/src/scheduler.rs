//! Scheduler: one tick recovers crashes, expires waits, promotes due
//! tasks, and executes the ready batch. Event delivery and review resume
//! close the loop for parked tasks.

use crate::clock::Clock;
use crate::error::{TaskCoreError, TaskCoreResult};
use crate::executor::Executor;
use crate::model::{event_type, Ms, Task, TaskEvent, TaskStatus, TaskStepStatus};
use crate::store::TaskStore;
use std::sync::Arc;

pub const DEFAULT_BATCH_LIMIT: i64 = 10;

pub struct Scheduler<S: TaskStore> {
    store: Arc<S>,
    executor: Executor<S>,
    clock: Arc<dyn Clock>,
    batch_limit: i64,
}

pub struct TickReport {
    pub recovered: Vec<String>,
    pub exhausted: Vec<String>,
    pub waits_released: usize,
    pub executed: Vec<String>,
    pub errors: Vec<String>,
}

impl<S: TaskStore> Scheduler<S> {
    pub fn new(
        store: Arc<S>,
        executor: Executor<S>,
        clock: Arc<dyn Clock>,
        batch_limit: i64,
    ) -> Self {
        Self {
            store,
            executor,
            clock,
            batch_limit,
        }
    }

    /// Run one scheduling pass. Safe to call on a timer and at startup.
    pub async fn tick(&self) -> TaskCoreResult<TickReport> {
        self.tick_filtered(None).await
    }

    /// Scheduling pass that executes ONLY `task_id`. Recovery, wait
    /// expiry, and promotion stay global (cheap bookkeeping, no model
    /// calls), but no other task's steps run — so a stranger's slow
    /// model turn can never delay this task's wait expiry. Single-task
    /// drivers (`task-cli run`/`demo-time`) must use this instead of
    /// `tick`: executing strangers is both a side effect and a timing
    /// pollutant (a 12s foreign turn once stretched a 3s wait to 13s).
    pub async fn tick_one(&self, task_id: &str) -> TaskCoreResult<TickReport> {
        self.tick_filtered(Some(task_id)).await
    }

    async fn tick_filtered(&self, only: Option<&str>) -> TaskCoreResult<TickReport> {
        let now = self.clock.now_ms();
        let recovery = crate::recovery::recover_stale(
            self.store.as_ref(),
            now,
            crate::recovery::DEFAULT_LEASE_MS,
        )
        .await?;
        let released = crate::recovery::expire_waits(self.store.as_ref(), now).await?;
        self.promote_due(now).await?;

        let batch = self.store.ready_batch(now, self.batch_limit).await?;
        let mut executed = Vec::new();
        let mut errors = Vec::new();
        for task in batch {
            if only.is_some_and(|id| id != task.id) {
                continue;
            }
            // Re-read: a previous execution in this batch may have touched it.
            let Some(fresh) = self.store.get(&task.id).await? else {
                continue;
            };
            if fresh.status != TaskStatus::Ready {
                continue;
            }
            match self.executor.execute_task(&task.id).await {
                Ok(done) => executed.push(done.id),
                Err(err) => errors.push(format!("{}: {err}", task.id)),
            }
        }
        Ok(TickReport {
            recovered: recovery.recovered,
            exhausted: recovery.exhausted,
            waits_released: released.len(),
            executed,
            errors,
        })
    }

    /// Move due `pending`/`scheduled` tasks to `ready`.
    async fn promote_due(&self, now: Ms) -> TaskCoreResult<()> {
        let batch = self.store.ready_batch(now, self.batch_limit).await?;
        for mut task in batch {
            if task.status == TaskStatus::Ready {
                continue;
            }
            task.status = TaskStatus::Ready;
            self.store.update(&task, now).await?;
            self.store
                .record_event(TaskEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    event_type: event_type::READY.to_string(),
                    task_id: Some(task.id.clone()),
                    step_id: None,
                    correlation_id: None,
                    payload: serde_json::json!({}),
                    created_at: now,
                })
                .await?;
        }
        Ok(())
    }

    /// Deliver an external event (avatar receipt, agent turn done, tool
    /// callback) to the parked task waiting for it.
    pub async fn deliver_event(
        &self,
        event_type: &str,
        correlation_id: Option<&str>,
        payload: serde_json::Value,
    ) -> TaskCoreResult<Option<Task>> {
        let now = self.clock.now_ms();
        let Some(mut task) = self
            .store
            .find_waiter(event_type, correlation_id, now)
            .await?
        else {
            tracing::debug!(
                event_type = %event_type,
                correlation_id = ?correlation_id,
                "task event arrived with no waiter"
            );
            return Ok(None);
        };
        // Receipt content summary (never the full payload): the one line
        // that shows what the renderer actually reported. Computed before
        // the payload moves into the step result.
        let receipt_status = payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string();
        let completed_steps = payload
            .get("completed_steps")
            .and_then(serde_json::Value::as_array)
            .map(|steps| steps.len());
        if let Some(step) = task.steps.get_mut(task.current_step_index) {
            step.status = TaskStepStatus::Completed;
            step.result = Some(payload);
            step.finished_at = Some(now);
        }
        task.current_step_index += 1;
        task.wait_for = None;
        task.status = TaskStatus::Ready;
        task.next_attempt_at = Some(now);
        self.store.update(&task, now).await?;
        tracing::info!(
            task_id = %task.id,
            event_type = %event_type,
            receipt_status = %receipt_status,
            completed_steps = ?completed_steps,
            "task event delivered, step advanced"
        );
        Ok(Some(task))
    }

    /// Resume a task parked in `needs_review`. `approved` continues at the
    /// current step; rejection fails the task with the given reason.
    pub async fn resume_from_review(
        &self,
        task_id: &str,
        approved: bool,
        note: Option<String>,
    ) -> TaskCoreResult<Task> {
        let now = self.clock.now_ms();
        let mut task = self
            .store
            .get(task_id)
            .await?
            .ok_or_else(|| TaskCoreError::NotFound(task_id.to_string()))?;
        if task.status != TaskStatus::NeedsReview {
            return Err(TaskCoreError::Step(format!(
                "task {task_id} is {} (expected needs_review)",
                task.status.as_str()
            )));
        }
        if approved {
            if let Some(step) = task.steps.get_mut(task.current_step_index) {
                // The parked review step is satisfied by the approval itself.
                if step.step_type == crate::model::TaskStepType::Approval {
                    step.status = TaskStepStatus::Completed;
                    step.result = Some(serde_json::json!({ "approved": true, "note": note }));
                    step.finished_at = Some(now);
                    task.current_step_index += 1;
                } else {
                    step.status = TaskStepStatus::Pending;
                }
            }
            task.status = TaskStatus::Ready;
            task.next_attempt_at = Some(now);
            task.last_error = None;
            self.store.update(&task, now).await?;
            Ok(task)
        } else {
            let reason = note.unwrap_or_else(|| "rejected in review".to_string());
            if let Some(step) = task.steps.get_mut(task.current_step_index) {
                step.status = TaskStepStatus::Failed;
                step.error = Some(reason.clone());
                step.finished_at = Some(now);
            }
            // NeedsReview -> Failed is the administrative rejection edge.
            task.status = TaskStatus::Failed;
            task.finished_at = Some(now);
            task.last_error = Some(crate::model::TaskError {
                message: reason,
                retryable: false,
                timestamp: now,
            });
            self.store.update(&task, now).await?;
            Ok(task)
        }
    }

    /// Cancel a task from any non-terminal state.
    pub async fn cancel(&self, task_id: &str, reason: String) -> TaskCoreResult<Task> {
        let now = self.clock.now_ms();
        let mut task = self
            .store
            .get(task_id)
            .await?
            .ok_or_else(|| TaskCoreError::NotFound(task_id.to_string()))?;
        if task.status.is_terminal() {
            return Err(TaskCoreError::Step(format!(
                "task {task_id} is already {}",
                task.status.as_str()
            )));
        }
        task.status = TaskStatus::Cancelled;
        task.finished_at = Some(now);
        task.lease_until = None;
        task.wait_for = None;
        task.last_error = Some(crate::model::TaskError {
            message: reason,
            retryable: false,
            timestamp: now,
        });
        self.store.update(&task, now).await?;
        self.store
            .record_event(TaskEvent {
                id: uuid::Uuid::new_v4().to_string(),
                event_type: event_type::CANCELLED.to_string(),
                task_id: Some(task.id.clone()),
                step_id: None,
                correlation_id: None,
                payload: serde_json::json!({}),
                created_at: now,
            })
            .await?;
        Ok(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::executor::test_runners;
    use crate::model::{priority, NewTask, NewTaskStep, TaskStepType};
    use crate::store::SqliteTaskStore;

    fn setup() -> (
        Arc<SqliteTaskStore>,
        Arc<ManualClock>,
        Scheduler<SqliteTaskStore>,
    ) {
        let store = Arc::new(SqliteTaskStore::open_in_memory().expect("store"));
        let clock = ManualClock::new(1_000);
        let executor = Executor::new(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            test_runners(),
            60_000,
        );
        let scheduler =
            Scheduler::new(store.clone(), executor, clock.clone() as Arc<dyn Clock>, 10);
        (store, clock, scheduler)
    }

    fn notify_task() -> NewTask {
        NewTask {
            title: "notify".to_string(),
            instruction: "notify".to_string(),
            scheduled_at: None,
            priority: Some(priority::NORMAL),
            max_attempts: Some(3),
            verification: None,
            parent_task_id: None,
            steps: vec![NewTaskStep {
                step_type: TaskStepType::Notification,
                input: serde_json::json!({ "body": "hi" }),
                max_attempts: None,
            }],
        }
    }

    #[tokio::test]
    async fn tick_promotes_and_executes() {
        let (store, _, scheduler) = setup();
        let created = store.create(notify_task(), 1_000).await.expect("create");
        assert_eq!(created.status, TaskStatus::Pending);
        let report = scheduler.tick().await.expect("tick");
        assert!(report.errors.is_empty());
        assert_eq!(report.executed, vec![created.id.clone()]);
        let done = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn tick_one_executes_only_the_target() {
        let (store, _, scheduler) = setup();
        let first = store.create(notify_task(), 1_000).await.expect("create");
        let second = store.create(notify_task(), 1_000).await.expect("create2");
        let report = scheduler.tick_one(&first.id).await.expect("tick_one");
        assert_eq!(report.executed, vec![first.id.clone()]);
        let done = store.get(&first.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed);
        // The stranger is promoted (global bookkeeping) but never
        // executed: no foreign model turn may run inside this tick.
        let stranger = store.get(&second.id).await.expect("get").expect("task");
        assert_eq!(stranger.status, TaskStatus::Ready);
        assert_eq!(stranger.steps[0].status, TaskStepStatus::Pending);
    }

    #[tokio::test]
    async fn future_task_waits_for_its_schedule() {
        let (store, clock, scheduler) = setup();
        let mut input = notify_task();
        input.scheduled_at = Some(50_000);
        let created = store.create(input, 1_000).await.expect("create");
        assert_eq!(created.status, TaskStatus::Scheduled);
        let report = scheduler.tick().await.expect("tick");
        assert!(report.executed.is_empty());
        clock.set(60_000);
        let report = scheduler.tick().await.expect("tick");
        assert_eq!(report.executed, vec![created.id.clone()]);
    }

    #[tokio::test]
    async fn event_delivery_resumes_waiting_task() {
        let (store, _, scheduler) = setup();
        let mut input = notify_task();
        input.steps = vec![NewTaskStep {
            step_type: TaskStepType::Wait,
            input: serde_json::json!({
                "event_type": "avatar.routine.completed",
                "duration_ms": 60_000,
            }),
            max_attempts: None,
        }];
        let created = store.create(input, 1_000).await.expect("create");
        scheduler.tick().await.expect("tick");
        let waiting = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(waiting.status, TaskStatus::Waiting);
        let resumed = scheduler
            .deliver_event(
                "avatar.routine.completed",
                Some(&created.id),
                serde_json::json!({ "status": "success", "completed_steps": [] }),
            )
            .await
            .expect("deliver")
            .expect("waiter");
        assert_eq!(resumed.status, TaskStatus::Ready);
        scheduler.tick().await.expect("tick2");
        let done = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn review_resume_approves_and_rejects() {
        let (store, _, scheduler) = setup();
        let mut input = notify_task();
        input.steps = vec![NewTaskStep {
            step_type: TaskStepType::Approval,
            input: serde_json::json!({ "reason": "ok?" }),
            max_attempts: None,
        }];
        let created = store.create(input.clone(), 1_000).await.expect("create");
        scheduler.tick().await.expect("tick");
        let parked = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(parked.status, TaskStatus::NeedsReview);
        let resumed = scheduler
            .resume_from_review(&created.id, true, None)
            .await
            .expect("approve");
        assert_eq!(resumed.status, TaskStatus::Ready);
        scheduler.tick().await.expect("tick2");
        let done = store.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed);

        let created2 = store.create(input, 1_000).await.expect("create2");
        scheduler.tick().await.expect("tick3");
        let rejected = scheduler
            .resume_from_review(&created2.id, false, Some("no".to_string()))
            .await
            .expect("reject");
        assert_eq!(rejected.status, TaskStatus::Failed);
    }
}
