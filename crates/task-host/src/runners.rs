//! Host step runners: tool/registry, notification, avatar-routine, agent.
//!
//! Authority rule: runners never mint their own permission. A tool that
//! needs a capability the task does not hold parks in `needs_review` with
//! a structured capability request; [`TaskHost::review`] mints a
//! single-use ticket on approval and the step retries exactly once with it.

use crate::{EmitFn, HostServices};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use task_core::{
    Clock, ExecutionReceipt, Ms, ReceiptStatus, StepContext, StepOutcome, StepRunner, Task,
    TaskStep, TaskStore, TaskWait,
};
use tool_core::{ToolContext, ToolError};

pub const AVATAR_ROUTINE_REQUESTED_EVENT: &str = ipc_core::events::AVATAR_ROUTINE_REQUESTED;
pub const AVATAR_ROUTINE_COMPLETED_EVENT: &str = ipc_core::events::AVATAR_ROUTINE_COMPLETED;
/// How long the host waits for the renderer's routine receipt. The
/// WebView side (`DEFAULT_RUN_TIMEOUT_MS` in
/// `src/lib/tasks/host-avatar.ts`) always runs strictly under this
/// budget (55s default, or budget minus `RECEIPT_MARGIN_MS`) so a slow
/// routine resolves locally instead of racing the host wait.
pub const DEFAULT_ROUTINE_RECEIPT_TIMEOUT_MS: Ms = 60_000;
/// Single-use approval tickets live at most 5 minutes in the stash.
pub const TICKET_STASH_TTL: Duration = Duration::from_secs(5 * 60);

/// Structured `needs_review` reason for capability-gated steps. Stored as
/// the task's `last_error.message` so the review handler can mint exactly
/// the ticket the user approved — nothing wider.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CapabilityReviewRequest {
    pub kind: String,
    pub tool: String,
    pub capability: capability_core::Capability,
    pub resource: capability_core::Resource,
    pub detail: String,
}

impl CapabilityReviewRequest {
    pub fn new(
        tool: &str,
        capability: capability_core::Capability,
        resource: capability_core::Resource,
        detail: String,
    ) -> Self {
        Self {
            kind: "capability".to_string(),
            tool: tool.to_string(),
            capability,
            resource,
            detail,
        }
    }

    pub fn parse(reason: &str) -> Option<Self> {
        serde_json::from_str::<Self>(reason)
            .ok()
            .filter(|req| req.kind == "capability")
    }
}

/// Durable receipt recording for tool invocations. When wired, every
/// attempt records an [`ExecutionReceipt`] and a retry whose earlier
/// attempt already succeeded replays the stored output instead of
/// re-invoking the tool — so a crash between "effect happened" and
/// "step completed" does not double-fire the side effect.
#[derive(Clone)]
pub struct ReceiptRecorder {
    store: Arc<task_core::SqliteTaskStore>,
    clock: Arc<dyn Clock>,
}

/// One attempt's receipt fields, bundled so the recorder stays readable.
struct AttemptRecord<'a> {
    ctx: &'a StepContext,
    step: &'a TaskStep,
    tool_name: &'a str,
    status: ReceiptStatus,
    started_at: Ms,
    external_id: Option<String>,
    output: serde_json::Value,
}

/// Runs any registered tool by name. Input: `{ "tool": "...", "args": {...} }`.
pub struct ToolRunner {
    services: Arc<HostServices>,
    receipts: Option<ReceiptRecorder>,
}

impl ToolRunner {
    pub fn new(services: Arc<HostServices>) -> Self {
        Self {
            services,
            receipts: None,
        }
    }

    /// Record an execution receipt per attempt and replay prior successes.
    /// Unwired runners keep the old behavior (hosts without receipt needs,
    /// and unit tests that construct runners directly).
    pub fn with_receipts(
        mut self,
        store: Arc<task_core::SqliteTaskStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        self.set_receipts(store, clock);
        self
    }

    pub fn set_receipts(&mut self, store: Arc<task_core::SqliteTaskStore>, clock: Arc<dyn Clock>) {
        self.receipts = Some(ReceiptRecorder { store, clock });
    }

