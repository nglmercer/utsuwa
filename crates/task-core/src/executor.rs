//! Step executor: runs one task's steps in order, persists every outcome,
//! and only completes a task after verification passes.

use crate::clock::Clock;
use crate::error::{TaskCoreError, TaskCoreResult};
use crate::model::{
    event_type, Ms, Task, TaskError, TaskEvent, TaskStatus, TaskStep, TaskStepStatus, TaskStepType,
    TaskWait,
};
use crate::store::TaskStore;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Backoff between attempts: base delay, doubled per attempt, capped.
pub const RETRY_BACKOFF_BASE_MS: Ms = 1_000;
pub const RETRY_BACKOFF_MAX_MS: Ms = 60_000;
/// Default per-step execution timeout.
pub const DEFAULT_STEP_TIMEOUT_MS: Ms = 120_000;
/// Default wait for an avatar routine receipt before the wait expires.
pub const DEFAULT_AVATAR_RECEIPT_TIMEOUT_MS: Ms = 60_000;

pub fn backoff_ms(attempt: i32) -> Ms {
    let shift = attempt.clamp(0, 10) as u32;
    (RETRY_BACKOFF_BASE_MS.saturating_mul(1 << shift)).min(RETRY_BACKOFF_MAX_MS)
}

#[derive(Debug, Clone)]
pub enum StepOutcome {
    Completed(serde_json::Value),
    Wait(TaskWait),
    NeedsReview { reason: String },
    Failed { message: String, retryable: bool },
}

#[derive(Debug, Clone)]
pub struct StepContext {
    pub task_id: String,
    pub attempt: i32,
    pub timeout_ms: Ms,
    pub now_ms: Ms,
}

#[async_trait]
pub trait StepRunner: Send + Sync {
    async fn run(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome;
}

pub struct Executor<S: TaskStore> {
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    runners: Runners,
    lease_ms: Ms,
}

#[derive(Clone)]
pub struct Runners {
    pub wait: Arc<dyn StepRunner>,
    pub notification: Arc<dyn StepRunner>,
    pub avatar_routine: Arc<dyn StepRunner>,
    pub tool: Arc<dyn StepRunner>,
    pub agent: Arc<dyn StepRunner>,
    pub approval: Arc<dyn StepRunner>,
}

impl<S: TaskStore> Executor<S> {
    pub fn new(store: Arc<S>, clock: Arc<dyn Clock>, runners: Runners, lease_ms: Ms) -> Self {
        Self {
            store,
            clock,
            runners,
            lease_ms,
        }
    }

    fn runner_for(&self, step_type: TaskStepType) -> &Arc<dyn StepRunner> {
        match step_type {
            TaskStepType::Wait => &self.runners.wait,
            TaskStepType::Notification => &self.runners.notification,
            TaskStepType::AvatarRoutine => &self.runners.avatar_routine,
            TaskStepType::Tool => &self.runners.tool,
            TaskStepType::Agent => &self.runners.agent,
            TaskStepType::Approval => &self.runners.approval,
        }
    }

