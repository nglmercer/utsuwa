//! Plugin lifecycle IPC behind `plugin.*`.
use super::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;

impl Dispatcher {
    /// Every known WASM plugin with trust + lifecycle state, for the
    /// Plugins panel (plan Phase 24/25). Needs the agent runtime for its
    /// plugin manager; without one the panel reports unavailability.
    pub(crate) fn plugin_list(&self) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        serde_json::to_value(agent.plugin_manager().infos()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
    }
    /// One lifecycle transition by plugin id: enable / disable / update /
    /// remove (plan Phase 24). Activation never grants authority — it only
    /// loads code and registers tools behind policy + tickets.
    pub(crate) fn plugin_manage(
        &self,
        request: &IpcRequest,
        op: PluginOp,
    ) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        let id = request
            .params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "plugin lifecycle methods need a string 'id'".to_string(),
            })?;
        if id.is_empty() || id.len() > 128 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "plugin 'id' must be 1-128 chars".to_string(),
            });
        }
        let manager = agent.plugin_manager();
        let outcome = match op {
            PluginOp::Enable => manager.enable(id),
            PluginOp::Disable => manager.disable(id),
            PluginOp::Update => manager.update(id),
            PluginOp::Remove => manager.remove(id),
        };
        outcome.map_err(|err| IpcErrorBody {
            // Unknown ids are a caller error; engine/lifecycle failures
            // are host-side.
            code: match err {
                plugin_wasm::WasmError::Unknown(_) => ErrorCode::InvalidParams,
                _ => ErrorCode::Internal,
            },
            message: err.to_string(),
        })?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// A plugin lifecycle operation behind `plugin.*` IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginOp {
    Enable,
    Disable,
    Update,
    Remove,
}
