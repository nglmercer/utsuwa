//! MCP settings IPC.
//!
//! The WebView configures MCP servers (via the generic `settings.set`
//! `mcp.servers` key) but never executes MCP itself on native builds:
//! connections, discovery, and tool calls all live in `mcp-runtime`,
//! driven by the agent runtime. These three methods are the narrow
//! native bridge around that:
//!
//! - `mcp.status`: per-server connection state (never credentials).
//! - `mcp.connect`: connect one server now for the settings "test" path.
//! - `mcp.set_server_token`: write-only HTTP Bearer [REDACTED] the OS
//!   secret store (empty deletes).
//!
//! All three run off the UI/IPC callback thread (see the dispatcher's
//! dedicated-worker branch): status/connect await the agent worker, and
//! token writes perform synchronous secret-store IPC.

use super::Dispatcher;
use crate::runtime::providers::SETTING_MCP_SERVERS;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;

const MAX_ID_LENGTH: usize = 128;
const MAX_TOKEN_LENGTH: usize = 4096;
const MAX_ERROR_LENGTH: usize = 500;

fn invalid_params(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::InvalidParams,
        message: message.into(),
    }
}

fn internal(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::Internal,
        message: message.into(),
    }
}

fn valid_server_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LENGTH
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn bounded(message: String) -> String {
    if message.len() > MAX_ERROR_LENGTH {
        message.chars().take(MAX_ERROR_LENGTH).collect()
    } else {
        message
    }
}

impl Dispatcher {
    fn require_agent(&self, method: &str) -> Result<&crate::runtime::AgentRuntime, IpcErrorBody> {
        self.agent
            .as_deref()
            .ok_or_else(|| internal(format!("{method} needs the agent runtime (degraded mode)")))
    }

    pub(crate) async fn mcp_status(&self) -> Result<Value, IpcErrorBody> {
        let agent = self.require_agent("mcp.status")?;
        let statuses = agent.mcp_status().await;
        let rows: Vec<Value> = statuses
            .into_iter()
            .map(|status| {
                let has_token = self
                    .secrets
                    .as_ref()
                    .and_then(|secrets| {
                        secrets
                            .get(&mcp_runtime::bearer_secret_account(&status.id))
                            .ok()
                            .flatten()
                    })
                    .is_some_and(|token| !token.is_empty());
                let mut row = serde_json::to_value(&status).unwrap_or(Value::Null);
                if let Some(object) = row.as_object_mut() {
                    object.insert("has_token".to_string(), Value::Bool(has_token));
                }
                row
            })
            .collect();
        Ok(Value::Array(rows))
    }

    pub(crate) async fn mcp_connect(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let agent = self.require_agent("mcp.connect")?;
        let id = request
            .params
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !valid_server_id(id) {
            return Err(invalid_params("mcp.connect needs a valid 'id'"));
        }
        // Sync first so a server the UI just saved is testable immediately
        // (agent turns sync lazily on their own cadence).
        if let Some(storage) = self.storage.as_ref() {
            let configs = {
                let storage = storage
                    .lock()
                    .map_err(|_| internal("storage lock failed"))?;
                let raw = storage.get_setting(SETTING_MCP_SERVERS).map_err(|err| {
                    internal(format!("could not read {SETTING_MCP_SERVERS}: {err}"))
                })?;
                match raw {
                    Some(value) => serde_json::from_value::<Vec<mcp_runtime::McpServerConfig>>(
                        value,
                    )
                    .map_err(|err| {
                        invalid_params(format!("{SETTING_MCP_SERVERS} is not valid: {err}"))
                    })?,
                    None => Vec::new(),
                }
            };
            agent
                .mcp_manager()
                .sync_configs(configs)
                .await
                .map_err(|err| internal(format!("mcp sync failed: {err}")))?;
        }
        let mut registry = tool_core::ToolRegistry::new();
        match agent.mcp_manager().register_into(id, &mut registry).await {
            Ok(tools) => Ok(serde_json::json!({ "tools": tools })),
            Err(err) => Err(internal(format!(
                "mcp connect failed: {}",
                bounded(err.to_string())
            ))),
        }
    }

    pub(crate) async fn mcp_set_server_token(
        &self,
        request: &IpcRequest,
    ) -> Result<Value, IpcErrorBody> {
        let id = request
            .params
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !valid_server_id(id) {
            return Err(invalid_params("mcp.set_server_token needs a valid 'id'"));
        }
        let token = request
            .params
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if token.len() > MAX_TOKEN_LENGTH {
            return Err(invalid_params("mcp.set_server_token 'token' is too long"));
        }
        let secrets = self
            .secrets
            .as_ref()
            .ok_or_else(|| internal("secret store is not attached".to_string()))?;
        let account = mcp_runtime::bearer_secret_account(id);
        if token.is_empty() {
            secrets
                .delete(&account)
                .map_err(|err| internal(format!("could not clear MCP token: {err}")))?;
        } else {
            secrets
                .set(&account, token)
                .map_err(|err| internal(format!("could not store MCP token: {err}")))?;
        }
        // Drop the live client so the next use reconnects with the new
        // token; without an agent yet there is nothing to drop.
        if let Some(agent) = self.agent.as_deref() {
            agent.mcp_manager().drop_client(id).await;
        }
        Ok(serde_json::json!({ "ok": true, "has_token": !token.is_empty() }))
    }
}
