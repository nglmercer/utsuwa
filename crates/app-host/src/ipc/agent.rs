//! Agent-turn IPC: send, history parsing, cancel.
use super::Dispatcher;
use crate::runtime::AgentRequest;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use model_core::{ModelMessage, ModelRole};
use serde_json::Value;

impl Dispatcher {
    /// Start (or supersede) an agent turn. Returns immediately; the text,
    /// tool results, and approval prompts arrive as host events
    /// (`agent.turn_done`, `permission.requested`, …).
    pub(crate) fn parse_agent_history(
        value: Option<&Value>,
    ) -> Result<Vec<ModelMessage>, IpcErrorBody> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };
        let entries = value.as_array().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: "agent.send_message 'history' must be an array".to_string(),
        })?;
        if entries.len() > 100 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message 'history' exceeds 100 messages".to_string(),
            });
        }
        entries
            .iter()
            .map(|entry| {
                let object = entry.as_object().ok_or_else(|| IpcErrorBody {
                    code: ErrorCode::InvalidParams,
                    message: "agent history entries must be objects".to_string(),
                })?;
                let role = match object.get("role").and_then(Value::as_str) {
                    Some("system") => ModelRole::System,
                    Some("user") => ModelRole::User,
                    Some("assistant") => ModelRole::Assistant,
                    _ => {
                        return Err(IpcErrorBody {
                            code: ErrorCode::InvalidParams,
                            message: "agent history role must be system, user, or assistant"
                                .to_string(),
                        })
                    }
                };
                let content = object.get("content").cloned().unwrap_or(Value::Null);
                Ok(ModelMessage::from_wire(role, content))
            })
            .collect()
    }
    pub(crate) fn agent_send(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        let text = request
            .params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message needs a string 'text'".to_string(),
            })?;
        let text = text.trim();
        let history = Self::parse_agent_history(request.params.get("history"))?;
        let append_user_message = request
            .params
            .get("append_user_message")
            .and_then(Value::as_bool)
            .unwrap_or(history.is_empty());
        if text.is_empty() && history.is_empty() {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message needs text or history".to_string(),
            });
        }
        if text.len() > 32 * 1024 {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message 'text' exceeds 32 KiB".to_string(),
            });
        }
        let system_prompt = request
            .params
            .get("system_prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        if system_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.len() > 128 * 1024)
        {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "agent.send_message 'system_prompt' exceeds 128 KiB".to_string(),
            });
        }
        agent
            .send_request(AgentRequest {
                text: text.to_string(),
                history,
                system_prompt,
                append_user_message,
            })
            .map_err(|err| IpcErrorBody {
                code: ErrorCode::Internal,
                message: err.to_string(),
            })?;
        Ok(serde_json::json!({ "ok": true, "accepted": true }))
    }
    pub(crate) fn agent_cancel(&self) -> Result<Value, IpcErrorBody> {
        let agent = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        agent.cancel();
        Ok(serde_json::json!({ "ok": true }))
    }
}
