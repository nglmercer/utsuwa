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
use std::path::PathBuf;
use std::sync::Arc;
use task_core::{Clock as _, TaskStore as _};
use task_host::interval::{interval_task, IntervalOptions};
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
    tasks_db: PathBuf,
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
        let invalid = |code: &str, message: String| {
            tool_core::ToolError::structured("tasks.create_interval", code, message)
        };
        let instruction = args.instruction.trim().to_string();
        if instruction.is_empty() {
            return Err(invalid(
                "invalid_instruction",
                "instruction must be a non-empty string".to_string(),
            ));
        }
        let times = args.times.unwrap_or(3);
        if times == 0 || times > INTERVAL_TOOL_MAX_TIMES {
            return Err(invalid(
                "invalid_times",
                format!("times must be 1..={INTERVAL_TOOL_MAX_TIMES}"),
            ));
        }
        let gap_ms = args.gap_ms.unwrap_or(3_000);
        if gap_ms < INTERVAL_TOOL_MIN_GAP_MS {
            return Err(invalid(
                "invalid_gap",
                format!("gap_ms must be >= {INTERVAL_TOOL_MIN_GAP_MS}"),
            ));
        }
        if args.scheduled_at.is_some_and(|at| at < 0) {
            return Err(invalid(
                "invalid_scheduled_at",
                "scheduled_at must be epoch milliseconds >= 0".to_string(),
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
            prompts: vec![instruction; times],
            gap_ms,
            max_iterations: 4,
            scheduled_at: args.scheduled_at,
        });
        let store = task_core::SqliteTaskStore::open(&self.tasks_db)
            .map_err(|err| invalid("store_unavailable", format!("cannot open tasks.db: {err}")))?;
        let now = task_core::SystemClock.now_ms();
        let created = store
            .create(task, now)
            .await
            .map_err(|err| invalid("create_failed", format!("cannot create task: {err}")))?;
        Ok(serde_json::json!({
            "task_id": created.id,
            "title": created.title,
            "status": created.status.as_str(),
            "steps": created.steps.len(),
            "repetitions": times,
            "gap_ms": gap_ms,
        }))
    }
}

/// Durable-task tools for the DEFAULT agent only. Wired into the native
/// turn catalog (runtime.rs), never into task-agent registries
/// (main.rs, task-cli): tasks must not spawn tasks.
pub struct TasksToolPack {
    tasks_db: PathBuf,
}

impl TasksToolPack {
    pub fn new() -> Self {
        Self {
            tasks_db: tasks_db_path(),
        }
    }

    pub fn with_tasks_db(tasks_db: PathBuf) -> Self {
        Self { tasks_db }
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
        vec![TypedToolAdapter::arc(CreateIntervalTaskTool {
            tasks_db: self.tasks_db.clone(),
        })]
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
        CreateIntervalTaskTool { tasks_db }
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
    fn pack_exposes_create_interval() {
        let pack = TasksToolPack::with_tasks_db(PathBuf::from("/nonexistent-tasks.db"));
        let ids: Vec<String> = pack
            .tools(&ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect();
        assert_eq!(ids, vec!["tasks.create_interval".to_string()]);
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
                assert_eq!(
                    step.input.get("prompt").and_then(serde_json::Value::as_str),
                    Some("use system.time to get the date")
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
}
