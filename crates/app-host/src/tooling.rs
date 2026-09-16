//! Per-turn tool composition (refactor step 3).
//!
//! This module owns *what* the agent can call each turn; `runtime`
//! only decides *when* to snapshot. Contents:
//!
//! - [`SystemToolPack`]: read-only host facts (`system.environment`,
//!   `system.time`).
//! - [`ProcessToolPack`]: structured process execution (`process.spawn`,
//!   `process.status`, `process.kill`).
//! - Settings synchronization: the composition root reads settings into
//!   the extension managers once per configuration generation (see
//!   `AgentRuntime::sync_mcp_cached` / `discover_plugins_cached`). The
//!   leaf-crate sources (`McpToolSource`, `PluginToolSource`,
//!   `MemoryToolPack`, `DesktopToolPack`) only collect from
//!   already-configured managers — app-host never learns how their tools
//!   are built.
//!
//! The host filesystem surface arrives via `tool-filesystem-host`'s pack.

use host_core::HostEnvironment;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use task_core::{Clock as _, TaskStore as _};
use task_host::interval::{interval_task, numbered_prompts, IntervalOptions};
use tool_sdk::{ToolLoadContext, ToolPack, TypedTool, TypedToolAdapter};

/// Empty argument object shared by the system-fact tools. Deserializing
/// (rather than asserting `is_object`) keeps unknown-field tolerance
/// identical to the historical manual implementations.
#[derive(Deserialize)]
struct NoArgs {}

/// Read-only host facts for models that need a small, explicit lookup
/// instead of relying on the larger trusted system context. No capability
/// requirement; never exposes a raw OS handle or mutation API.
struct HostEnvironmentTool {
    environment: HostEnvironment,
}

#[async_trait::async_trait]
impl TypedTool for HostEnvironmentTool {
    type Args = NoArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "system.environment"
    }

    fn description(&self) -> &'static str {
        "Return the native operating system, home directory, current working directory, path style, and validated special user directories. Read-only; use these exact paths instead of guessing or translating directory names."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![tool_core::ToolEffect::ReadOnly]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        Ok(self.environment.json_value())
    }
}

/// Read-only clock facts for models that must use the host's actual
/// current time instead of inferring it from training data or a
/// conversation date. Snapshots the clock on every invocation.
struct SystemTimeTool;

#[async_trait::async_trait]
impl TypedTool for SystemTimeTool {
    type Args = NoArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "system.time"
    }

    fn description(&self) -> &'static str {
        "Return the current local and UTC time from the native host. Read-only, fresh on every call; use this instead of guessing the date or time."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![tool_core::ToolEffect::ReadOnly]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let utc = chrono::Utc::now();
        let local = utc.with_timezone(&chrono::Local);
        let mut content = serde_json::json!({
            "local": local.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "utc": utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "date": local.format("%Y-%m-%d").to_string(),
            "time": local.format("%H:%M:%S").to_string(),
            "utc_offset": local.format("%:z").to_string(),
            "unix_timestamp": utc.timestamp(),
        });
        if let Ok(timezone) = iana_time_zone::get_timezone() {
            if let Some(object) = content.as_object_mut() {
                object.insert("timezone".to_string(), serde_json::Value::String(timezone));
            }
        }
        Ok(content)
    }
}

/// Static system-fact tools. Pure reads, infallible to collect.
pub struct SystemToolPack {
    environment: HostEnvironment,
}

impl SystemToolPack {
    pub fn new(environment: HostEnvironment) -> Self {
        Self { environment }
    }
}

impl ToolPack for SystemToolPack {
    fn id(&self) -> &'static str {
        "builtin.system"
    }

    fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        vec![
            TypedToolAdapter::arc(HostEnvironmentTool {
                environment: self.environment.clone(),
            }),
            TypedToolAdapter::arc(SystemTimeTool),
        ]
    }
}

/// Structured process execution tools behind the process broker.
pub struct ProcessToolPack {
    manager: Arc<tool_process::ProcessManager>,
}

impl ProcessToolPack {
    pub fn new(manager: Arc<tool_process::ProcessManager>) -> Self {
        Self { manager }
    }
}

impl ToolPack for ProcessToolPack {
    fn id(&self) -> &'static str {
        "builtin.process"
    }

    fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        vec![
            Arc::new(tool_process::SpawnTool {
                manager: Arc::clone(&self.manager),
                limits: tool_process::ProcessLimits::default(),
            }) as Arc<dyn tool_core::Tool>,
            Arc::new(tool_process::StatusTool {
                manager: Arc::clone(&self.manager),
            }),
            Arc::new(tool_process::KillTool {
                manager: Arc::clone(&self.manager),
            }),
        ]
    }
}

fn tasks_db_path() -> PathBuf {
    // UTSUWA_TASKS_DB is a test/probe escape hatch; production always uses
    // the same path the host uses so tools, CLI, and app share one authority.
    if let Ok(path) = std::env::var("UTSUWA_TASKS_DB") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    storage_core::default_state_dir("utsuwa").join("tasks.db")
}

/// Limits for model-driven interval tasks. The CLI verification command
/// allows tighter values; the model path floors the gap so a confused
/// turn cannot schedule a quota-burning tight loop.
const INTERVAL_TOOL_MAX_TIMES: usize = 30;
const INTERVAL_TOOL_MIN_GAP_MS: i64 = 1_000;
/// Limits for generic model-driven task creation: enough for real
/// multi-step work, bounded so a confused turn cannot queue an
/// unbounded plan.
const CREATE_TOOL_MAX_STEPS: usize = 30;
const CREATE_TOOL_MAX_ATTEMPTS: i32 = 10;

// Small task-tool helpers. Deliberately functions, not a framework: every
// model-facing task tool funnels its errors, store access, and id checks
// through these so the five tools cannot drift apart.
fn task_tool_error(
    tool: &'static str,
    code: &'static str,
    message: impl Into<String>,
) -> tool_core::ToolError {
    tool_core::ToolError::structured(tool, code, message.into())
}

