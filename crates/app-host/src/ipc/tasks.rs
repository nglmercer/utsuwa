//! Durable task IPC (`task.*`): the native host is the task authority.
//!
//! Every mutating call (create/review/event) persists, wakes the single
//! background tick loop, and returns immediately. The loop dispatches ready
//! tasks to workers, so a slow model turn can never stall an IPC response
//! or the WebView chat. The 2s loop in `main.rs` also covers time-based
//! progress (leases, waits, schedules) when nothing wakes it. Degraded mode
//! (no agent executor, hence no loop or workers) falls back to one inline
//! tick per mutation, so tasks still progress without the runtime.

use crate::ipc::dispatcher::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;
use std::sync::Arc;

fn internal(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::Internal,
        message: message.into(),
    }
}

fn invalid_params(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::InvalidParams,
        message: message.into(),
    }
}

fn task_error(err: task_host::TaskHostError) -> IpcErrorBody {
    match &err {
        task_host::TaskHostError::Core(task_core::TaskCoreError::NotFound(_)) => {
            invalid_params(err.to_string())
        }
        task_host::TaskHostError::Core(task_core::TaskCoreError::Validation(_)) => {
            invalid_params(err.to_string())
        }
        task_host::TaskHostError::Core(task_core::TaskCoreError::IllegalTransition { .. }) => {
            invalid_params(err.to_string())
        }
        _ => internal(err.to_string()),
    }
}

fn param_str(params: &Value, name: &str) -> Result<String, IpcErrorBody> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid_params(format!("missing non-empty string param '{name}'")))
}

impl Dispatcher {
    fn tasks(&self) -> Result<Arc<task_host::TaskHost>, IpcErrorBody> {
        self.tasks
            .clone()
            .ok_or_else(|| internal("task host unavailable (tasks.db could not be opened)"))
    }

    /// Notify the tick loop after a mutation. With workers running this
    /// returns without executing anything; in degraded mode (no loop, no
    /// workers) it drives one inline tick so tasks still progress.
    async fn wake_after_mutation(&self, host: &task_host::TaskHost) {
        host.wake();
        if host.has_workers() {
            return;
        }
        if let Err(err) = host.tick().await {
            tracing::warn!(%err, "task tick failed after ipc (degraded mode)");
        }
    }

    pub(crate) async fn task_create(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let input: task_core::NewTask = serde_json::from_value(request.params.clone())
            .map_err(|err| invalid_params(format!("invalid task body: {err}")))?;
        let task = host.create(input).await.map_err(task_error)?;
        let result = serde_json::to_value(&task).map_err(|err| internal(err.to_string()))?;
        self.wake_after_mutation(&host).await;
        Ok(result)
    }

    pub(crate) async fn task_get(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let task_id = param_str(&request.params, "task_id")?;
        let task = host.get(&task_id).await.map_err(task_error)?;
        match task {
            Some(task) => serde_json::to_value(&task).map_err(|err| internal(err.to_string())),
            None => Err(invalid_params(format!("unknown task '{task_id}'"))),
        }
    }

    pub(crate) async fn task_list(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let status = match request.params.get("status").and_then(Value::as_str) {
            None => None,
            Some(raw) => Some(
                task_core::TaskStatus::parse(raw)
                    .ok_or_else(|| invalid_params("unknown task status filter"))?,
            ),
        };
        let limit = request
            .params
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(50);
        let tasks = host.list(status, limit).await.map_err(task_error)?;
        serde_json::to_value(&tasks).map_err(|err| internal(err.to_string()))
    }

    pub(crate) async fn task_cancel(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let task_id = param_str(&request.params, "task_id")?;
        let reason = request
            .params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("cancelled from frontend")
            .to_string();
        let task = host.cancel(&task_id, reason).await.map_err(task_error)?;
        serde_json::to_value(&task).map_err(|err| internal(err.to_string()))
    }

