//! Native task host: durable scheduler + host step runners.
//!
//! Boundary: Rust decides WHAT must happen, WHEN, and whether it succeeded.
//! The renderer (TypeScript) decides HOW the VRM looks while doing it — it
//! produces routine receipts; this host verifies them before completing.

pub mod interval;
pub mod runners;

use runners::TICKET_STASH_TTL;
pub use runners::{
    AgentRunner, AgentStepBackend, AvatarRoutineRunner, CapabilityReviewRequest, LimitedRunner,
    NotificationRunner, ToolRunner,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use task_core::{
    Clock, Executor, Ms, NewTask, Runners, Scheduler, SqliteTaskStore, SystemClock, Task,
    TaskStatus, TaskStore, TickReport,
};
use thiserror::Error;

pub type EmitFn = Arc<dyn Fn(ipc_core::HostEvent) + Send + Sync>;
pub type TaskResult<T> = Result<T, TaskHostError>;

#[derive(Debug, Error)]
pub enum TaskHostError {
    #[error(transparent)]
    Core(#[from] task_core::TaskCoreError),
    #[error("task is not parked for review: {0}")]
    NotParked(String),
}

/// Single-use approval ticket: minted by [`TaskHost::review`] after the user
/// approves exactly one gated step, consumed by the next run of that step.
struct StashedTicket {
    ticket: capability_core::CapabilityTicket,
    invocation_id: capability_core::InvocationId,
    stored_at: Instant,
}

/// Shared host state behind the runners.
pub struct HostServices {
    pub registry: Arc<tool_core::ToolRegistry>,
    pub(crate) tickets: Mutex<HashMap<(String, String), StashedTicket>>,
}

impl HostServices {
    pub fn new(registry: Arc<tool_core::ToolRegistry>) -> Self {
        Self {
            registry,
            tickets: Mutex::new(HashMap::new()),
        }
    }

    pub fn task_principal(&self) -> capability_core::Principal {
        capability_core::Principal::Agent(capability_core::AgentId::new("task-host"))
    }

    pub fn stash_ticket(
        &self,
        task_id: &str,
        step_id: &str,
        ticket: capability_core::CapabilityTicket,
        invocation_id: capability_core::InvocationId,
    ) {
        if let Ok(mut tickets) = self.tickets.lock() {
            tickets.insert(
                (task_id.to_string(), step_id.to_string()),
                StashedTicket {
                    ticket,
                    invocation_id,
                    stored_at: Instant::now(),
                },
            );
        }
    }

    /// Check whether a stashed ticket covers a request, without consuming
    /// it. The agent backend's authorizer peeks during `authorize` (which
    /// may run once per requirement) and consumes during `commit` (which
    /// runs once per call).
    pub fn peek_ticket_allows(
        &self,
        task_id: &str,
        step_id: &str,
        capability: &capability_core::Capability,
        resource: &capability_core::Resource,
    ) -> bool {
        let Ok(tickets) = self.tickets.lock() else {
            return false;
        };
        let Some(stashed) = tickets.get(&(task_id.to_string(), step_id.to_string())) else {
            return false;
        };
        if stashed.stored_at.elapsed() > TICKET_STASH_TTL {
            return false;
        }
        stashed.ticket.capability == *capability && stashed.ticket.scope.allows(resource)
    }

    /// Take (consume) a stashed ticket. Expired or missing tickets return
    /// `None` and the step parks for review again — fail closed.
    pub fn take_ticket(
        &self,
        task_id: &str,
        step_id: &str,
        _fresh: capability_core::InvocationId,
    ) -> Option<ConsumedTicket> {
        let mut tickets = self.tickets.lock().ok()?;
        let key = (task_id.to_string(), step_id.to_string());
        let stashed = tickets.remove(&key)?;
        if stashed.stored_at.elapsed() > TICKET_STASH_TTL {
            return None;
        }
        Some(ConsumedTicket {
            ticket: stashed.ticket,
            invocation_id: stashed.invocation_id,
        })
    }
}

/// Single-use ticket handed to exactly one step run.
pub struct ConsumedTicket {
    pub ticket: capability_core::CapabilityTicket,
    pub invocation_id: capability_core::InvocationId,
}

/// Step-type concurrency bounds across all workers. Agent turns are the
/// long pole (model latency); avatar dispatches park immediately after
/// emitting their request.
pub const AGENT_STEP_CONCURRENCY: usize = 1;
pub const AVATAR_STEP_CONCURRENCY: usize = 1;
pub const TOOL_STEP_CONCURRENCY: usize = 2;
pub const NOTIFICATION_STEP_CONCURRENCY: usize = 2;
/// Dispatch queue depth. A full queue leaves tasks `Ready` for the next
/// tick instead of stalling it — the tick loop must never block.
pub const DISPATCH_CHANNEL_SIZE: usize = 64;

/// One ready task handed from the tick loop to a worker.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub task_id: String,
    pub queued_at: Ms,
}

/// Worker pool sizing. Step-type bounds live on the runners (see the
/// `*_STEP_CONCURRENCY` constants); this only sizes the task-driving loops.
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    pub task_workers: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self { task_workers: 4 }
    }
}