fn open_task_store(
    path: &Path,
    tool: &'static str,
) -> Result<task_core::SqliteTaskStore, tool_core::ToolError> {
    task_core::SqliteTaskStore::open(path).map_err(|err| {
        task_tool_error(
            tool,
            "store_unavailable",
            format!("cannot open tasks.db: {err}"),
        )
    })
}

fn require_task_id<'a>(
    value: &'a str,
    tool: &'static str,
) -> Result<&'a str, tool_core::ToolError> {
    if value.trim().is_empty() {
        return Err(task_tool_error(
            tool,
            "invalid_task_id",
            "task_id must be a non-empty string",
        ));
    }
    Ok(value)
}

// The error code stays a parameter (not a fixed string) so each tool keeps
// its historical code (`invalid_instruction`, `invalid_title`, ...): model
// callers match on codes, and a shared helper must not rename them.
fn require_non_empty(
    value: &str,
    field: &str,
    tool: &'static str,
    code: &'static str,
) -> Result<(), tool_core::ToolError> {
    if value.trim().is_empty() {
        return Err(task_tool_error(
            tool,
            code,
            format!("{field} must be a non-empty string"),
        ));
    }
    Ok(())
}

fn now_ms() -> i64 {
    task_core::SystemClock.now_ms()
}

/// Shared task-store ownership for model-facing task tools. The native
/// host installs the `TaskHost`'s own store here so tools reuse the one
/// SQLite authority instead of reopening tasks.db on every call. Cheap to
/// clone (the store is an `Arc` over one connection); unset tools (tests,
/// degraded mode) fall back to opening the path per call, as before.
#[derive(Clone)]
pub struct TaskToolContext {
    store: task_core::SqliteTaskStore,
}

impl TaskToolContext {
    pub fn new(store: task_core::SqliteTaskStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &task_core::SqliteTaskStore {
        &self.store
    }
}

#[derive(Clone)]
enum TaskToolSource {
    Shared(TaskToolContext),
    Path(PathBuf),
}

impl TaskToolSource {
    fn open(&self, tool: &'static str) -> Result<task_core::SqliteTaskStore, tool_core::ToolError> {
        match self {
            TaskToolSource::Shared(ctx) => Ok(ctx.store.clone()),
            TaskToolSource::Path(path) => open_task_store(path, tool),
        }
    }
}

#[derive(Deserialize)]
struct CreateIntervalArgs {
    instruction: String,
    times: Option<usize>,
    gap_ms: Option<i64>,
    title: Option<String>,
    scheduled_at: Option<i64>,
}

/// Create a durable interval/scheduled task from a plain instruction.
/// Same builder the CLI verification uses — the model path and the
/// verification path cannot drift apart. No capability requirement:
/// like the memory tools, this only writes the host's own state dir,
/// and every side-effecting step still authorizes at execution time.
struct CreateIntervalTaskTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for CreateIntervalTaskTool {
    type Args = CreateIntervalArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.create_interval"
    }

    fn description(&self) -> &'static str {
        "Run an instruction N times with a pause between runs, optionally starting at a later time. The instruction names existing tools (system.time, notification.send); the durable task manager executes each repetition asynchronously. Returns the task id and step count — report them instead of waiting for the repetitions."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["instruction"],
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "What each repetition does; names the tools to use.",
                },
                "times": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": INTERVAL_TOOL_MAX_TIMES,
                    "default": 3,
                    "description": "Repetition count.",
                },
                "gap_ms": {
                    "type": "integer",
                    "minimum": INTERVAL_TOOL_MIN_GAP_MS,
                    "default": 3000,
                    "description": "Pause between repetitions in milliseconds.",
                },
                "title": {
                    "type": "string",
                    "description": "Short task title.",
                },
                "scheduled_at": {
                    "type": "integer",
                    "description": "First run no earlier than this (epoch milliseconds). Omit to start now.",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.create_interval";
        let instruction = args.instruction.trim().to_string();
        require_non_empty(&instruction, "instruction", tool, "invalid_instruction")?;
        let times = args.times.unwrap_or(3);
        if times == 0 || times > INTERVAL_TOOL_MAX_TIMES {
            return Err(task_tool_error(
                tool,
                "invalid_times",
                format!("times must be 1..={INTERVAL_TOOL_MAX_TIMES}"),
            ));
        }
        let gap_ms = args.gap_ms.unwrap_or(3_000);
        if gap_ms < INTERVAL_TOOL_MIN_GAP_MS {
            return Err(task_tool_error(
                tool,
                "invalid_gap",
                format!("gap_ms must be >= {INTERVAL_TOOL_MIN_GAP_MS}"),
            ));
        }
        if args.scheduled_at.is_some_and(|at| at < 0) {
            return Err(task_tool_error(
                tool,
                "invalid_scheduled_at",
                "scheduled_at must be epoch milliseconds >= 0",
            ));
        }
        let title = args
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| {
                format!("Interval task ({times}x, {}s gaps)", gap_ms as f64 / 1000.0)
            });
        let task = interval_task(&IntervalOptions {
            title,
            instruction: instruction.clone(),
            prompts: numbered_prompts(&instruction, times),
            gap_ms,
            max_iterations: 4,
            scheduled_at: args.scheduled_at,
        });
        let store = self.source.open(tool)?;
        let created = store.create(task, now_ms()).await.map_err(|err| {
            task_tool_error(tool, "create_failed", format!("cannot create task: {err}"))
        })?;
        Ok(serde_json::json!({
            "task_id": created.id,
            "title": created.title,
            "status": created.status.as_str(),
            "steps": created.steps.len(),
            "repetitions": times,
            "gap_ms": gap_ms,
            "execute_with": "leave the app running",
        }))
    }
}