    /// Run one task to a resting state: completed, failed, waiting, or
    /// needs_review. Returns the final task snapshot.
    pub async fn execute_task(&self, task_id: &str) -> TaskCoreResult<Task> {
        let now = self.clock.now_ms();
        let mut task = self
            .store
            .get(task_id)
            .await?
            .ok_or_else(|| TaskCoreError::NotFound(task_id.to_string()))?;
        if task.status != TaskStatus::Ready {
            return Err(TaskCoreError::Step(format!(
                "task {task_id} is {} (expected ready)",
                task.status.as_str()
            )));
        }
        task.status = TaskStatus::Running;
        task.started_at = task.started_at.or(Some(now));
        task.attempts += 1;
        task.lease_until = Some(now + self.lease_ms);
        task.next_attempt_at = None;
        self.store.update(&task, now).await?;
        self.emit(&task, None, event_type::STARTED, serde_json::json!({}))
            .await?;

        loop {
            let now = self.clock.now_ms();
            let index = task.current_step_index;
            let Some(step) = task.steps.get(index).cloned() else {
                return self.finish_task(task).await;
            };
            if step.status == TaskStepStatus::Completed || step.status == TaskStepStatus::Skipped {
                task.current_step_index += 1;
                self.store.update(&task, now).await?;
                continue;
            }
            let outcome = self.run_step(&task, &step).await;
            let now = self.clock.now_ms();
            task = self
                .store
                .get(task_id)
                .await?
                .ok_or_else(|| TaskCoreError::NotFound(task_id.to_string()))?;
            match outcome {
                StepOutcome::Completed(value) => {
                    self.complete_step(&mut task, index, value, now).await?;
                }
                StepOutcome::Wait(wait) => {
                    self.park_waiting(&mut task, index, wait, now).await?;
                    return Ok(task);
                }
                StepOutcome::NeedsReview { reason } => {
                    self.park_review(&mut task, index, reason, now).await?;
                    return Ok(task);
                }
                StepOutcome::Failed { message, retryable } => {
                    if self
                        .fail_step(&mut task, index, message, retryable, now)
                        .await?
                    {
                        return Ok(task);
                    }
                }
            }
        }
    }

    async fn run_step(&self, task: &Task, step: &TaskStep) -> StepOutcome {
        let now = self.clock.now_ms();
        let timeout_ms = step
            .input
            .get("timeout_ms")
            .and_then(serde_json::Value::as_i64)
            .filter(|t| *t > 0)
            .unwrap_or(DEFAULT_STEP_TIMEOUT_MS);
        let ctx = StepContext {
            task_id: task.id.clone(),
            attempt: step.attempts + 1,
            timeout_ms,
            now_ms: now,
        };
        {
            let mut claimed = task.clone();
            if let Some(current) = claimed.steps.get_mut(claimed.current_step_index) {
                current.status = TaskStepStatus::Running;
                current.started_at = current.started_at.or(Some(now));
                current.attempts += 1;
            }
            claimed.lease_until = Some(now + self.lease_ms);
            if self.store.update(&claimed, now).await.is_err() {
                return StepOutcome::Failed {
                    message: "failed to persist step claim".to_string(),
                    retryable: true,
                };
            }
        }
        self.emit(
            task,
            Some(&step.id),
            event_type::STEP_STARTED,
            serde_json::json!({
                "step_type": step.step_type.as_str(),
            }),
        )
        .await
        .ok();
        let runner = self.runner_for(step.step_type).clone();
        let task_snapshot = task.clone();
        let step_snapshot = step.clone();
        match tokio::time::timeout(
            Duration::from_millis(timeout_ms.max(1) as u64),
            runner.run(&ctx, &task_snapshot, &step_snapshot),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => StepOutcome::Failed {
                message: format!("step timed out after {timeout_ms}ms"),
                retryable: true,
            },
        }
    }

    async fn complete_step(
        &self,
        task: &mut Task,
        index: usize,
        value: serde_json::Value,
        now: Ms,
    ) -> TaskCoreResult<()> {
        if let Some(step) = task.steps.get_mut(index) {
            step.status = TaskStepStatus::Completed;
            step.result = Some(value.clone());
            step.finished_at = Some(now);
            step.error = None;
        }
        task.current_step_index = index + 1;
        task.lease_until = Some(now + self.lease_ms);
        task.last_error = None;
        self.store.update(task, now).await?;
        let step_id = task.steps.get(index).map(|s| s.id.clone());
        self.emit(
            task,
            step_id.as_deref(),
            event_type::STEP_COMPLETED,
            serde_json::json!({ "result": value }),
        )
        .await
    }

