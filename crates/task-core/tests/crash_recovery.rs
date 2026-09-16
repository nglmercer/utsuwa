//! Crash/restart fault suite: the durable-task contract is that a host
//! crash loses nothing but in-flight time. Every test here opens a
//! FILE-backed store, drops it mid-state (the simulated crash — no
//! cleanup, no graceful shutdown), reopens the same file, and asserts
//! recovery converges instead of losing work or double-firing effects.

use std::path::PathBuf;
use task_core::{
    expire_waits, idempotency_key, recover_stale, ExecutionReceipt, NewTask, NewTaskStep,
    ReceiptStatus, SqliteTaskStore, TaskStatus, TaskStepStatus, TaskStepType, TaskStore, TaskWait,
    DEFAULT_LEASE_MS,
};

fn scratch_db(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("utsuwa-task-fault-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn wait_task() -> NewTask {
    NewTask {
        title: "fault-probe".to_string(),
        instruction: "survive a crash".to_string(),
        scheduled_at: None,
        priority: None,
        max_attempts: Some(3),
        verification: None,
        parent_task_id: None,
        steps: vec![NewTaskStep {
            step_type: TaskStepType::Wait,
            input: serde_json::json!({ "duration_ms": 60_000 }),
            max_attempts: None,
        }],
    }
}

fn tool_task() -> NewTask {
    NewTask {
        title: "effect-probe".to_string(),
        instruction: "fire once".to_string(),
        scheduled_at: None,
        priority: None,
        max_attempts: Some(3),
        verification: None,
        parent_task_id: None,
        steps: vec![NewTaskStep {
            step_type: TaskStepType::Tool,
            input: serde_json::json!({ "tool": "test.echo", "args": {} }),
            max_attempts: None,
        }],
    }
}

#[tokio::test]
async fn receipts_survive_restart() {
    let db = scratch_db("receipts");
    let task_id = {
        let store = SqliteTaskStore::open(&db).expect("open");
        let task = store.create(tool_task(), 100).await.expect("create");
        let receipt = ExecutionReceipt {
            execution_id: "exec-1".to_string(),
            task_id: task.id.clone(),
            step_id: task.steps[0].id.clone(),
            idempotency_key: idempotency_key(&task.id, &task.steps[0].id, "test.echo", 1),
            operation: "test.echo".to_string(),
            status: ReceiptStatus::Success,
            started_at: 100,
            finished_at: Some(150),
            external_id: None,
            output: serde_json::json!({ "n": 1 }),
        };
        assert!(store.record_receipt(&receipt).await.expect("record"));
        task.id.clone()
        // Simulated crash: the store drops here without ceremony.
    };
    let store = SqliteTaskStore::open(&db).expect("reopen");
    let listed = store.list_receipts(&task_id, 10).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].execution_id, "exec-1");
    assert_eq!(listed[0].status, ReceiptStatus::Success);
    let replay = store
        .find_success_receipt(&task_id, &listed[0].step_id, "test.echo")
        .await
        .expect("find")
        .expect("success receipt survives restart");
    assert_eq!(replay.output, serde_json::json!({ "n": 1 }));
    let _ = std::fs::remove_file(&db);
}

#[tokio::test]
async fn stale_running_task_recovers_after_restart() {
    let db = scratch_db("stale");
    let task_id = {
        let store = SqliteTaskStore::open(&db).expect("open");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        task.attempts = 1;
        task.lease_until = Some(120);
        task.steps[0].status = TaskStepStatus::Running;
        task.steps[0].attempts = 1;
        store.update(&task, 120).await.expect("running");
        task.id.clone()
        // Crash: Running with an expired lease, mid-step.
    };
    let store = SqliteTaskStore::open(&db).expect("reopen");
    let report = recover_stale(&store, 10_000, DEFAULT_LEASE_MS)
        .await
        .expect("recover");
    assert_eq!(report.recovered, vec![task_id.clone()]);
    let loaded = store.get(&task_id).await.expect("get").expect("task");
    assert_eq!(loaded.status, TaskStatus::Ready);
    assert_eq!(loaded.steps[0].status, TaskStepStatus::Pending);
    assert_eq!(loaded.attempts, 1);
    let _ = std::fs::remove_file(&db);
}