#[derive(Deserialize)]
struct CreateTaskArgs {
    title: String,
    instruction: String,
    steps: Vec<task_core::NewTaskStep>,
    scheduled_at: Option<i64>,
    priority: Option<i32>,
    max_attempts: Option<i32>,
    verification: Option<task_core::VerificationSpec>,
}

/// Create any durable task from an explicit step list: one-shot work,
/// custom multi-step plans, and scheduled runs that are not plain
/// repetitions (for N-times-with-gaps, prefer `tasks.create_interval`).
/// Step inputs are validated up front so a malformed plan fails here
/// with a clear code instead of failing mid-run. No capability
/// requirement: like the other task tools this only writes the host's
/// own state dir, and every side-effecting step still authorizes at
/// execution time.
struct CreateTaskTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for CreateTaskTool {
    type Args = CreateTaskArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.create"
    }

    fn description(&self) -> &'static str {
        "Create a durable task from an explicit step list: one-shot work, custom multi-step plans, or a run scheduled for later. Each step names its type (agent, tool, wait, notification, approval, avatar_routine) plus its input. Returns the task id — report it instead of waiting for the task to finish."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["title", "instruction", "steps"],
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short task title.",
                },
                "instruction": {
                    "type": "string",
                    "description": "What the task accomplishes overall.",
                },
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": CREATE_TOOL_MAX_STEPS,
                    "description": "Ordered step list: [{step_type, input, max_attempts?}]. Agent steps need input.prompt; tool steps need input.tool plus input.args.",
                    "items": {
                        "type": "object",
                        "required": ["step_type", "input"],
                        "properties": {
                            "step_type": {
                                "type": "string",
                                "enum": ["avatar_routine", "notification", "wait", "agent", "tool", "approval"],
                            },
                            "input": { "type": "object" },
                            "max_attempts": { "type": "integer", "minimum": 1 },
                        },
                    },
                },
                "scheduled_at": {
                    "type": "integer",
                    "description": "First run no earlier than this (epoch milliseconds). Omit to start now.",
                },
                "priority": {
                    "type": "integer",
                    "description": "Higher runs first when several tasks are ready (10 background, 50 normal, 80 user-requested, 100 urgent).",
                },
                "max_attempts": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": CREATE_TOOL_MAX_ATTEMPTS,
                    "description": "Whole-task retry budget.",
                },
                "verification": {
                    "type": "object",
                    "description": "Completion check: {type: none|result_present} or {type: tool_receipt, tool_name} or {type: avatar_routine, expected_steps} or {type: file_exists, file_ref}.",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.create";
        let title = args.title.trim().to_string();
        require_non_empty(&title, "title", tool, "invalid_title")?;
        let instruction = args.instruction.trim().to_string();
        require_non_empty(&instruction, "instruction", tool, "invalid_instruction")?;
        if args.steps.is_empty() || args.steps.len() > CREATE_TOOL_MAX_STEPS {
            return Err(task_tool_error(
                tool,
                "invalid_steps",
                format!("steps must contain 1..={CREATE_TOOL_MAX_STEPS} steps"),
            ));
        }
        validate_create_steps(tool, &args.steps)?;
        if args.scheduled_at.is_some_and(|at| at < 0) {
            return Err(task_tool_error(
                tool,
                "invalid_scheduled_at",
                "scheduled_at must be epoch milliseconds >= 0",
            ));
        }
        if args
            .max_attempts
            .is_some_and(|n| !(1..=CREATE_TOOL_MAX_ATTEMPTS).contains(&n))
        {
            return Err(task_tool_error(
                tool,
                "invalid_max_attempts",
                format!("max_attempts must be 1..={CREATE_TOOL_MAX_ATTEMPTS}"),
            ));
        }
        let store = self.source.open(tool)?;
        let created = store
            .create(
                task_core::NewTask {
                    title,
                    instruction,
                    scheduled_at: args.scheduled_at,
                    priority: args.priority,
                    max_attempts: args.max_attempts,
                    verification: args.verification,
                    parent_task_id: None,
                    steps: args.steps,
                },
                now_ms(),
            )
            .await
            .map_err(|err| {
                task_tool_error(tool, "create_failed", format!("cannot create task: {err}"))
            })?;
        Ok(serde_json::json!({
            "task_id": created.id,
            "title": created.title,
            "status": created.status.as_str(),
            "steps": created.steps.len(),
            "execute_with": "leave the app running",
        }))
    }
}

