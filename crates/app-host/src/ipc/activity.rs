//! Audit-activity listing IPC.
use super::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;

impl Dispatcher {
    /// Recent audit records, newest first, for the Activity panel
    /// (plan Phase 35: time, tool, status, resource, principal, duration).
    /// `params.limit` clamps to 1..=200 (default 50). Details are already
    /// redacted at record time.
    pub(crate) fn activity_list(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let audit = self.audit.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "audit sink is not attached".to_string(),
        })?;
        let limit = request
            .params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let mut records = audit.records();
        records.reverse();
        records.truncate(limit);
        serde_json::to_value(&records).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
    }
}