    /// Execute one tool call; shared by the `tool` step type and the
    /// `notification` step type (which is just `notification.show`).
    pub async fn invoke_tool(
        &self,
        ctx: &StepContext,
        step: &TaskStep,
        tool_name: &str,
        args: serde_json::Value,
    ) -> StepOutcome {
        let tool = match self.services.registry.resolve(tool_name) {
            Ok(tool) => tool,
            Err(err) => {
                return StepOutcome::Failed {
                    message: format!("unknown tool '{tool_name}': {err}"),
                    retryable: false,
                };
            }
        };
        // Idempotent replay: an earlier attempt's success receipt means the
        // effect already went through — reuse it, never re-invoke.
        if let Some(replayed) = self.replay_if_succeeded(ctx, step, tool_name).await {
            return StepOutcome::Completed(replayed);
        }
        let mut tool_ctx = ToolContext::new(self.services.task_principal());
        // Single-use approval ticket, minted by TaskHost::review after the
        // user approved exactly this step. Consumed here, never reused.
        if let Some(stashed) =
            self.services
                .take_ticket(&ctx.task_id, &step.id, tool_ctx.invocation_id)
        {
            tool_ctx.invocation_id = stashed.invocation_id;
            tool_ctx = tool_ctx.with_ticket(stashed.ticket);
        }
        let started_at = self.now_ms(ctx);
        match tool.invoke(tool_ctx, args.clone()).await {
            Ok(output) => {
                let value = serde_json::json!({
                    "tool": tool_name,
                    "status": "success",
                    "output": output.content,
                    "truncated": output.truncated,
                });
                self.record_attempt(AttemptRecord {
                    ctx,
                    step,
                    tool_name,
                    status: ReceiptStatus::Success,
                    started_at,
                    external_id: None,
                    output: value.clone(),
                })
                .await;
                StepOutcome::Completed(value)
            }
            Err(err) => {
                // Timeouts are the honest unknown: the effect may or may
                // not have happened. Everything else failed deterministically.
                let timed_out = matches!(err, ToolError::Timeout(_));
                let outcome = self.map_tool_error(tool_name, &args, err);
                self.record_failure_receipt(ctx, step, tool_name, started_at, timed_out, &outcome)
                    .await;
                outcome
            }
        }
    }

    async fn replay_if_succeeded(
        &self,
        ctx: &StepContext,
        step: &TaskStep,
        tool_name: &str,
    ) -> Option<serde_json::Value> {
        let receipts = self.receipts.as_ref()?;
        receipts
            .store
            .find_success_receipt(&ctx.task_id, &step.id, tool_name)
            .await
            .ok()
            .flatten()
            .map(|receipt| {
                tracing::info!(
                    task_id = %ctx.task_id,
                    step_id = %step.id,
                    operation = %tool_name,
                    execution_id = %receipt.execution_id,
                    "replaying prior success receipt instead of re-invoking tool"
                );
                receipt.output
            })
    }

    fn now_ms(&self, ctx: &StepContext) -> Ms {
        self.receipts
            .as_ref()
            .map(|recorder| recorder.clock.now_ms())
            .unwrap_or(ctx.now_ms)
    }

    async fn record_attempt(&self, attempt: AttemptRecord<'_>) {
        let Some(receipts) = self.receipts.as_ref() else {
            return;
        };
        let AttemptRecord {
            ctx,
            step,
            tool_name,
            status,
            started_at,
            external_id,
            output,
        } = attempt;
        let receipt = ExecutionReceipt {
            execution_id: uuid::Uuid::new_v4().to_string(),
            task_id: ctx.task_id.clone(),
            step_id: step.id.clone(),
            idempotency_key: task_core::idempotency_key(
                &ctx.task_id,
                &step.id,
                tool_name,
                ctx.attempt,
            ),
            operation: tool_name.to_string(),
            status,
            started_at,
            finished_at: Some(receipts.clock.now_ms()),
            external_id,
            output,
        };
        if let Err(err) = receipts.store.record_receipt(&receipt).await {
            // Receipt loss must never fail the step: the step result is the
            // source of truth; the receipt is audit plus replay optimization.
            tracing::warn!(
                task_id = %ctx.task_id,
                step_id = %step.id,
                %err,
                "failed to record execution receipt"
            );
        }
    }