    async fn park_waiting(
        &self,
        task: &mut Task,
        index: usize,
        wait: TaskWait,
        now: Ms,
    ) -> TaskCoreResult<()> {
        if let Some(step) = task.steps.get_mut(index) {
            step.status = TaskStepStatus::Running;
        }
        task.status = TaskStatus::Waiting;
        task.wait_for = Some(wait.clone());
        task.lease_until = None;
        self.store.update(task, now).await?;
        let step_id = task.steps.get(index).map(|s| s.id.clone());
        self.emit(
            task,
            step_id.as_deref(),
            event_type::WAITING,
            serde_json::json!({ "wait_for": wait }),
        )
        .await
    }

    async fn park_review(
        &self,
        task: &mut Task,
        index: usize,
        reason: String,
        now: Ms,
    ) -> TaskCoreResult<()> {
        if let Some(step) = task.steps.get_mut(index) {
            step.status = TaskStepStatus::Running;
        }
        task.status = TaskStatus::NeedsReview;
        task.lease_until = None;
        task.last_error = Some(TaskError {
            message: reason.clone(),
            retryable: false,
            timestamp: now,
        });
        self.store.update(task, now).await?;
        let step_id = task.steps.get(index).map(|s| s.id.clone());
        self.emit(
            task,
            step_id.as_deref(),
            event_type::REVIEW_REQUIRED,
            serde_json::json!({ "reason": reason }),
        )
        .await?;
        tracing::info!(
            task_id = %task.id,
            reason = %reason,
            "task needs review"
        );
        Ok(())
    }

    /// Returns true when the task reached a resting state (requeued or failed).
    async fn fail_step(
        &self,
        task: &mut Task,
        index: usize,
        message: String,
        retryable: bool,
        now: Ms,
    ) -> TaskCoreResult<bool> {
        let attempts_left = task.attempts < task.max_attempts;
        if let Some(step) = task.steps.get_mut(index) {
            let step_retries_left = step.attempts < step.max_attempts;
            if retryable && attempts_left && step_retries_left {
                step.status = TaskStepStatus::Pending;
                step.error = Some(message.clone());
            } else {
                step.status = TaskStepStatus::Failed;
                step.error = Some(message.clone());
                step.finished_at = Some(now);
            }
        }
        let step_id = task.steps.get(index).map(|s| s.id.clone());
        if retryable && attempts_left {
            task.status = TaskStatus::Ready;
            task.lease_until = None;
            task.next_attempt_at = Some(now + backoff_ms(task.attempts));
            task.last_error = Some(TaskError {
                message: message.clone(),
                retryable: true,
                timestamp: now,
            });
            self.store.update(task, now).await?;
            self.emit(
                task,
                step_id.as_deref(),
                event_type::STEP_FAILED,
                serde_json::json!({ "message": message, "will_retry": true }),
            )
            .await?;
            return Ok(true);
        }
        task.status = TaskStatus::Failed;
        task.finished_at = Some(now);
        task.lease_until = None;
        task.last_error = Some(TaskError {
            message: message.clone(),
            retryable: false,
            timestamp: now,
        });
        self.store.update(task, now).await?;
        self.emit(
            task,
            step_id.as_deref(),
            event_type::FAILED,
            serde_json::json!({ "message": message }),
        )
        .await?;
        tracing::info!(
            task_id = %task.id,
            attempts = task.attempts,
            error = %message,
            "task failed"
        );
        Ok(true)
    }