/// Fail a malformed plan at creation time with the same requirements
/// the runners enforce at execution time (agent prompt, tool name):
/// a clear `invalid_steps` here beats a mid-run step failure.
fn validate_create_steps(
    tool: &'static str,
    steps: &[task_core::NewTaskStep],
) -> Result<(), tool_core::ToolError> {
    use task_core::TaskStepType;
    for (index, step) in steps.iter().enumerate() {
        let here = |detail: &str| {
            task_tool_error(
                tool,
                "invalid_steps",
                format!("step {index} ({}): {detail}", step.step_type.as_str()),
            )
        };
        match step.step_type {
            TaskStepType::Agent => {
                let prompt = step
                    .input
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if prompt.trim().is_empty() {
                    return Err(here("agent steps need a non-empty string input.prompt"));
                }
            }
            TaskStepType::Tool => {
                let name = step
                    .input
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if name.trim().is_empty() {
                    return Err(here("tool steps need a non-empty string input.tool"));
                }
            }
            _ => {}
        }
        if step.max_attempts.is_some_and(|n| n < 1) {
            return Err(here("max_attempts must be >= 1"));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ListTasksArgs {
    status: Option<String>,
    limit: Option<i64>,
}

/// List durable tasks with status and progress. Summaries only —
/// `tasks.get` has the full detail. Same local-state rationale as
/// creation: no capability requirement.
struct ListTasksTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for ListTasksTool {
    type Args = ListTasksArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.list"
    }

    fn description(&self) -> &'static str {
        "List durable tasks with their status and step progress. Use this to check what is scheduled, running, or finished before asking for details with tasks.get."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Only tasks in this status.",
                    "enum": ["pending", "scheduled", "ready", "running", "waiting", "needs_review", "completed", "failed", "cancelled"],
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 20,
                    "description": "Maximum tasks to return.",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.list";
        let status = args
            .status
            .map(|raw| {
                task_core::TaskStatus::parse(&raw).ok_or_else(|| {
                    task_tool_error(
                        tool,
                        "invalid_status",
                        format!(
                            "unknown status '{raw}': pending, scheduled, ready, running, waiting, needs_review, completed, failed, cancelled"
                        ),
                    )
                })
            })
            .transpose()?;
        let limit = args.limit.unwrap_or(20).clamp(1, 200);
        let store = self.source.open(tool)?;
        let tasks = store.list(status, limit).await.map_err(|err| {
            task_tool_error(tool, "list_failed", format!("cannot list tasks: {err}"))
        })?;
        let summaries: Vec<serde_json::Value> = tasks
            .iter()
            .map(|task| {
                serde_json::json!({
                    "id": task.id,
                    "title": task.title,
                    "status": task.status.as_str(),
                    "step": task.current_step_index.min(task.steps.len()),
                    "steps": task.steps.len(),
                    "attempts": task.attempts,
                    "scheduled_at": task.scheduled_at,
                })
            })
            .collect();
        Ok(serde_json::json!({ "tasks": summaries, "count": tasks.len() }))
    }
}

#[derive(Deserialize)]
struct GetTaskArgs {
    task_id: String,
}

/// Full task detail: status, per-step progress, step results, errors.
/// This is how the agent checks whether a scheduled task ran and what
/// each repetition produced.
struct GetTaskTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for GetTaskTool {
    type Args = GetTaskArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.get"
    }

    fn description(&self) -> &'static str {
        "Get one durable task in full: status, per-step progress, step results, and errors. Use this to check whether a scheduled task ran and what each repetition produced."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task_id"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id from tasks.create, tasks.create_interval or tasks.list.",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.get";
        let task_id = require_task_id(&args.task_id, tool)?;
        let store = self.source.open(tool)?;
        let task = store
            .get(task_id)
            .await
            .map_err(|err| task_tool_error(tool, "get_failed", format!("cannot read task: {err}")))?
            .ok_or_else(|| {
                task_tool_error(tool, "unknown_task", format!("unknown task '{task_id}'"))
            })?;
        serde_json::to_value(&task).map_err(|err| {
            task_tool_error(tool, "serialization", format!("cannot encode task: {err}"))
        })
    }
}

#[derive(Deserialize)]
struct CancelTaskArgs {
    task_id: String,
    reason: Option<String>,
}

/// Stop a task that has not finished. Same `cancel_task` the
/// scheduler uses — one implementation, no drift. The cancelled task
/// stays in history (durable by design); there is no hard delete.
struct CancelTaskTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for CancelTaskTool {
    type Args = CancelTaskArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.cancel"
    }

    fn description(&self) -> &'static str {
        "Stop a task that has not finished (pending, scheduled, running, waiting, or needs_review). The task is marked cancelled and kept as history. Cancellation is final; create a new task instead of resuming."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task_id"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id from tasks.create, tasks.create_interval or tasks.list.",
                },
                "reason": {
                    "type": "string",
                    "description": "Why the task is cancelled (recorded).",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.cancel";
        let task_id = require_task_id(&args.task_id, tool)?;
        let reason = args
            .reason
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| "cancelled by agent".to_string());
        let store = self.source.open(tool)?;
        match task_core::cancel_task(&store, task_id, reason, now_ms()).await {
            Ok(task) => Ok(serde_json::json!({
                "task_id": task.id,
                "status": task.status.as_str(),
            })),
            Err(task_core::TaskCoreError::NotFound(_)) => Err(task_tool_error(
                tool,
                "unknown_task",
                format!("unknown task '{task_id}'"),
            )),
            Err(task_core::TaskCoreError::Step(message)) => {
                Err(task_tool_error(tool, "already_terminal", message))
            }
            Err(err) => Err(task_tool_error(
                tool,
                "cancel_failed",
                format!("cannot cancel task: {err}"),
            )),
        }
    }
}

#[derive(Deserialize)]
struct EditTaskArgs {
    task_id: String,
    title: Option<String>,
    instruction: Option<String>,
    steps: Option<Vec<task_core::NewTaskStep>>,
    scheduled_at: Option<i64>,
}

/// Change a task that never started (pending/scheduled, 0 attempts).
/// Anything already running must be cancelled and recreated instead —
/// rewriting a live task's steps would corrupt the run the manager is
/// executing.
struct EditTaskTool {
    source: TaskToolSource,
}

#[async_trait::async_trait]
impl TypedTool for EditTaskTool {
    type Args = EditTaskArgs;
    type Output = serde_json::Value;