    /// Failure receipts are audit: timeouts record `unknown_outcome` (the
    /// effect may or may not have happened), everything else `failed`.
    /// Neither blocks a later retry — only `success` replays.
    async fn record_failure_receipt(
        &self,
        ctx: &StepContext,
        step: &TaskStep,
        tool_name: &str,
        started_at: Ms,
        timed_out: bool,
        outcome: &StepOutcome,
    ) {
        let StepOutcome::Failed { message, .. } = outcome else {
            return;
        };
        self.record_attempt(AttemptRecord {
            ctx,
            step,
            tool_name,
            status: if timed_out {
                ReceiptStatus::UnknownOutcome
            } else {
                ReceiptStatus::Failed
            },
            started_at,
            external_id: None,
            output: serde_json::json!({ "tool": tool_name, "error": message }),
        })
        .await;
    }

    fn map_tool_error(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        err: ToolError,
    ) -> StepOutcome {
        match err {
            ToolError::Timeout(tool) => StepOutcome::Failed {
                message: format!("tool {tool} timed out"),
                retryable: true,
            },
            ToolError::Filesystem {
                message, retryable, ..
            } => StepOutcome::Failed { message, retryable },
            ToolError::Denied { tool, reason } => self.needs_review(tool_name, &tool, args, reason),
            ToolError::Structured {
                tool,
                code,
                message,
                details,
            } if code == "permission_required" => {
                let detail = details
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&message)
                    .to_string();
                let _ = tool;
                self.needs_review(tool_name, tool_name, args, detail)
            }
            other => StepOutcome::Failed {
                message: other.model_message(),
                retryable: false,
            },
        }
    }

    fn needs_review(
        &self,
        tool_name: &str,
        _resolved: &str,
        args: &serde_json::Value,
        detail: String,
    ) -> StepOutcome {
        let requirement = self
            .services
            .registry
            .resolve(tool_name)
            .ok()
            .and_then(|tool| tool.required_capability(args));
        match requirement {
            Some(req) => {
                let request = CapabilityReviewRequest::new(
                    tool_name,
                    req.capability,
                    req.resource,
                    detail.clone(),
                );
                StepOutcome::NeedsReview {
                    reason: serde_json::to_string(&request).unwrap_or(detail),
                }
            }
            None => StepOutcome::NeedsReview { reason: detail },
        }
    }
}

#[async_trait]
impl StepRunner for ToolRunner {
    async fn run(&self, ctx: &StepContext, _task: &Task, step: &TaskStep) -> StepOutcome {
        let tool_name = step
            .input
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if tool_name.is_empty() {
            return StepOutcome::Failed {
                message: "tool step needs a non-empty string 'tool'".to_string(),
                retryable: false,
            };
        }
        let args = step
            .input
            .get("args")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        self.invoke_tool(ctx, step, tool_name, args).await
    }
}

/// Notification steps are `notification.show` through the same registry +
/// approval path as every other tool call. No ambient authority.
pub struct NotificationRunner {
    inner: ToolRunner,
}

impl NotificationRunner {
    pub fn new(services: Arc<HostServices>) -> Self {
        Self {
            inner: ToolRunner::new(services),
        }
    }

    pub fn with_receipts(
        mut self,
        store: Arc<task_core::SqliteTaskStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        self.inner.set_receipts(store, clock);
        self
    }
}

#[async_trait]
impl StepRunner for NotificationRunner {
    async fn run(&self, ctx: &StepContext, _task: &Task, step: &TaskStep) -> StepOutcome {
        self.inner
            .invoke_tool(ctx, step, "notification.show", step.input.clone())
            .await
    }
}

/// Dispatches the routine to the renderer over `avatar.routine.requested`
/// and parks until the `avatar.routine.completed` receipt arrives (or the
/// receipt timeout expires and the wait is retried/failed like any other).
pub struct AvatarRoutineRunner {
    emit: EmitFn,
}

impl AvatarRoutineRunner {
    pub fn new(emit: EmitFn) -> Self {
        Self { emit }
    }
}