    async fn finish_task(&self, mut task: Task) -> TaskCoreResult<Task> {
        let now = self.clock.now_ms();
        task.result = Some(aggregate_results(&task));
        match crate::verify::verify_task(&task) {
            Ok(()) => {
                task.status = TaskStatus::Completed;
                task.finished_at = Some(now);
                task.lease_until = None;
                self.store.update(&task, now).await?;
                self.emit(&task, None, event_type::COMPLETED, serde_json::json!({}))
                    .await?;
                tracing::info!(
                    task_id = %task.id,
                    attempts = task.attempts,
                    "task completed"
                );
                Ok(task)
            }
            Err(err) => {
                let message = format!("verification failed: {err}");
                if task.attempts < task.max_attempts {
                    // Retry the step that produced the bad receipt — not the
                    // whole task, and never by re-verifying the same result.
                    let retry_index = verification_retry_index(&task);
                    if let Some(step) = task.steps.get_mut(retry_index) {
                        step.status = TaskStepStatus::Pending;
                        step.result = None;
                        step.error = Some(message.clone());
                        step.finished_at = None;
                    }
                    task.current_step_index = retry_index;
                    task.status = TaskStatus::Ready;
                    task.lease_until = None;
                    task.next_attempt_at = Some(now + backoff_ms(task.attempts));
                    task.last_error = Some(TaskError {
                        message: message.clone(),
                        retryable: true,
                        timestamp: now,
                    });
                    self.store.update(&task, now).await?;
                    self.emit(
                        &task,
                        None,
                        event_type::STEP_FAILED,
                        serde_json::json!({ "message": message, "will_retry": true }),
                    )
                    .await?;
                    tracing::info!(
                        task_id = %task.id,
                        attempts = task.attempts,
                        error = %message,
                        "task verification failed, retrying failed step"
                    );
                    Ok(task)
                } else {
                    task.status = TaskStatus::Failed;
                    task.finished_at = Some(now);
                    task.lease_until = None;
                    task.last_error = Some(TaskError {
                        message: message.clone(),
                        retryable: false,
                        timestamp: now,
                    });
                    self.store.update(&task, now).await?;
                    self.emit(&task, None, event_type::FAILED, serde_json::json!({}))
                        .await?;
                    tracing::info!(
                        task_id = %task.id,
                        attempts = task.attempts,
                        error = %message,
                        "task failed verification, attempts exhausted"
                    );
                    Ok(task)
                }
            }
        }
    }