pub struct TaskHost {
    store: Arc<SqliteTaskStore>,
    scheduler: Scheduler<SqliteTaskStore>,
    services: Arc<HostServices>,
    clock: Arc<SystemClock>,
    /// Wake signal for the single background tick loop. Mutating IPC calls
    /// notify instead of ticking inline, so responses return immediately.
    wake: Arc<tokio::sync::Notify>,
    dispatch_tx: tokio::sync::mpsc::Sender<DispatchRequest>,
    /// Taken exactly once by [`run_worker_pool`]; `None` afterwards.
    dispatch_rx: Mutex<Option<tokio::sync::mpsc::Receiver<DispatchRequest>>>,
    /// True once workers run. Ticks dispatch only when set; without
    /// workers (tests, task-cli) ticks execute inline, as before.
    dispatch_active: AtomicBool,
    /// Task ids already queued or executing. Prevents double-dispatch
    /// between ticks while a worker is still running the task.
    in_flight: Mutex<HashSet<String>>,
}

impl TaskHost {
    pub fn open(
        db_path: &Path,
        registry: Arc<tool_core::ToolRegistry>,
        emit: EmitFn,
        agent_backend: Option<Arc<dyn AgentStepBackend>>,
    ) -> TaskResult<Self> {
        let store = Arc::new(SqliteTaskStore::open(db_path)?);
        let services = Arc::new(HostServices::new(registry));
        Self::from_store(store, services, emit, agent_backend)
    }

    pub fn open_in_memory(
        registry: Arc<tool_core::ToolRegistry>,
        emit: EmitFn,
        agent_backend: Option<Arc<dyn AgentStepBackend>>,
    ) -> TaskResult<Self> {
        let store = Arc::new(SqliteTaskStore::open_in_memory()?);
        let services = Arc::new(HostServices::new(registry));
        Self::from_store(store, services, emit, agent_backend)
    }

    /// Open with externally built services, so an agent backend can share
    /// the same ticket stash the runners and review handler use.
    pub fn open_with_services(
        db_path: &Path,
        services: Arc<HostServices>,
        emit: EmitFn,
        agent_backend: Option<Arc<dyn AgentStepBackend>>,
    ) -> TaskResult<Self> {
        let store = Arc::new(SqliteTaskStore::open(db_path)?);
        Self::from_store(store, services, emit, agent_backend)
    }

    pub fn open_in_memory_with_services(
        services: Arc<HostServices>,
        emit: EmitFn,
        agent_backend: Option<Arc<dyn AgentStepBackend>>,
    ) -> TaskResult<Self> {
        let store = Arc::new(SqliteTaskStore::open_in_memory()?);
        Self::from_store(store, services, emit, agent_backend)
    }