#[async_trait]
impl StepRunner for AvatarRoutineRunner {
    async fn run(&self, ctx: &StepContext, _task: &Task, step: &TaskStep) -> StepOutcome {
        let receipt_timeout_ms = step
            .input
            .get("receipt_timeout_ms")
            .and_then(serde_json::Value::as_i64)
            .filter(|t| *t > 0)
            .unwrap_or(DEFAULT_ROUTINE_RECEIPT_TIMEOUT_MS);
        (self.emit)(ipc_core::HostEvent {
            event: AVATAR_ROUTINE_REQUESTED_EVENT.to_string(),
            data: serde_json::json!({
                "task_id": ctx.task_id,
                "step_id": step.id,
                "routine": step.input,
            }),
        });
        StepOutcome::Wait(TaskWait {
            event_type: Some(AVATAR_ROUTINE_COMPLETED_EVENT.to_string()),
            correlation_id: Some(ctx.task_id.clone()),
            timeout_at: Some(ctx.now_ms + receipt_timeout_ms),
        })
    }
}

/// Backend for `agent` steps. task-host ships only the fail-closed default;
/// app-host injects the real sub-turn backend without task-host changes.
#[async_trait]
pub trait AgentStepBackend: Send + Sync {
    async fn run_agent_step(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome;
}

pub struct AgentRunner {
    backend: Arc<dyn AgentStepBackend>,
    lease_renewal: Option<LeaseRenewal>,
}

/// Lease-renewal context: long agent steps must keep their task lease alive
/// while the model turn runs, or a later recovery pass requeues the task
/// and the step runs twice.
#[derive(Clone)]
pub struct LeaseRenewal {
    store: Arc<task_core::SqliteTaskStore>,
    clock: Arc<dyn task_core::Clock>,
    lease_ms: Ms,
}

impl AgentRunner {
    pub fn new(backend: Arc<dyn AgentStepBackend>) -> Self {
        Self {
            backend,
            lease_renewal: None,
        }
    }

    pub fn not_wired() -> Self {
        Self {
            backend: Arc::new(NotWiredAgentBackend),
            lease_renewal: None,
        }
    }

    /// Renew the task lease periodically while the backend runs. Without
    /// this, any agent step outliving the lease is recovered and retried
    /// while the original turn is still executing.
    pub fn with_lease_renewal(
        mut self,
        store: Arc<task_core::SqliteTaskStore>,
        clock: Arc<dyn task_core::Clock>,
        lease_ms: Ms,
    ) -> Self {
        self.lease_renewal = Some(LeaseRenewal {
            store,
            clock,
            lease_ms,
        });
        self
    }
}

#[async_trait]
impl StepRunner for AgentRunner {
    async fn run(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome {
        // Held to the end of the step: dropping it aborts the renew loop.
        let _renew = self
            .lease_renewal
            .as_ref()
            .map(|renewal| renewal.spawn(&ctx.task_id, &step.id));
        self.backend.run_agent_step(ctx, task, step).await
    }
}

impl LeaseRenewal {
    /// Spawn a renewal loop; the returned guard aborts it on drop. Only
    /// extends the lease while the task is still `Running` — a cancelled
    /// or finished task is left alone so recovery semantics stay intact.
    fn spawn(&self, task_id: &str, step_id: &str) -> RenewGuard {
        let store = Arc::clone(&self.store);
        let clock = Arc::clone(&self.clock);
        let task_id = task_id.to_string();
        let step_id = step_id.to_string();
        // Renew well before expiry: a quarter of the lease, at least 10s so
        // short test leases don't spin.
        let period = Duration::from_millis((self.lease_ms / 4).max(10_000) as u64);
        let lease_ms = self.lease_ms;
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let now = clock.now_ms();
                match store.get(&task_id).await {
                    Ok(Some(mut task)) if task.status == task_core::TaskStatus::Running => {
                        task.lease_until = Some(now + lease_ms);
                        if store.update(&task, now).await.is_ok() {
                            tracing::debug!(
                                task_id = %task_id,
                                step_id = %step_id,
                                lease_until = task.lease_until,
                                "agent step lease renewed"
                            );
                        }
                    }
                    _ => break,
                }
            }
        });
        RenewGuard { handle }
    }
}

struct RenewGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for RenewGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bounds concurrent executions of one step type across all workers.
/// Agent turns are the long pole (model latency); tool and notification
/// calls are short but numerous; avatar dispatches park immediately.
pub struct LimitedRunner {
    inner: Arc<dyn StepRunner>,
    semaphore: Arc<tokio::sync::Semaphore>,
    label: &'static str,
}

