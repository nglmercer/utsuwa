//! Host-level fault injection: a crash between "tool effect fired" and
//! "step completion persisted" must not double-fire the effect after
//! restart. The success receipt recorded by the first attempt replays on
//! the recovered run instead of re-invoking the tool.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use task_core::{
    idempotency_key, ExecutionReceipt, NewTask, NewTaskStep, ReceiptStatus, TaskStatus,
    TaskStepStatus, TaskStepType, TaskStore,
};
use task_host::TaskHost;

fn scratch_db(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("utsuwa-host-fault-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    path
}

struct CountingTool {
    calls: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl tool_core::Tool for CountingTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("test.counting"),
            description: "counting".to_string(),
            input_schema: serde_json::json!({}),
            effects: vec![tool_core::ToolEffect::ExternalSideEffect],
        }
    }

    async fn invoke(
        &self,
        _ctx: tool_core::ToolContext,
        _args: serde_json::Value,
    ) -> Result<tool_core::ToolOutput, tool_core::ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(tool_core::ToolOutput::json(
            serde_json::json!({ "fired": true }),
        ))
    }
}

fn registry_with(calls: Arc<AtomicU32>) -> Arc<tool_core::ToolRegistry> {
    let mut registry = tool_core::ToolRegistry::new();
    registry
        .register(Arc::new(CountingTool { calls }))
        .expect("register");
    Arc::new(registry)
}

fn counting_task() -> NewTask {
    NewTask {
        title: "fire-once".to_string(),
        instruction: "fire once".to_string(),
        scheduled_at: None,
        priority: None,
        max_attempts: Some(3),
        verification: None,
        parent_task_id: None,
        steps: vec![NewTaskStep {
            step_type: TaskStepType::Tool,
            input: serde_json::json!({ "tool": "test.counting", "args": {} }),
            max_attempts: Some(3),
        }],
    }
}

#[tokio::test]
async fn tool_effect_fires_once_across_simulated_crash() {
    let db = scratch_db("fire-once");
    let calls = Arc::new(AtomicU32::new(0));
    let emit: task_host::EmitFn = Arc::new(|_| {});

    // Phase 1: the task starts, the tool effect fires, the success receipt
    // lands — and then the host dies before the step completion persists.
    // Only the receipt + the stale Running state survive (exactly what a
    // real crash between effect and completion leaves behind).
    let task_id = {
        let host =
            TaskHost::open(&db, registry_with(calls.clone()), emit.clone(), None).expect("open");
        let mut task = host.create(counting_task()).await.expect("create");
        let step_id = task.steps[0].id.clone();
        task.status = TaskStatus::Ready;
        host.store()
            .update(&task, host.now_ms())
            .await
            .expect("ready");
        task.status = TaskStatus::Running;
        task.attempts = 1;
        task.lease_until = Some(host.now_ms() - 1);
        task.steps[0].status = TaskStepStatus::Running;
        task.steps[0].attempts = 1;
        host.store()
            .update(&task, host.now_ms())
            .await
            .expect("running");
        let receipt = ExecutionReceipt {
            execution_id: "exec-crash".to_string(),
            task_id: task.id.clone(),
            step_id: step_id.clone(),
            idempotency_key: idempotency_key(&task.id, &step_id, "test.counting", 1),
            operation: "test.counting".to_string(),
            status: ReceiptStatus::Success,
            started_at: host.now_ms(),
            finished_at: Some(host.now_ms()),
            external_id: None,
            output: serde_json::json!({
                "tool": "test.counting",
                "status": "success",
                "output": { "fired": true },
                "truncated": false,
            }),
        };
        assert!(host.store().record_receipt(&receipt).await.expect("record"));
        task.id.clone()
        // Crash: host drops with Running/expired-lease state on disk.
    };

    // Phase 2: a fresh host on the same file recovers and finishes the
    // task WITHOUT re-invoking the tool.
    let host = TaskHost::open(&db, registry_with(calls.clone()), emit, None).expect("reopen");
    host.tick().await.expect("tick");
    let done = host.get(&task_id).await.expect("get").expect("task");
    assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        done.steps[0].result.as_ref().and_then(|r| r.get("status")),
        Some(&serde_json::json!("success"))
    );
    let _ = std::fs::remove_file(&db);
}

#[tokio::test]
async fn normal_tool_execution_records_a_success_receipt() {
    let host = TaskHost::open_in_memory(
        registry_with(Arc::new(AtomicU32::new(0))),
        Arc::new(|_| {}),
        None,
    )
    .expect("host");
    let created = host.create(counting_task()).await.expect("create");
    host.tick().await.expect("tick");
    let done = host.get(&created.id).await.expect("get").expect("task");
    assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
    let receipts = host
        .store()
        .list_receipts(&created.id, 10)
        .await
        .expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, ReceiptStatus::Success);
    assert_eq!(receipts[0].operation, "test.counting");
    // Second tick is a no-op: terminal tasks never re-execute.
    host.tick().await.expect("tick2");
    let receipts = host
        .store()
        .list_receipts(&created.id, 10)
        .await
        .expect("receipts");
    assert_eq!(receipts.len(), 1);
}