    fn from_store(
        store: Arc<SqliteTaskStore>,
        services: Arc<HostServices>,
        emit: EmitFn,
        agent_backend: Option<Arc<dyn AgentStepBackend>>,
    ) -> TaskResult<Self> {
        let clock = Arc::new(SystemClock);
        let agent_inner = match agent_backend {
            Some(backend) => AgentRunner::new(backend),
            None => AgentRunner::not_wired(),
        }
        .with_lease_renewal(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            task_core::recovery::DEFAULT_LEASE_MS,
        );
        let runners = Runners {
            wait: Arc::new(task_core::WaitRunner),
            notification: Arc::new(LimitedRunner::new(
                Arc::new(NotificationRunner::new(services.clone())),
                NOTIFICATION_STEP_CONCURRENCY,
                "notification",
            )),
            avatar_routine: Arc::new(LimitedRunner::new(
                Arc::new(AvatarRoutineRunner::new(emit)),
                AVATAR_STEP_CONCURRENCY,
                "avatar",
            )),
            tool: Arc::new(LimitedRunner::new(
                Arc::new(ToolRunner::new(services.clone())),
                TOOL_STEP_CONCURRENCY,
                "tool",
            )),
            agent: Arc::new(LimitedRunner::new(
                Arc::new(agent_inner),
                AGENT_STEP_CONCURRENCY,
                "agent",
            )),
            approval: Arc::new(task_core::ApprovalRunner),
        };
        let executor = Executor::new(
            store.clone(),
            clock.clone() as Arc<dyn Clock>,
            runners,
            task_core::recovery::DEFAULT_LEASE_MS,
        );
        let scheduler = Scheduler::new(
            store.clone(),
            executor,
            clock.clone() as Arc<dyn Clock>,
            task_core::scheduler::DEFAULT_BATCH_LIMIT,
        );
        let (dispatch_tx, dispatch_rx) = tokio::sync::mpsc::channel(DISPATCH_CHANNEL_SIZE);
        Ok(Self {
            store,
            scheduler,
            services,
            clock,
            wake: Arc::new(tokio::sync::Notify::new()),
            dispatch_tx,
            dispatch_rx: Mutex::new(Some(dispatch_rx)),
            dispatch_active: AtomicBool::new(false),
            in_flight: Mutex::new(HashSet::new()),
        })
    }

    pub fn now_ms(&self) -> Ms {
        self.clock.now_ms()
    }

    /// Wake the background tick loop. Mutating IPC calls notify instead of
    /// ticking inline, so the response returns without waiting on recovery,
    /// promotion, or any other task's execution.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// True once [`run_worker_pool`] runs. IPC uses this to decide between
    /// wake-only (production) and the degraded inline tick (no workers).
    pub fn has_workers(&self) -> bool {
        self.dispatch_active.load(Ordering::SeqCst)
    }

    /// One scheduling pass. With workers running this is dispatch-only:
    /// bookkeeping plus enqueue, returning before any step executes. Without
    /// workers (tests, task-cli) it executes inline, as before.
    pub async fn tick(&self) -> TaskResult<TickReport> {
        if !self.dispatch_active.load(Ordering::SeqCst) {
            return Ok(self.scheduler.tick().await?);
        }
        let (mut report, batch) = self.scheduler.collect_ready().await?;
        for task_id in batch {
            if !self.mark_in_flight(&task_id) {
                continue;
            }
            let queued_at = self.now_ms();
            match self.dispatch_tx.try_send(DispatchRequest {
                task_id: task_id.clone(),
                queued_at,
            }) {
                Ok(()) => {
                    tracing::info!(task_id = %task_id, queued_at, "task queued for worker");
                    report.dispatched.push(task_id);
                }
                Err(_) => {
                    self.unmark_in_flight(&task_id);
                    tracing::warn!(task_id = %task_id, "dispatch queue full; task stays ready");
                }
            }
        }
        Ok(report)
    }