impl LimitedRunner {
    pub fn new(inner: Arc<dyn StepRunner>, max_concurrent: usize, label: &'static str) -> Self {
        Self {
            inner,
            semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1))),
            label,
        }
    }
}

#[async_trait]
impl StepRunner for LimitedRunner {
    async fn run(&self, ctx: &StepContext, task: &Task, step: &TaskStep) -> StepOutcome {
        let _permit = match self.semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => {
                return StepOutcome::Failed {
                    message: format!("{} runner shut down", self.label),
                    retryable: true,
                }
            }
        };
        self.inner.run(ctx, task, step).await
    }
}

/// Explicit fail-closed default: agent steps fail with a clear message
/// instead of silently parking in `needs_review` forever.
pub struct NotWiredAgentBackend;

#[async_trait]
impl AgentStepBackend for NotWiredAgentBackend {
    async fn run_agent_step(
        &self,
        _ctx: &StepContext,
        _task: &Task,
        _step: &TaskStep,
    ) -> StepOutcome {
        StepOutcome::Failed {
            message: "agent step backend is not wired on this host; wire an AgentStepBackend to run agent steps"
                .to_string(),
            retryable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostServices;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use task_core::{TaskStatus, TaskStepStatus, TaskStepType};
    use tool_core::{Tool, ToolEffect, ToolMetadata};

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("system.echo"),
                description: "echo".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![],
            }
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, ToolError> {
            Ok(tool_core::ToolOutput::json(args))
        }
    }

    struct TicketTool;