    async fn emit(
        &self,
        task: &Task,
        step_id: Option<&str>,
        event_type: &str,
        payload: serde_json::Value,
    ) -> TaskCoreResult<()> {
        self.store
            .record_event(TaskEvent {
                id: uuid::Uuid::new_v4().to_string(),
                event_type: event_type.to_string(),
                task_id: Some(task.id.clone()),
                step_id: step_id.map(str::to_string),
                correlation_id: None,
                payload,
                created_at: self.clock.now_ms(),
            })
            .await
    }
}

/// Index of the step a verification retry should re-run: the most recent
/// step with an explicit failure receipt (`status` present and not
/// `"success"` — a failed routine/tool receipt names its producer).
/// Results without a `status` field (wait steps, plain completions) are
/// neutral and never selected. Falls back to the last step when every
/// receipt claims success but verification still failed (misconfigured
/// expectation or an overclaiming producer — bounded by max_attempts, and
/// the reason verification specs must be exact).
fn verification_retry_index(task: &Task) -> usize {
    let last = task.steps.len().saturating_sub(1);
    task.steps
        .iter()
        .rposition(|step| {
            step.result
                .as_ref()
                .and_then(|result| result.get("status"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|status| status != "success")
        })
        .unwrap_or(last)
}

fn aggregate_results(task: &Task) -> serde_json::Value {
    let steps: Vec<serde_json::Value> = task
        .steps
        .iter()
        .map(|step| {
            serde_json::json!({
                "step_id": step.id,
                "step_type": step.step_type.as_str(),
                "status": step.status.as_str(),
                "result": step.result.clone().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    // Surface the last step result at the top level too: verification specs
    // (avatar receipts, tool receipts) read `result.status` directly.
    let mut merged = serde_json::json!({ "steps": steps });
    if let Some(last) = task
        .steps
        .iter()
        .rev()
        .find_map(|step| step.result.clone())
        .and_then(|value| value.as_object().cloned())
    {
        if let Some(object) = merged.as_object_mut() {
            for (key, value) in last {
                object.insert(key, value);
            }
        }
    }
    merged
}

/// Wait steps never block the executor thread: a duration becomes a durable
/// `waiting` state that `expire_waits` releases; an event wait parks until
/// the matching event (or timeout) arrives.
pub struct WaitRunner;

#[async_trait]
impl StepRunner for WaitRunner {
    async fn run(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome {
        let duration_ms = step
            .input
            .get("duration_ms")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let event_type = step
            .input
            .get("event_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let correlation_id = step
            .input
            .get("correlation_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(task.id.clone()));
        // A zero-duration wait with no event is a no-op, not a park.
        if duration_ms <= 0 && event_type.is_none() {
            return StepOutcome::Completed(serde_json::json!({ "waited_ms": 0 }));
        }
        StepOutcome::Wait(TaskWait {
            event_type,
            correlation_id,
            timeout_at: if duration_ms > 0 {
                Some(ctx.now_ms + duration_ms)
            } else {
                None
            },
        })
    }
}

/// Approval steps always park for a human (or a review agent); resuming them
/// is the scheduler's job via `resume_from_review`.
pub struct ApprovalRunner;

#[async_trait]
impl StepRunner for ApprovalRunner {
    async fn run(&self, _ctx: &StepContext, _task: &Task, step: &TaskStep) -> StepOutcome {
        let reason = step
            .input
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("approval required")
            .to_string();
        StepOutcome::NeedsReview { reason }
    }
}

/// Notification runner with an injected sender so tests never touch the OS.
pub struct FnRunner<F>
where
    F: Fn(&StepContext, &Task, &TaskStep) -> StepOutcome + Send + Sync,
{
    func: F,
}

impl<F> FnRunner<F>
where
    F: Fn(&StepContext, &Task, &TaskStep) -> StepOutcome + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

#[async_trait]
impl<F> StepRunner for FnRunner<F>
where
    F: Fn(&StepContext, &Task, &TaskStep) -> StepOutcome + Send + Sync,
{
    async fn run(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome {
        (self.func)(ctx, task, step)
    }
}

/// Builds runners with `FnRunner` stubs for the host-integrated step types.
/// The real host wires notification/avatar/tool/agent to their adapters.
pub fn test_runners() -> Runners {
    Runners {
        wait: Arc::new(WaitRunner),
        notification: Arc::new(FnRunner::new(|_, _, step| {
            StepOutcome::Completed(serde_json::json!({ "notified": true, "input": step.input }))
        })),
        avatar_routine: Arc::new(FnRunner::new(|_, _, _| StepOutcome::Failed {
            message: "avatar routine runner not wired".to_string(),
            retryable: false,
        })),
        tool: Arc::new(FnRunner::new(|_, _, _| StepOutcome::Failed {
            message: "tool runner not wired".to_string(),
            retryable: false,
        })),
        agent: Arc::new(FnRunner::new(|_, _, _| StepOutcome::Failed {
            message: "agent runner not wired".to_string(),
            retryable: false,
        })),
        approval: Arc::new(ApprovalRunner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::model::{priority, NewTask, NewTaskStep};
    use crate::store::SqliteTaskStore;

    fn setup() -> (
        Arc<SqliteTaskStore>,
        Arc<ManualClock>,
        Executor<SqliteTaskStore>,
    ) {
        let store = Arc::new(SqliteTaskStore::open_in_memory().expect("store"));
        let clock = ManualClock::new(1_000);
        let executor = Executor::new(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            test_runners(),
            60_000,
        );
        (store, clock, executor)
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
    async fn executes_notification_task_to_completion() {
        let (store, _, executor) = setup();
        let mut task = store.create(notify_task(), 1_000).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 1_000).await.expect("ready");
        let done = executor.execute_task(&task.id).await.expect("execute");
        assert_eq!(done.status, TaskStatus::Completed);
        assert!(done.result.is_some());
        let events = store.list_events(&task.id, 50).await.expect("events");
        assert!(events.iter().any(|e| e.event_type == event_type::COMPLETED));
    }

    #[tokio::test]
    async fn retryable_failure_requeues_with_backoff() {
        let (store, _, _) = setup();
        let runners = Runners {
            notification: Arc::new(FnRunner::new(|_, _, _| StepOutcome::Failed {
                message: "boom".to_string(),
                retryable: true,
            })),
            ..test_runners()
        };
        let clock = ManualClock::new(1_000);
        let executor = Executor::new(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            runners,
            60_000,
        );
        let mut task = store.create(notify_task(), 1_000).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 1_000).await.expect("ready");
        let parked = executor.execute_task(&task.id).await.expect("execute");
        assert_eq!(parked.status, TaskStatus::Ready);
        assert!(parked.next_attempt_at.unwrap_or(0) > 1_000);
        // Exhaust attempts: max_attempts = 3, runs 2 and 3 still requeue/fail.
        clock.set(100_000);
        let parked = executor.execute_task(&task.id).await.expect("run2");
        assert_eq!(parked.status, TaskStatus::Ready);
        clock.set(200_000);
        let failed = executor.execute_task(&task.id).await.expect("run3");
        assert_eq!(failed.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn wait_step_parks_until_timeout() {
        let (store, _, executor) = setup();
        let mut task_input = notify_task();
        task_input.steps = vec![NewTaskStep {
            step_type: TaskStepType::Wait,
            input: serde_json::json!({ "duration_ms": 5_000 }),
            max_attempts: None,
        }];
        let mut task = store.create(task_input, 1_000).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 1_000).await.expect("ready");
        let waiting = executor.execute_task(&task.id).await.expect("execute");
        assert_eq!(waiting.status, TaskStatus::Waiting);
        assert!(waiting.wait_for.is_some());
    }

    #[tokio::test]
    async fn verification_failure_retries_the_failed_step_only() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let (store, clock, _) = setup();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let runners = Runners {
            avatar_routine: Arc::new(FnRunner::new(move |_, _, _| {
                let call = calls_clone.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    StepOutcome::Completed(serde_json::json!({
                        "status": "failed",
                        "completed_steps": [],
                    }))
                } else {
                    StepOutcome::Completed(serde_json::json!({
                        "status": "success",
                        "completed_steps": ["wave"],
                    }))
                }
            })),
            ..test_runners()
        };
        let executor = Executor::new(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            runners,
            60_000,
        );
        let mut input = notify_task();
        input.verification = Some(crate::model::VerificationSpec::AvatarRoutine {
            expected_steps: vec!["wave".to_string()],
        });
        input.steps = vec![NewTaskStep {
            step_type: TaskStepType::AvatarRoutine,
            input: serde_json::json!({}),
            max_attempts: None,
        }];
        let mut task = store.create(input, 1_000).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 1_000).await.expect("ready");

        // First run: bad receipt → verification fails → step reset, requeued.
        let requeued = executor.execute_task(&task.id).await.expect("run1");
        assert_eq!(requeued.status, TaskStatus::Ready);
        assert_eq!(requeued.steps[0].status, TaskStepStatus::Pending);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second run: good receipt → verified → completed.
        clock.set(100_000);
        let done = executor.execute_task(&task.id).await.expect("run2");
        assert_eq!(done.status, TaskStatus::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn approval_step_parks_for_review() {
        let (store, _, executor) = setup();
        let mut task_input = notify_task();
        task_input.steps = vec![NewTaskStep {
            step_type: TaskStepType::Approval,
            input: serde_json::json!({ "reason": "check this" }),
            max_attempts: None,
        }];
        let mut task = store.create(task_input, 1_000).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 1_000).await.expect("ready");
        let parked = executor.execute_task(&task.id).await.expect("execute");
        assert_eq!(parked.status, TaskStatus::NeedsReview);
    }
}