    /// Execute one task outside the tick loop (worker path). Clears the
    /// in-flight mark on every exit so a later tick can redispatch retries.
    pub async fn execute_task(&self, task_id: &str) -> TaskResult<Task> {
        let started_at = self.now_ms();
        tracing::info!(task_id = %task_id, started_at, "task dispatched to worker");
        let outcome = self.scheduler.execute_task(task_id).await;
        self.unmark_in_flight(task_id);
        let finished_at = self.now_ms();
        match &outcome {
            Ok(task) => tracing::info!(
                task_id = %task_id,
                status = %task.status.as_str(),
                started_at,
                finished_at,
                "task worker finished"
            ),
            Err(err) => tracing::warn!(
                task_id = %task_id,
                %err,
                started_at,
                finished_at,
                "task worker ended with error"
            ),
        }
        Ok(outcome?)
    }

    fn mark_in_flight(&self, task_id: &str) -> bool {
        self.in_flight
            .lock()
            .map(|mut set| set.insert(task_id.to_string()))
            .unwrap_or(false)
    }

    fn unmark_in_flight(&self, task_id: &str) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(task_id);
        }
    }

    /// Scheduling pass that executes ONLY `task_id` (see
    /// `Scheduler::tick_one`). Single-task headless drivers use this so
    /// a stranger's slow model turn can never delay wait expiry.
    pub async fn tick_one(&self, task_id: &str) -> TaskResult<TickReport> {
        Ok(self.scheduler.tick_one(task_id).await?)
    }

    pub async fn create(&self, task: NewTask) -> TaskResult<Task> {
        let now = self.now_ms();
        Ok(self.store.create(task, now).await?)
    }

    pub async fn get(&self, task_id: &str) -> TaskResult<Option<Task>> {
        Ok(self.store.get(task_id).await?)
    }

    pub async fn list(&self, status: Option<TaskStatus>, limit: i64) -> TaskResult<Vec<Task>> {
        Ok(self.store.list(status, limit.clamp(1, 200)).await?)
    }

    pub async fn cancel(&self, task_id: &str, reason: String) -> TaskResult<Task> {
        Ok(self.scheduler.cancel(task_id, reason).await?)
    }

    pub async fn deliver_event(
        &self,
        event_type: &str,
        correlation_id: Option<&str>,
        payload: serde_json::Value,
    ) -> TaskResult<Option<Task>> {
        Ok(self
            .scheduler
            .deliver_event(event_type, correlation_id, payload)
            .await?)
    }

    /// Resolve a parked review. Approving a capability-gated step mints a
    /// single-use ticket for exactly the approved (capability, resource) and
    /// the step retries once with it. Approving a plain approval step (or a
    /// non-capability review) just resumes.
    pub async fn review(
        &self,
        task_id: &str,
        approved: bool,
        note: Option<String>,
    ) -> TaskResult<Task> {
        let task = self
            .store
            .get(task_id)
            .await?
            .ok_or_else(|| task_core::TaskCoreError::NotFound(task_id.to_string()))?;
        if task.status != TaskStatus::NeedsReview {
            return Err(TaskHostError::NotParked(task_id.to_string()));
        }
        let capability_gated = task
            .last_error
            .as_ref()
            .and_then(|err| CapabilityReviewRequest::parse(&err.message))
            .is_some();
        if approved {
            if let Some(request) = task
                .last_error
                .as_ref()
                .and_then(|err| CapabilityReviewRequest::parse(&err.message))
            {
                let step_id = task
                    .steps
                    .get(task.current_step_index)
                    .map(|step| step.id.clone())
                    .unwrap_or_default();
                let invocation_id = capability_core::InvocationId::fresh();
                let ticket = capability_core::CapabilityTicket::mint(
                    self.services.task_principal(),
                    request.capability,
                    capability_core::ResourceScope::new(vec![request.resource]),
                    invocation_id,
                    Duration::from_secs(5 * 60),
                );
                self.services
                    .stash_ticket(task_id, &step_id, ticket, invocation_id);
            }
        }
        let resolved = self
            .scheduler
            .resume_from_review(task_id, approved, note)
            .await?;
        tracing::info!(
            task_id = %task_id,
            approved,
            capability_gated,
            status = %resolved.status.as_str(),
            "task review resolved"
        );
        Ok(resolved)
    }

    /// Inspect a parked review without resolving it: returns the structured
    /// capability request when the review is capability-gated.
    pub async fn inspect_review(
        &self,
        task_id: &str,
    ) -> TaskResult<Option<CapabilityReviewRequest>> {
        let task = self
            .store
            .get(task_id)
            .await?
            .ok_or_else(|| task_core::TaskCoreError::NotFound(task_id.to_string()))?;
        if task.status != TaskStatus::NeedsReview {
            return Err(TaskHostError::NotParked(task_id.to_string()));
        }
        Ok(task
            .last_error
            .as_ref()
            .and_then(|err| CapabilityReviewRequest::parse(&err.message)))
    }
}