    pub(crate) async fn task_review(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let task_id = param_str(&request.params, "task_id")?;
        let approved = request
            .params
            .get("approved")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid_params("missing boolean param 'approved'"))?;
        let note = request
            .params
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_string);
        let task = host
            .review(&task_id, approved, note)
            .await
            .map_err(task_error)?;
        let result = serde_json::to_value(&task).map_err(|err| internal(err.to_string()))?;
        self.wake_after_mutation(&host).await;
        Ok(result)
    }

    pub(crate) async fn task_event(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let host = self.tasks()?;
        let event_type = param_str(&request.params, "event_type")?;
        let correlation_id = request
            .params
            .get("correlation_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let payload = request
            .params
            .get("payload")
            .cloned()
            .unwrap_or(Value::Null);
        let task = host
            .deliver_event(&event_type, correlation_id.as_deref(), payload)
            .await
            .map_err(task_error)?;
        self.wake_after_mutation(&host).await;
        Ok(serde_json::json!({
            "delivered": task.is_some(),
            "task": task,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc_core::{IpcMethod, IpcRequest};

    fn dispatcher() -> (Dispatcher, Arc<task_host::TaskHost>) {
        let registry = Arc::new(tool_core::ToolRegistry::new());
        let emit: task_host::EmitFn = Arc::new(|_| {});
        let host = Arc::new(
            task_host::TaskHost::open_in_memory(registry, emit, None).expect("in-memory tasks"),
        );
        (Dispatcher::new("test").with_tasks(Arc::clone(&host)), host)
    }

    fn request(method: IpcMethod, params: serde_json::Value) -> IpcRequest {
        IpcRequest::new(method, params)
    }

    #[tokio::test]
    async fn task_ipc_round_trip() {
        let (dispatcher, host) = dispatcher();
        let created = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskCreate,
                serde_json::json!({
                    "title": "ipc-task",
                    "instruction": "do it",
                    "steps": [{ "step_type": "wait", "input": { "duration_ms": 0 } }],
                }),
            ))
            .await
            .expect("create");
        // Create persists and wakes only: no inline execution anymore.
        assert_eq!(created["title"], "ipc-task");
        assert_eq!(created["status"], "pending");
        let task_id = created["id"].as_str().expect("id").to_string();
        // The background loop's scoped tick completes the zero-duration wait.
        host.tick_one(&task_id).await.expect("tick");

        let listed = dispatcher
            .dispatch_async(&request(IpcMethod::TaskList, serde_json::json!({})))
            .await
            .expect("list");
        assert_eq!(listed.as_array().expect("array").len(), 1);

        let fetched = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskGet,
                serde_json::json!({ "task_id": task_id }),
            ))
            .await
            .expect("get");
        assert_eq!(fetched["status"], "completed");
    }

    #[tokio::test]
    async fn task_review_and_cancel_flow() {
        let (dispatcher, host) = dispatcher();
        let created = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskCreate,
                serde_json::json!({
                    "title": "approval-task",
                    "instruction": "approve me",
                    "steps": [{ "step_type": "approval", "input": { "reason": "ok?" } }],
                }),
            ))
            .await
            .expect("create");
        let task_id = created["id"].as_str().expect("id").to_string();
        // The background loop's scoped tick parks it in needs_review.
        host.tick_one(&task_id).await.expect("tick");
        let parked = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskGet,
                serde_json::json!({ "task_id": task_id }),
            ))
            .await
            .expect("get");
        assert_eq!(parked["status"], "needs_review");

        let approved = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskReview,
                serde_json::json!({ "task_id": task_id, "approved": true }),
            ))
            .await
            .expect("review");
        assert_eq!(approved["status"], "ready");
        // Review only resumes: the loop's tick runs the approved step.
        host.tick_one(&task_id).await.expect("tick");

        let done = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskGet,
                serde_json::json!({ "task_id": task_id }),
            ))
            .await
            .expect("get");
        assert_eq!(done["status"], "completed");

        // Cancelling a terminal task is rejected, not silently ignored.
        let err = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskCancel,
                serde_json::json!({ "task_id": task_id }),
            ))
            .await
            .expect_err("cancel terminal");
        assert_eq!(err.code, ErrorCode::Internal);
    }

    #[tokio::test]
    async fn task_create_rejects_invalid_bodies() {
        let (dispatcher, _host) = dispatcher();
        let err = dispatcher
            .dispatch_async(&request(IpcMethod::TaskCreate, serde_json::json!({})))
            .await
            .expect_err("empty body");
        assert_eq!(err.code, ErrorCode::InvalidParams);
        let err = dispatcher
            .dispatch_async(&request(
                IpcMethod::TaskCreate,
                serde_json::json!({ "title": "x", "instruction": "y", "steps": [] }),
            ))
            .await
            .expect_err("no steps");
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }
}