#[tokio::test]
async fn waiting_task_survives_restart_and_expires() {
    let db = scratch_db("waiting");
    let task_id = {
        let store = SqliteTaskStore::open(&db).expect("open");
        let mut task = store.create(wait_task(), 100).await.expect("create");
        task.status = TaskStatus::Ready;
        store.update(&task, 110).await.expect("ready");
        task.status = TaskStatus::Running;
        store.update(&task, 120).await.expect("running");
        task.status = TaskStatus::Waiting;
        task.wait_for = Some(TaskWait {
            event_type: None,
            correlation_id: None,
            timeout_at: Some(5_000),
        });
        store.update(&task, 130).await.expect("waiting");
        task.id.clone()
        // Crash while parked in a duration wait.
    };
    let store = SqliteTaskStore::open(&db).expect("reopen");
    let loaded = store.get(&task_id).await.expect("get").expect("task");
    assert_eq!(loaded.status, TaskStatus::Waiting);
    assert!(loaded.wait_for.is_some());
    let released = expire_waits(&store, 6_000).await.expect("expire");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].status, TaskStatus::Ready);
    assert_eq!(released[0].steps[0].status, TaskStepStatus::Completed);
    let _ = std::fs::remove_file(&db);
}

/// A pre-receipts (schema v1) database must open cleanly under the v2
/// store: existing tasks keep working and receipts become available.
/// The v1 schema is reproduced here in raw SQL so the test pins exactly
/// what shipped before, not what the current code writes.
#[tokio::test]
async fn schema_v1_database_migrates_to_v2() {
    let db = scratch_db("migrate");
    {
        let conn = rusqlite::Connection::open(&db).expect("raw open");
        conn.execute_batch(
            "CREATE TABLE tasks (
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
            CREATE TABLE task_steps (
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
            CREATE TABLE task_events (
                id TEXT PRIMARY KEY,
                event_type TEXT NOT NULL,
                task_id TEXT,
                step_id TEXT,
                correlation_id TEXT,
                payload TEXT NOT NULL DEFAULT '{}',
                created_at INTEGER NOT NULL
            );
            INSERT INTO tasks (id, title, instruction, status, priority, created_at, updated_at,
                               attempts, max_attempts, current_step_index)
            VALUES ('legacy-1', 'legacy', 'from v1', 'pending', 50, 100, 100, 0, 3, 0);
            INSERT INTO task_steps (id, task_id, step_index, step_type, status, input, attempts, max_attempts)
            VALUES ('legacy-step', 'legacy-1', 0, 'wait', 'pending', '{\"duration_ms\": 10}', 0, 1);
            PRAGMA user_version = 1;",
        )
        .expect("v1 schema");
    }
    let store = SqliteTaskStore::open(&db).expect("open migrates");
    let legacy = store.get("legacy-1").await.expect("get").expect("task");
    assert_eq!(legacy.title, "legacy");
    assert_eq!(legacy.steps.len(), 1);
    // Receipts work on the migrated database.
    let receipt = ExecutionReceipt {
        execution_id: "exec-legacy".to_string(),
        task_id: "legacy-1".to_string(),
        step_id: "legacy-step".to_string(),
        idempotency_key: idempotency_key("legacy-1", "legacy-step", "test.echo", 1),
        operation: "test.echo".to_string(),
        status: ReceiptStatus::Success,
        started_at: 200,
        finished_at: Some(250),
        external_id: None,
        output: serde_json::json!({}),
    };
    assert!(store.record_receipt(&receipt).await.expect("record"));
    let version: i32 = rusqlite::Connection::open(&db)
        .expect("raw reopen")
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, task_core::SCHEMA_VERSION);
    let _ = std::fs::remove_file(&db);
}