impl From<TaskHostError> for task_core::TaskCoreError {
    fn from(value: TaskHostError) -> Self {
        match value {
            TaskHostError::Core(inner) => inner,
            other => task_core::TaskCoreError::Step(other.to_string()),
        }
    }
}

/// Background tick loop: recover, expire, promote, dispatch, sleep. The
/// single scheduling authority — mutating IPC calls only wake it via
/// [`TaskHost::wake`], never tick inline. Logs and continues on tick
/// errors: a poisoned tick must never kill the loop.
pub async fn run_tick_loop(host: Arc<TaskHost>, interval: Duration) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = host.wake.notified() => {},
        }
        match host.tick().await {
            Ok(report) => {
                if !report.errors.is_empty() {
                    tracing::warn!(errors = ?report.errors, "task tick errors");
                }
                if !report.recovered.is_empty() || !report.exhausted.is_empty() {
                    tracing::info!(
                        recovered = report.recovered.len(),
                        exhausted = report.exhausted.len(),
                        "task recovery pass"
                    );
                }
            }
            Err(err) => tracing::warn!(%err, "task tick failed"),
        }
    }
}

/// Worker pool: the only task executor in production. Takes the dispatch
/// receiver exactly once (a second pool logs and exits), activates
/// dispatch-mode ticks, then drives ready tasks independently of the tick
/// loop until the host drops. Runs forever by design.
pub async fn run_worker_pool(host: Arc<TaskHost>, config: WorkerConfig) {
    let rx = host
        .dispatch_rx
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    let Some(rx) = rx else {
        tracing::error!("task worker pool already started; ignoring second pool");
        return;
    };
    host.dispatch_active.store(true, Ordering::SeqCst);
    let shared = Arc::new(tokio::sync::Mutex::new(rx));
    let mut joins = Vec::new();
    for id in 0..config.task_workers.max(1) {
        let host = Arc::clone(&host);
        let shared = Arc::clone(&shared);
        joins.push(tokio::spawn(async move {
            loop {
                let request = { shared.lock().await.recv().await };
                let Some(request) = request else { break };
                tracing::debug!(
                    task_id = %request.task_id,
                    worker = id,
                    queued_at = request.queued_at,
                    "worker picked up task"
                );
                if let Err(err) = host.execute_task(&request.task_id).await {
                    tracing::debug!(
                        task_id = %request.task_id,
                        %err,
                        "worker execution ended"
                    );
                }
            }
        }));
    }
    for join in joins {
        let _ = join.await;
    }
}