    #[async_trait]
    impl Tool for TicketTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("test.gated"),
                description: "gated".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![ToolEffect::ExternalSideEffect],
            }
        }

        fn required_capability(
            &self,
            _args: &serde_json::Value,
        ) -> Option<tool_core::CapabilityRequirement> {
            Some(tool_core::CapabilityRequirement {
                capability: capability_core::Capability::NotificationSend,
                resource: capability_core::Resource::NotificationService,
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            _args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, ToolError> {
            if !ctx.has_ticket(
                capability_core::Capability::NotificationSend,
                capability_core::Resource::NotificationService,
            ) {
                return Err(ToolError::structured_with_details(
                    "test.gated",
                    "permission_required",
                    "needs approval",
                    serde_json::json!({}),
                ));
            }
            Ok(tool_core::ToolOutput::json(
                serde_json::json!({ "ok": true }),
            ))
        }
    }

    fn services() -> Arc<HostServices> {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        registry.register(Arc::new(TicketTool)).unwrap();
        Arc::new(HostServices {
            registry: Arc::new(registry),
            tickets: Mutex::new(HashMap::new()),
        })
    }

    fn step(input: serde_json::Value) -> TaskStep {
        TaskStep {
            id: "step-1".to_string(),
            step_type: TaskStepType::Tool,
            status: TaskStepStatus::Pending,
            input,
            result: None,
            attempts: 0,
            max_attempts: 1,
            started_at: None,
            finished_at: None,
            error: None,
        }
    }

    fn task() -> Task {
        Task {
            id: "task-1".to_string(),
            title: "t".to_string(),
            instruction: "i".to_string(),
            status: TaskStatus::Running,
            priority: 50,
            created_at: 0,
            updated_at: 0,
            scheduled_at: None,
            started_at: None,
            finished_at: None,
            attempts: 1,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: None,
            wait_for: None,
            steps: Vec::new(),
            current_step_index: 0,
            result: None,
            last_error: None,
            verification: None,
            parent_task_id: None,
        }
    }

    fn ctx() -> StepContext {
        StepContext {
            task_id: "task-1".to_string(),
            attempt: 1,
            timeout_ms: 5_000,
            now_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn pure_tool_completes_with_output() {
        let runner = ToolRunner::new(services());
        let outcome = runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "system.echo", "args": { "a": 1 } })),
            )
            .await;
        match outcome {
            StepOutcome::Completed(value) => {
                assert_eq!(value["tool"], "system.echo");
                assert_eq!(value["status"], "success");
                assert_eq!(value["output"]["a"], 1);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_tool_fails_closed() {
        let runner = ToolRunner::new(services());
        let outcome = runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "nope.missing", "args": {} })),
            )
            .await;
        assert!(matches!(
            outcome,
            StepOutcome::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn gated_tool_parks_with_structured_review_request() {
        let runner = ToolRunner::new(services());
        let outcome = runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "test.gated", "args": {} })),
            )
            .await;
        match outcome {
            StepOutcome::NeedsReview { reason } => {
                let req = CapabilityReviewRequest::parse(&reason).expect("structured");
                assert_eq!(req.tool, "test.gated");
                assert_eq!(
                    req.capability,
                    capability_core::Capability::NotificationSend
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stashed_ticket_is_consumed_single_use() {
        let services = services();
        let outcome_runner = ToolRunner::new(services.clone());
        // Without a ticket the gated tool parks.
        let parked = outcome_runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "test.gated", "args": {} })),
            )
            .await;
        assert!(matches!(parked, StepOutcome::NeedsReview { .. }));
        // Stash a real ticket; the next run consumes it and succeeds.
        let invocation = capability_core::InvocationId::fresh();
        let ticket = capability_core::CapabilityTicket::mint(
            services.task_principal(),
            capability_core::Capability::NotificationSend,
            capability_core::ResourceScope::new(vec![
                capability_core::Resource::NotificationService,
            ]),
            invocation,
            Duration::from_secs(60),
        );
        services.stash_ticket("task-1", "step-1", ticket, invocation);
        let done = outcome_runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "test.gated", "args": {} })),
            )
            .await;
        assert!(matches!(done, StepOutcome::Completed(_)), "{done:?}");
        // Single use: consumed, so the tool parks again.
        let parked_again = outcome_runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "test.gated", "args": {} })),
            )
            .await;
        assert!(matches!(parked_again, StepOutcome::NeedsReview { .. }));
    }

    #[tokio::test]
    async fn avatar_runner_emits_request_and_waits_for_receipt() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: EmitFn = Arc::new(move |event| sink.lock().unwrap().push(event));
        let runner = AvatarRoutineRunner::new(emit);
        let outcome = runner
            .run(&ctx(), &task(), &step(serde_json::json!({ "steps": [] })))
            .await;
        match outcome {
            StepOutcome::Wait(wait) => {
                assert_eq!(
                    wait.event_type.as_deref(),
                    Some(AVATAR_ROUTINE_COMPLETED_EVENT)
                );
                assert_eq!(wait.correlation_id.as_deref(), Some("task-1"));
                assert!(wait.timeout_at.unwrap_or(0) > 1_000);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, AVATAR_ROUTINE_REQUESTED_EVENT);
        assert_eq!(events[0].data["task_id"], "task-1");
    }

    #[tokio::test]
    async fn success_receipt_replays_without_reinvoking() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use task_core::ManualClock;

        struct CountingTool {
            calls: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Tool for CountingTool {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: capability_core::ToolId::new("test.counting"),
                    description: "counting".to_string(),
                    input_schema: serde_json::json!({}),
                    effects: vec![ToolEffect::ExternalSideEffect],
                }
            }

            async fn invoke(
                &self,
                _ctx: ToolContext,
                _args: serde_json::Value,
            ) -> Result<tool_core::ToolOutput, ToolError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(tool_core::ToolOutput::json(serde_json::json!({ "n": 1 })))
            }
        }

        let calls = Arc::new(AtomicU32::new(0));
        let mut registry = tool_core::ToolRegistry::new();
        registry
            .register(Arc::new(CountingTool {
                calls: calls.clone(),
            }))
            .unwrap();
        let services = Arc::new(HostServices::new(Arc::new(registry)));
        let store = Arc::new(task_core::SqliteTaskStore::open_in_memory().expect("store"));
        let clock = ManualClock::new(1_000);
        let runner =
            ToolRunner::new(services).with_receipts(store.clone(), clock.clone() as Arc<dyn Clock>);
        let input = serde_json::json!({ "tool": "test.counting", "args": {} });

        let first = runner.run(&ctx(), &task(), &step(input.clone())).await;
        assert!(matches!(first, StepOutcome::Completed(_)), "{first:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let receipts = store.list_receipts("task-1", 10).await.expect("receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].status, ReceiptStatus::Success);
        assert_eq!(receipts[0].operation, "test.counting");

        // Second attempt (e.g. the completion was lost to a crash): the
        // stored success replays, the tool is NOT invoked again, and no
        // second receipt is written.
        let retry_ctx = StepContext {
            attempt: 2,
            ..ctx()
        };
        let second = runner.run(&retry_ctx, &task(), &step(input)).await;
        match (first, second) {
            (StepOutcome::Completed(a), StepOutcome::Completed(b)) => assert_eq!(a, b),
            other => panic!("unexpected outcomes: {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let receipts = store.list_receipts("task-1", 10).await.expect("receipts");
        assert_eq!(receipts.len(), 1);
    }

    #[tokio::test]
    async fn failed_attempt_does_not_replay() {
        use task_core::ManualClock;

        let store = Arc::new(task_core::SqliteTaskStore::open_in_memory().expect("store"));
        let clock = ManualClock::new(1_000);
        let runner = ToolRunner::new(services())
            .with_receipts(store.clone(), clock.clone() as Arc<dyn Clock>);
        // Deterministic failure (unknown tool): recorded, never replayed.
        let outcome = runner
            .run(
                &ctx(),
                &task(),
                &step(serde_json::json!({ "tool": "nope.missing", "args": {} })),
            )
            .await;
        assert!(matches!(outcome, StepOutcome::Failed { .. }));
        // Unknown tools fail before any invocation, so no receipt exists...
        assert!(store
            .find_success_receipt("task-1", "step-1", "nope.missing")
            .await
            .expect("find")
            .is_none());
        // ...while a failed receipt for a real tool does not replay either.
        let failed = ExecutionReceipt {
            execution_id: uuid::Uuid::new_v4().to_string(),
            task_id: "task-1".to_string(),
            step_id: "step-1".to_string(),
            idempotency_key: task_core::idempotency_key("task-1", "step-1", "system.echo", 1),
            operation: "system.echo".to_string(),
            status: ReceiptStatus::Failed,
            started_at: 1_000,
            finished_at: Some(1_100),
            external_id: None,
            output: serde_json::json!({}),
        };
        assert!(store.record_receipt(&failed).await.expect("record"));
        let retry = runner
            .run(
                &StepContext {
                    attempt: 2,
                    ..ctx()
                },
                &task(),
                &step(serde_json::json!({ "tool": "system.echo", "args": {} })),
            )
            .await;
        assert!(matches!(retry, StepOutcome::Completed(_)), "{retry:?}");
        assert_eq!(
            store.list_receipts("task-1", 10).await.expect("list").len(),
            2
        );
    }

    #[tokio::test]
    async fn unwired_agent_backend_fails_closed() {
        let runner = AgentRunner::not_wired();
        let outcome = runner
            .run(&ctx(), &task(), &step(serde_json::json!({})))
            .await;
        match outcome {
            StepOutcome::Failed { message, retryable } => {
                assert!(!retryable);
                assert!(message.contains("not wired"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    struct ConcurrencyProbe {
        current: Arc<std::sync::atomic::AtomicUsize>,
        max: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl StepRunner for ConcurrencyProbe {
        async fn run(&self, _ctx: &StepContext, _task: &Task, _step: &TaskStep) -> StepOutcome {
            let held = self
                .current
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.max
                .fetch_max(held, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.current
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            StepOutcome::Completed(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn limited_runner_serializes_one_slot() {
        let probe = Arc::new(ConcurrencyProbe {
            current: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let runner = Arc::new(LimitedRunner::new(probe.clone(), 1, "probe"));
        let ctx_a = ctx();
        let task_a = task();
        let step_a = step(serde_json::json!({}));
        let ctx_b = ctx();
        let task_b = task();
        let step_b = step(serde_json::json!({}));
        let (first, second) = tokio::join!(
            runner.run(&ctx_a, &task_a, &step_a),
            runner.run(&ctx_b, &task_b, &step_b)
        );
        assert!(matches!(first, StepOutcome::Completed(_)));
        assert!(matches!(second, StepOutcome::Completed(_)));
        assert_eq!(
            probe.max.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "single slot must never overlap executions"
        );
    }
}