    fn id(&self) -> &'static str {
        "tasks.edit"
    }

    fn description(&self) -> &'static str {
        "Change a task that never started: title, instruction, steps, or scheduled start. Only pending/scheduled tasks with 0 attempts can be edited; cancel and recreate anything already running. Pass a past scheduled_at to run as soon as possible."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task_id"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task id from tasks.create, tasks.create_interval or tasks.list.",
                },
                "title": { "type": "string", "description": "New title." },
                "instruction": { "type": "string", "description": "New instruction." },
                "steps": {
                    "type": "array",
                    "description": "Full replacement step list: [{step_type, input, max_attempts?}].",
                    "items": {
                        "type": "object",
                        "required": ["step_type", "input"],
                        "properties": {
                            "step_type": {
                                "type": "string",
                                "enum": ["avatar_routine", "notification", "wait", "agent", "tool", "approval"],
                            },
                            "input": { "type": "object" },
                            "max_attempts": { "type": "integer", "minimum": 1 },
                        },
                    },
                },
                "scheduled_at": {
                    "type": "integer",
                    "description": "First run no earlier than this (epoch milliseconds). A past value runs as soon as possible.",
                },
            },
        })
    }

    fn effects(&self) -> Vec<tool_core::ToolEffect> {
        vec![]
    }

    async fn call(
        &self,
        _ctx: tool_core::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, tool_core::ToolError> {
        let tool = "tasks.edit";
        let task_id = require_task_id(&args.task_id, tool)?;
        if let Some(scheduled_at) = args.scheduled_at {
            if scheduled_at < 0 {
                return Err(task_tool_error(
                    tool,
                    "invalid_scheduled_at",
                    "scheduled_at must be epoch milliseconds >= 0",
                ));
            }
        }
        if let Some(steps) = &args.steps {
            if steps.is_empty() {
                return Err(task_tool_error(
                    tool,
                    "invalid_steps",
                    "steps must contain at least one step",
                ));
            }
        }
        if let Some(title) = &args.title {
            require_non_empty(title, "title", tool, "invalid_title")?;
        }
        let store = self.source.open(tool)?;
        let mut task = store
            .get(task_id)
            .await
            .map_err(|err| task_tool_error(tool, "get_failed", format!("cannot read task: {err}")))?
            .ok_or_else(|| {
                task_tool_error(tool, "unknown_task", format!("unknown task '{task_id}'"))
            })?;
        let editable = matches!(
            task.status,
            task_core::TaskStatus::Pending | task_core::TaskStatus::Scheduled
        ) && task.attempts == 0
            && task.current_step_index == 0;
        if !editable {
            return Err(task_tool_error(
                tool,
                "already_started",
                "only tasks that never started (pending/scheduled, 0 attempts) can be edited; cancel it and create a new one instead",
            ));
        }
        let now = now_ms();
        if let Some(title) = args.title {
            task.title = title;
        }
        if let Some(instruction) = args.instruction {
            task.instruction = instruction;
        }
        if let Some(steps) = args.steps {
            task.steps = steps
                .into_iter()
                .map(|step| task_core::TaskStep {
                    id: uuid::Uuid::new_v4().to_string(),
                    step_type: step.step_type,
                    status: task_core::TaskStepStatus::Pending,
                    input: step.input,
                    result: None,
                    attempts: 0,
                    max_attempts: step.max_attempts.unwrap_or(1).max(1),
                    started_at: None,
                    finished_at: None,
                    error: None,
                })
                .collect();
            task.current_step_index = 0;
        }
        // Status never changes here: the schedule takes effect through
        // the field alone (the ready batch gates pending tasks on
        // scheduled_at too), and Pending -> Scheduled is not a legal
        // transition — Scheduled only arises at creation.
        if let Some(scheduled_at) = args.scheduled_at {
            task.scheduled_at = Some(scheduled_at);
        }
        store.update(&task, now).await.map_err(|err| {
            task_tool_error(tool, "edit_failed", format!("cannot update task: {err}"))
        })?;
        Ok(serde_json::json!({
            "task_id": task.id,
            "title": task.title,
            "status": task.status.as_str(),
            "steps": task.steps.len(),
        }))
    }
}

/// Durable-task tools for the DEFAULT agent only. Wired into the native
/// turn catalog (runtime.rs), never into task-agent registries
/// (main.rs, task-cli): tasks must not spawn tasks.
pub struct TasksToolPack {
    source: TaskToolSource,
}

impl TasksToolPack {
    pub fn new() -> Self {
        Self {
            source: TaskToolSource::Path(tasks_db_path()),
        }
    }

    pub fn with_tasks_db(tasks_db: PathBuf) -> Self {
        Self {
            source: TaskToolSource::Path(tasks_db),
        }
    }

    /// Share one SQLite authority with the `TaskHost` instead of reopening
    /// tasks.db on every tool call. The native host uses this; unset packs
    /// keep the lazy per-call open.
    pub fn with_context(context: TaskToolContext) -> Self {
        Self {
            source: TaskToolSource::Shared(context),
        }
    }
}