#[allow(unused_imports)]
pub use runners::{
    AVATAR_ROUTINE_COMPLETED_EVENT, AVATAR_ROUTINE_REQUESTED_EVENT,
    DEFAULT_ROUTINE_RECEIPT_TIMEOUT_MS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use task_core::{NewTaskStep, TaskStepType, VerificationSpec};

    struct GatedTool;

    #[async_trait]
    impl tool_core::Tool for GatedTool {
        fn metadata(&self) -> tool_core::ToolMetadata {
            tool_core::ToolMetadata {
                id: capability_core::ToolId::new("test.gated"),
                description: "gated".to_string(),
                input_schema: serde_json::json!({}),
                effects: vec![tool_core::ToolEffect::ExternalSideEffect],
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
            ctx: tool_core::ToolContext,
            _args: serde_json::Value,
        ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
            if !ctx.has_ticket(
                capability_core::Capability::NotificationSend,
                capability_core::Resource::NotificationService,
            ) {
                return Err(tool_core::ToolError::structured(
                    "test.gated",
                    "permission_required",
                    "needs approval",
                ));
            }
            Ok(tool_core::ToolOutput::json(
                serde_json::json!({ "ok": true }),
            ))
        }
    }

    fn registry() -> Arc<tool_core::ToolRegistry> {
        let mut registry = tool_core::ToolRegistry::new();
        registry.register(Arc::new(GatedTool)).unwrap();
        Arc::new(registry)
    }

    fn silent_emit() -> EmitFn {
        Arc::new(|_| {})
    }

    fn gated_task() -> NewTask {
        NewTask {
            title: "gated".to_string(),
            instruction: "gated".to_string(),
            scheduled_at: None,
            priority: None,
            max_attempts: Some(3),
            verification: None,
            parent_task_id: None,
            steps: vec![NewTaskStep {
                step_type: TaskStepType::Tool,
                input: serde_json::json!({ "tool": "test.gated", "args": {} }),
                max_attempts: Some(3),
            }],
        }
    }

    #[test]
    fn peek_checks_scope_without_consuming() {
        let services = HostServices::new(registry());
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
        services.stash_ticket("t", "s", ticket, invocation);
        assert!(services.peek_ticket_allows(
            "t",
            "s",
            &capability_core::Capability::NotificationSend,
            &capability_core::Resource::NotificationService,
        ));
        assert!(!services.peek_ticket_allows(
            "t",
            "s",
            &capability_core::Capability::DesktopControl,
            &capability_core::Resource::NotificationService,
        ));
        // Peeked twice, still present for the real consume.
        assert!(services.take_ticket("t", "s", invocation).is_some());
        assert!(!services.peek_ticket_allows(
            "t",
            "s",
            &capability_core::Capability::NotificationSend,
            &capability_core::Resource::NotificationService,
        ));
    }

    #[tokio::test]
    async fn approval_mints_single_use_ticket_and_step_succeeds() {
        let host = TaskHost::open_in_memory(registry(), silent_emit(), None).expect("host");
        let created = host.create(gated_task()).await.expect("create");
        host.tick().await.expect("tick");
        let parked = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(parked.status, TaskStatus::NeedsReview);

        let request = host
            .inspect_review(&created.id)
            .await
            .expect("inspect")
            .expect("cap");
        assert_eq!(request.tool, "test.gated");

        host.review(&created.id, true, None).await.expect("approve");
        host.tick().await.expect("tick2");
        let done = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    }

    #[tokio::test]
    async fn rejection_fails_the_task() {
        let host = TaskHost::open_in_memory(registry(), silent_emit(), None).expect("host");
        let created = host.create(gated_task()).await.expect("create");
        host.tick().await.expect("tick");
        let rejected = host
            .review(&created.id, false, Some("no".to_string()))
            .await
            .expect("reject");
        assert_eq!(rejected.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn avatar_receipt_completes_routine_task() {
        let events: Arc<Mutex<Vec<ipc_core::HostEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: EmitFn = Arc::new(move |event| sink.lock().unwrap().push(event));
        let host = TaskHost::open_in_memory(registry(), emit, None).expect("host");
        let task = NewTask {
            title: "wave".to_string(),
            instruction: "wave".to_string(),
            scheduled_at: None,
            priority: None,
            max_attempts: Some(2),
            verification: Some(VerificationSpec::AvatarRoutine {
                expected_steps: vec!["wave".to_string()],
            }),
            parent_task_id: None,
            steps: vec![NewTaskStep {
                step_type: TaskStepType::AvatarRoutine,
                input: serde_json::json!({ "steps": [{ "gesture": "wave" }] }),
                max_attempts: Some(2),
            }],
        };
        let created = host.create(task).await.expect("create");
        host.tick().await.expect("tick");
        let waiting = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(waiting.status, TaskStatus::Waiting);
        assert_eq!(events.lock().unwrap().len(), 1);

        // Renderer receipt: only the steps the renderer actually completed.
        host.deliver_event(
            AVATAR_ROUTINE_COMPLETED_EVENT,
            Some(&created.id),
            serde_json::json!({ "status": "success", "completed_steps": ["wave"] }),
        )
        .await
        .expect("deliver");
        host.tick().await.expect("tick2");
        let done = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    }

    #[tokio::test]
    async fn partial_receipt_fails_verification() {
        let host = TaskHost::open_in_memory(registry(), silent_emit(), None).expect("host");
        let task = NewTask {
            title: "wave".to_string(),
            instruction: "wave".to_string(),
            scheduled_at: None,
            priority: None,
            max_attempts: Some(1),
            verification: Some(VerificationSpec::AvatarRoutine {
                expected_steps: vec!["wave".to_string(), "nod".to_string()],
            }),
            parent_task_id: None,
            steps: vec![NewTaskStep {
                step_type: TaskStepType::AvatarRoutine,
                input: serde_json::json!({ "steps": [] }),
                max_attempts: Some(1),
            }],
        };
        let created = host.create(task).await.expect("create");
        host.tick().await.expect("tick");
        host.deliver_event(
            AVATAR_ROUTINE_COMPLETED_EVENT,
            Some(&created.id),
            serde_json::json!({ "status": "success", "completed_steps": ["wave"] }),
        )
        .await
        .expect("deliver");
        host.tick().await.expect("tick2");
        let done = host.get(&created.id).await.expect("get").expect("task");
        // One attempt only: verification failure is terminal.
        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
    }

    #[tokio::test]
    async fn wait_step_survives_across_ticks() {
        let host = TaskHost::open_in_memory(registry(), silent_emit(), None).expect("host");
        let mut task = gated_task();
        task.steps = vec![NewTaskStep {
            step_type: TaskStepType::Wait,
            input: serde_json::json!({ "duration_ms": 50 }),
            max_attempts: Some(1),
        }];
        task.max_attempts = Some(1);
        let created = host.create(task).await.expect("create");
        host.tick().await.expect("tick");
        let waiting = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(waiting.status, TaskStatus::Waiting);
        tokio::time::sleep(Duration::from_millis(80)).await;
        host.tick().await.expect("tick2");
        let done = host.get(&created.id).await.expect("get").expect("task");
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    }

    #[tokio::test]
    async fn dispatch_tick_enqueues_and_worker_executes() {
        let host =
            Arc::new(TaskHost::open_in_memory(registry(), silent_emit(), None).expect("host"));
        let mut task = gated_task();
        task.steps = vec![NewTaskStep {
            step_type: TaskStepType::Wait,
            input: serde_json::json!({ "duration_ms": 0 }),
            max_attempts: Some(1),
        }];
        task.max_attempts = Some(1);
        let created = host.create(task).await.expect("create");
        assert!(!host.has_workers());

        tokio::spawn(run_worker_pool(
            Arc::clone(&host),
            WorkerConfig { task_workers: 2 },
        ));
        let start = Instant::now();
        while !host.has_workers() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "worker pool did not activate"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Dispatch-only: the tick enqueues without executing.
        let report = host.tick().await.expect("tick");
        assert_eq!(report.dispatched, vec![created.id.clone()]);
        assert!(report.executed.is_empty());
        let queued = host.get(&created.id).await.expect("get").expect("task");
        assert_ne!(queued.status, TaskStatus::Completed);

        // The worker executes independently of the tick loop.
        let start = Instant::now();
        loop {
            let current = host.get(&created.id).await.expect("get").expect("task");
            if current.status == TaskStatus::Completed {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "worker did not finish: {current:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