impl Default for TasksToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPack for TasksToolPack {
    fn id(&self) -> &'static str {
        "builtin.tasks"
    }

    fn tools(&self, _ctx: &ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        vec![
            TypedToolAdapter::arc(CreateIntervalTaskTool {
                source: self.source.clone(),
            }),
            TypedToolAdapter::arc(CreateTaskTool {
                source: self.source.clone(),
            }),
            TypedToolAdapter::arc(ListTasksTool {
                source: self.source.clone(),
            }),
            TypedToolAdapter::arc(GetTaskTool {
                source: self.source.clone(),
            }),
            TypedToolAdapter::arc(CancelTaskTool {
                source: self.source.clone(),
            }),
            TypedToolAdapter::arc(EditTaskTool {
                source: self.source.clone(),
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::Principal;

    fn ctx() -> tool_core::ToolContext {
        tool_core::ToolContext::new(Principal::User)
    }

    fn scratch_db(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("utsuwa-tasks-tool-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn tool_at(tasks_db: PathBuf) -> CreateIntervalTaskTool {
        CreateIntervalTaskTool {
            source: TaskToolSource::Path(tasks_db),
        }
    }

    fn args(instruction: &str) -> CreateIntervalArgs {
        CreateIntervalArgs {
            instruction: instruction.to_string(),
            times: None,
            gap_ms: None,
            title: None,
            scheduled_at: None,
        }
    }

    #[test]
    fn pack_exposes_task_management_tools() {
        let pack = TasksToolPack::with_tasks_db(PathBuf::from("/nonexistent-tasks.db"));
        let ids: Vec<String> = pack
            .tools(&ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                "tasks.create_interval".to_string(),
                "tasks.create".to_string(),
                "tasks.list".to_string(),
                "tasks.get".to_string(),
                "tasks.cancel".to_string(),
                "tasks.edit".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn create_interval_writes_task_through_shared_builder() {
        let db = scratch_db("build");
        let tool = tool_at(db.clone());
        let out = tool
            .call(ctx(), args("use system.time to get the date"))
            .await
            .expect("call");
        assert_eq!(
            out.get("repetitions").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            out.get("gap_ms").and_then(serde_json::Value::as_i64),
            Some(3_000)
        );
        let id = out
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id")
            .to_string();
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let task = store.get(&id).await.expect("get").expect("task");
        assert_eq!(task.steps.len(), 5);
        for (index, step) in task.steps.iter().enumerate() {
            if index % 2 == 0 {
                assert_eq!(step.step_type, task_core::TaskStepType::Agent);
                let prompt = step
                    .input
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                assert!(
                    prompt.starts_with("use system.time to get the date"),
                    "{prompt}"
                );
                assert!(
                    prompt.contains(&format!("repetition {} of 3", index / 2 + 1)),
                    "{prompt}"
                );
            } else {
                assert_eq!(step.step_type, task_core::TaskStepType::Wait);
                assert_eq!(
                    step.input
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_i64),
                    Some(3_000)
                );
            }
        }
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn create_interval_result_mentions_no_test_cli() {
        let db = scratch_db("no-cli-hint");
        let tool = tool_at(db.clone());
        let out = tool
            .call(ctx(), args("use system.time to get the date"))
            .await
            .expect("call");
        assert_eq!(
            out.get("execute_with").and_then(serde_json::Value::as_str),
            Some("leave the app running")
        );
        let rendered = serde_json::to_string(&out).expect("render");
        for leak in ["task-cli", "headless"] {
            assert!(
                !rendered.contains(leak),
                "tool result shown to the desktop agent must not advertise test CLIs ({leak})"
            );
        }
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn create_interval_rejects_bad_input() {
        // Validation runs before any store access, so this path never
        // touches the database.
        let tool = tool_at(PathBuf::from("/nonexistent-tasks.db"));
        assert!(tool.call(ctx(), args("  ")).await.is_err());
        let mut bad = args("x");
        bad.times = Some(0);
        assert!(tool.call(ctx(), bad).await.is_err());
        let mut bad = args("x");
        bad.times = Some(31);
        assert!(tool.call(ctx(), bad).await.is_err());
        let mut bad = args("x");
        bad.times = Some(2);
        bad.gap_ms = Some(999);
        assert!(tool.call(ctx(), bad).await.is_err());
        let mut bad = args("x");
        bad.scheduled_at = Some(-1);
        assert!(tool.call(ctx(), bad).await.is_err());
    }

    fn create_tool_at(tasks_db: PathBuf) -> CreateTaskTool {
        CreateTaskTool {
            source: TaskToolSource::Path(tasks_db),
        }
    }

    fn create_args(title: &str, steps: Vec<task_core::NewTaskStep>) -> CreateTaskArgs {
        CreateTaskArgs {
            title: title.to_string(),
            instruction: "do the thing".to_string(),
            steps,
            scheduled_at: None,
            priority: None,
            max_attempts: None,
            verification: None,
        }
    }

    fn wait_step() -> task_core::NewTaskStep {
        task_core::NewTaskStep {
            step_type: task_core::TaskStepType::Wait,
            input: serde_json::json!({ "duration_ms": 10 }),
            max_attempts: None,
        }
    }

    fn agent_step(prompt: &str) -> task_core::NewTaskStep {
        task_core::NewTaskStep {
            step_type: task_core::TaskStepType::Agent,
            input: serde_json::json!({ "prompt": prompt }),
            max_attempts: None,
        }
    }

    #[tokio::test]
    async fn create_writes_generic_task() {
        let db = scratch_db("create");
        let tool = create_tool_at(db.clone());
        let out = tool
            .call(
                ctx(),
                create_args("mixed plan", vec![wait_step(), agent_step("say hi")]),
            )
            .await
            .expect("call");
        let id = out
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id")
            .to_string();
        assert_eq!(
            out.get("steps").and_then(serde_json::Value::as_u64),
            Some(2)
        );
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let task = store.get(&id).await.expect("get").expect("task");
        assert_eq!(task.title, "mixed plan");
        assert_eq!(task.steps.len(), 2);
        assert_eq!(task.steps[0].step_type, task_core::TaskStepType::Wait);
        assert_eq!(task.steps[1].step_type, task_core::TaskStepType::Agent);
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn create_rejects_malformed_plans() {
        // Validation runs before any store access, so this path never
        // touches the database.
        let tool = create_tool_at(PathBuf::from("/nonexistent-tasks.db"));
        assert!(tool
            .call(ctx(), create_args("  ", vec![wait_step()]))
            .await
            .is_err());
        assert!(tool.call(ctx(), create_args("t", vec![])).await.is_err());
        let too_many = (0..CREATE_TOOL_MAX_STEPS + 1)
            .map(|_| wait_step())
            .collect();
        assert!(tool.call(ctx(), create_args("t", too_many)).await.is_err());
        // Agent step without a prompt fails here, not mid-run.
        assert!(tool
            .call(ctx(), create_args("t", vec![agent_step("  ")]))
            .await
            .is_err());
        let tool_step = task_core::NewTaskStep {
            step_type: task_core::TaskStepType::Tool,
            input: serde_json::json!({ "args": {} }),
            max_attempts: None,
        };
        assert!(tool
            .call(ctx(), create_args("t", vec![tool_step]))
            .await
            .is_err());
        let mut bad = create_args("t", vec![wait_step()]);
        bad.scheduled_at = Some(-1);
        assert!(tool.call(ctx(), bad).await.is_err());
        let mut bad = create_args("t", vec![wait_step()]);
        bad.max_attempts = Some(11);
        assert!(tool.call(ctx(), bad).await.is_err());
    }

    #[tokio::test]
    async fn create_carries_schedule_priority_and_verification() {
        let db = scratch_db("create-opts");
        let tool = create_tool_at(db.clone());
        let mut args = create_args("later", vec![wait_step()]);
        args.scheduled_at = Some(9_999_999_999_999);
        args.priority = Some(task_core::model::priority::URGENT);
        args.max_attempts = Some(5);
        args.verification = Some(task_core::VerificationSpec::ResultPresent);
        let out = tool.call(ctx(), args).await.expect("call");
        let id = out
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id");
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let task = store.get(id).await.expect("get").expect("task");
        assert_eq!(task.scheduled_at, Some(9_999_999_999_999));
        assert_eq!(task.status, task_core::TaskStatus::Scheduled);
        assert_eq!(task.priority, task_core::model::priority::URGENT);
        assert_eq!(task.max_attempts, 5);
        assert!(matches!(
            task.verification,
            Some(task_core::VerificationSpec::ResultPresent)
        ));
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn create_interval_carries_scheduled_start() {
        let db = scratch_db("scheduled");
        let tool = tool_at(db.clone());
        let mut scheduled = args("do it later");
        scheduled.times = Some(1);
        scheduled.scheduled_at = Some(9_999_999_999_999);
        let out = tool.call(ctx(), scheduled).await.expect("call");
        assert_eq!(
            out.get("status").and_then(serde_json::Value::as_str),
            Some("scheduled")
        );
        let id = out
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id");
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let task = store.get(id).await.expect("get").expect("task");
        assert_eq!(task.scheduled_at, Some(9_999_999_999_999));
        assert_eq!(task.steps.len(), 1);
        let _ = std::fs::remove_file(&db);
    }

    async fn create_task(db: &std::path::Path, instruction: &str) -> String {
        let out = tool_at(db.to_path_buf())
            .call(ctx(), args(instruction))
            .await
            .expect("create");
        out.get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id")
            .to_string()
    }

    #[tokio::test]
    async fn list_returns_summaries_and_filters() {
        let db = scratch_db("list");
        let first = create_task(&db, "first").await;
        let _second = create_task(&db, "second").await;
        let tool = ListTasksTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let out = tool
            .call(
                ctx(),
                ListTasksArgs {
                    status: None,
                    limit: None,
                },
            )
            .await
            .expect("list");
        assert_eq!(
            out.get("count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
        let out = tool
            .call(
                ctx(),
                ListTasksArgs {
                    status: Some("completed".to_string()),
                    limit: None,
                },
            )
            .await
            .expect("filtered");
        assert_eq!(
            out.get("count").and_then(serde_json::Value::as_u64),
            Some(0)
        );
        let out = tool
            .call(
                ctx(),
                ListTasksArgs {
                    status: Some("bogus".to_string()),
                    limit: None,
                },
            )
            .await;
        assert!(out.is_err());
        // Summary carries progress without full step detail.
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let task = store.get(&first).await.expect("get").expect("task");
        assert_eq!(task.steps.len(), 5);
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn get_returns_full_task_or_unknown() {
        let db = scratch_db("get");
        let id = create_task(&db, "fetch me").await;
        let tool = GetTaskTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let out = tool
            .call(
                ctx(),
                GetTaskArgs {
                    task_id: id.clone(),
                },
            )
            .await
            .expect("get");
        assert_eq!(
            out.get("id").and_then(serde_json::Value::as_str),
            Some(id.as_str())
        );
        assert_eq!(
            out.get("steps")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(5)
        );
        let missing = tool
            .call(
                ctx(),
                GetTaskArgs {
                    task_id: "nope".to_string(),
                },
            )
            .await;
        assert!(missing.is_err());
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn cancel_marks_cancelled_and_refuses_terminal() {
        let db = scratch_db("cancel");
        let id = create_task(&db, "stop me").await;
        let tool = CancelTaskTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let out = tool
            .call(
                ctx(),
                CancelTaskArgs {
                    task_id: id.clone(),
                    reason: Some("no longer needed".to_string()),
                },
            )
            .await
            .expect("cancel");
        assert_eq!(
            out.get("status").and_then(serde_json::Value::as_str),
            Some("cancelled")
        );
        // Second cancel fails: already terminal.
        let again = tool
            .call(
                ctx(),
                CancelTaskArgs {
                    task_id: id.clone(),
                    reason: None,
                },
            )
            .await;
        assert!(again.is_err());
        let missing = tool
            .call(
                ctx(),
                CancelTaskArgs {
                    task_id: "nope".to_string(),
                    reason: None,
                },
            )
            .await;
        assert!(missing.is_err());
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn edit_updates_unstarted_task() {
        let db = scratch_db("edit");
        let id = create_task(&db, "fix me").await;
        let tool = EditTaskTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let out = tool
            .call(
                ctx(),
                EditTaskArgs {
                    task_id: id.clone(),
                    title: Some("fixed title".to_string()),
                    instruction: Some("fixed instruction".to_string()),
                    steps: None,
                    scheduled_at: Some(9_999_999_999_999),
                },
            )
            .await
            .expect("edit");
        // Status never changes on edit (Pending -> Scheduled is not a
        // legal transition); the schedule takes effect through the field.
        assert_eq!(
            out.get("status").and_then(serde_json::Value::as_str),
            Some("pending")
        );
        assert_eq!(
            out.get("title").and_then(serde_json::Value::as_str),
            Some("fixed title")
        );
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let edited = store.get(&id).await.expect("get").expect("task");
        assert_eq!(edited.scheduled_at, Some(9_999_999_999_999));
        assert_eq!(edited.instruction, "fixed instruction");
        // Step replacement rebuilds fresh pending steps.
        let out = tool
            .call(
                ctx(),
                EditTaskArgs {
                    task_id: id.clone(),
                    title: None,
                    instruction: None,
                    steps: Some(vec![task_core::NewTaskStep {
                        step_type: task_core::TaskStepType::Wait,
                        input: serde_json::json!({ "duration_ms": 500 }),
                        max_attempts: Some(1),
                    }]),
                    scheduled_at: None,
                },
            )
            .await
            .expect("edit steps");
        assert_eq!(
            out.get("steps").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        // Validation: empty title, empty steps, bad schedule.
        for bad in [
            EditTaskArgs {
                task_id: id.clone(),
                title: Some("  ".to_string()),
                instruction: None,
                steps: None,
                scheduled_at: None,
            },
            EditTaskArgs {
                task_id: id.clone(),
                title: None,
                instruction: None,
                steps: Some(vec![]),
                scheduled_at: None,
            },
            EditTaskArgs {
                task_id: id.clone(),
                title: None,
                instruction: None,
                steps: None,
                scheduled_at: Some(-5),
            },
        ] {
            assert!(tool.call(ctx(), bad).await.is_err());
        }
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn edit_refuses_started_or_finished_tasks() {
        let db = scratch_db("edit-started");
        let id = create_task(&db, "running").await;
        let store = task_core::SqliteTaskStore::open(&db).expect("open");
        let mut task = store.get(&id).await.expect("get").expect("task");
        // Leave the unstarted set through a legal transition: a Ready
        // task is claimable, so editing must refuse it too.
        task.status = task_core::TaskStatus::Ready;
        store
            .update(&task, task_core::SystemClock.now_ms())
            .await
            .expect("mark ready");
        let tool = EditTaskTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let refused = tool
            .call(
                ctx(),
                EditTaskArgs {
                    task_id: id.clone(),
                    title: Some("too late".to_string()),
                    instruction: None,
                    steps: None,
                    scheduled_at: None,
                },
            )
            .await;
        assert!(refused.is_err());
        // Terminal tasks refuse as well.
        let finished = create_task(&db, "done").await;
        CancelTaskTool {
            source: TaskToolSource::Path(db.clone()),
        }
        .call(
            ctx(),
            CancelTaskArgs {
                task_id: finished.clone(),
                reason: None,
            },
        )
        .await
        .expect("cancel");
        let refused = tool
            .call(
                ctx(),
                EditTaskArgs {
                    task_id: finished,
                    title: Some("too late".to_string()),
                    instruction: None,
                    steps: None,
                    scheduled_at: None,
                },
            )
            .await;
        assert!(refused.is_err());
        let _ = std::fs::remove_file(&db);
    }

    fn err_code(err: &tool_core::ToolError) -> String {
        match err {
            tool_core::ToolError::Structured { code, .. } => code.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn helpers_reject_empty_ids_and_values() {
        assert_eq!(
            err_code(&require_task_id("  ", "tasks.get").expect_err("empty id")),
            "invalid_task_id"
        );
        assert!(require_task_id("t-1", "tasks.get").is_ok());
        assert_eq!(
            err_code(
                &require_non_empty(
                    "",
                    "instruction",
                    "tasks.create_interval",
                    "invalid_instruction"
                )
                .expect_err("empty value")
            ),
            "invalid_instruction"
        );
        assert!(now_ms() > 0);
    }

    #[tokio::test]
    async fn get_rejects_empty_task_id_without_touching_the_store() {
        let tool = GetTaskTool {
            source: TaskToolSource::Path(PathBuf::from("/nonexistent-tasks.db")),
        };
        let err = tool
            .call(
                ctx(),
                GetTaskArgs {
                    task_id: "   ".to_string(),
                },
            )
            .await
            .expect_err("empty id");
        assert_eq!(err_code(&err), "invalid_task_id");
    }

    #[tokio::test]
    async fn tools_surface_store_open_errors() {
        // A path under a missing directory cannot be opened: every tool
        // must report `store_unavailable`, not panic or hang.
        let missing = std::env::temp_dir().join(format!(
            "utsuwa-no-such-dir-{}/tasks.db",
            std::process::id()
        ));
        let tool = ListTasksTool {
            source: TaskToolSource::Path(missing),
        };
        let err = tool
            .call(
                ctx(),
                ListTasksArgs {
                    status: None,
                    limit: None,
                },
            )
            .await
            .expect_err("unopenable store");
        assert_eq!(err_code(&err), "store_unavailable");
    }

    #[tokio::test]
    async fn create_and_edit_reject_negative_schedules() {
        let db = scratch_db("bad-schedule");
        let tool = tool_at(db.clone());
        let err = tool
            .call(
                ctx(),
                CreateIntervalArgs {
                    scheduled_at: Some(-1),
                    ..args("use system.time")
                },
            )
            .await
            .expect_err("negative scheduled_at");
        assert_eq!(err_code(&err), "invalid_scheduled_at");

        let id = create_task(&db, "editable").await;
        let edit = EditTaskTool {
            source: TaskToolSource::Path(db.clone()),
        };
        let err = edit
            .call(
                ctx(),
                EditTaskArgs {
                    task_id: id,
                    title: None,
                    instruction: None,
                    steps: None,
                    scheduled_at: Some(-5),
                },
            )
            .await
            .expect_err("negative scheduled_at");
        assert_eq!(err_code(&err), "invalid_scheduled_at");
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn shared_context_tools_see_the_same_store_without_a_path() {
        // An in-memory store has no path at all: only the shared context
        // makes it reachable, proving tools reuse the installed authority
        // instead of reopening a path.
        let store = task_core::SqliteTaskStore::open_in_memory().expect("open");
        let context = TaskToolContext::new(store.clone());
        let create = CreateIntervalTaskTool {
            source: TaskToolSource::Shared(context.clone()),
        };
        let out = create
            .call(ctx(), args("use system.time to get the date"))
            .await
            .expect("create through shared store");
        let id = out
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .expect("task_id")
            .to_string();
        let get = GetTaskTool {
            source: TaskToolSource::Shared(context),
        };
        let fetched = get
            .call(
                ctx(),
                GetTaskArgs {
                    task_id: id.clone(),
                },
            )
            .await
            .expect("get through shared store");
        assert_eq!(
            fetched.get("id").and_then(serde_json::Value::as_str),
            Some(id.as_str())
        );
        // And the pack-level constructor wires the same context through.
        let pack = TasksToolPack::with_context(TaskToolContext::new(store));
        assert_eq!(pack.tools(&ToolLoadContext::default()).len(), 6);
    }
}
