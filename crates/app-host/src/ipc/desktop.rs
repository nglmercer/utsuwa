//! User-facing screen sharing controls.
//!
//! These methods are deliberately separate from the model's `desktop.*`
//! tools. A user clicking Share Screen explicitly starts the host capture
//! session; the model still has to pass through the normal capability-ticket
//! path before it can request a frame or control the desktop.

use super::Dispatcher;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;
use tool_desktop::{CaptureConfig, CaptureQuality, CaptureTarget};

fn invalid(message: impl Into<String>) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::InvalidParams,
        message: message.into(),
    }
}

fn runtime_missing() -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::Internal,
        message: "agent runtime is not attached".to_string(),
    }
}

fn runtime_error(error: impl std::fmt::Display) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::Internal,
        message: error.to_string(),
    }
}

fn capture_target(params: &Value) -> Result<CaptureTarget, IpcErrorBody> {
    let Some(target) = params.get("target") else {
        return Ok(CaptureTarget::Desktop);
    };
    let object = target
        .as_object()
        .ok_or_else(|| invalid("'target' must be an object"))?;
    match object.get("type").and_then(Value::as_str) {
        None | Some("desktop") => Ok(CaptureTarget::Desktop),
        Some("display") => object
            .get("display_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(|id| CaptureTarget::Display(id.to_string()))
            .ok_or_else(|| invalid("display target needs a non-empty 'display_id'")),
        Some("window") => object
            .get("window_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(|id| CaptureTarget::Window(id.to_string()))
            .ok_or_else(|| invalid("window target needs a non-empty 'window_id'")),
        Some(other) => Err(invalid(format!("unknown capture target type '{other}'"))),
    }
}

fn capture_config(params: &Value) -> Result<CaptureConfig, IpcErrorBody> {
    let max_fps = params.get("max_fps").and_then(Value::as_u64).unwrap_or(2);
    if !(1..=60).contains(&max_fps) {
        return Err(invalid("max_fps must be between 1 and 60"));
    }
    let max_width = params
        .get("max_width")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| (1..=8192).contains(value))
                .ok_or_else(|| invalid("max_width must be between 1 and 8192"))
        })
        .transpose()?;
    let quality = match params
        .get("quality")
        .and_then(Value::as_str)
        .unwrap_or("normal")
    {
        "draft" => CaptureQuality::Draft,
        "normal" => CaptureQuality::Normal,
        "high" => CaptureQuality::High,
        other => return Err(invalid(format!("unknown capture quality '{other}'"))),
    };
    Ok(CaptureConfig {
        target: capture_target(params)?,
        max_fps: u32::try_from(max_fps).unwrap_or(2),
        include_cursor: params
            .get("include_cursor")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_width,
        quality,
    })
}

fn status_value(
    runtime: Option<&std::sync::Arc<crate::runtime::AgentRuntime>>,
) -> Result<Value, IpcErrorBody> {
    let runtime = runtime.ok_or_else(runtime_missing)?;
    serde_json::to_value(runtime.screen_share_status()).map_err(runtime_error)
}

impl Dispatcher {
    pub(crate) fn desktop_share_screen_start(
        &self,
        request: &IpcRequest,
    ) -> Result<Value, IpcErrorBody> {
        let runtime = self.agent.as_ref().ok_or_else(runtime_missing)?;
        let config = capture_config(&request.params)?;
        let status = runtime.screen_share_start(config).map_err(runtime_error)?;
        serde_json::to_value(status).map_err(runtime_error)
    }

    pub(crate) fn desktop_share_screen_pause(&self) -> Result<Value, IpcErrorBody> {
        let runtime = self.agent.as_ref().ok_or_else(runtime_missing)?;
        let status = runtime.screen_share_pause().map_err(runtime_error)?;
        serde_json::to_value(status).map_err(runtime_error)
    }

    pub(crate) fn desktop_share_screen_resume(&self) -> Result<Value, IpcErrorBody> {
        let runtime = self.agent.as_ref().ok_or_else(runtime_missing)?;
        let status = runtime.screen_share_resume().map_err(runtime_error)?;
        serde_json::to_value(status).map_err(runtime_error)
    }

    pub(crate) fn desktop_share_screen_stop(&self) -> Result<Value, IpcErrorBody> {
        let runtime = self.agent.as_ref().ok_or_else(runtime_missing)?;
        let status = runtime.screen_share_stop().map_err(runtime_error)?;
        serde_json::to_value(status).map_err(runtime_error)
    }

    pub(crate) fn desktop_share_screen_status(&self) -> Result<Value, IpcErrorBody> {
        status_value(self.agent.as_ref())
    }

    pub(crate) fn desktop_control_set(&self, enabled: bool) -> Result<Value, IpcErrorBody> {
        let runtime = self.agent.as_ref().ok_or_else(runtime_missing)?;
        let status = runtime
            .screen_share_set_control(enabled)
            .map_err(runtime_error)?;
        serde_json::to_value(status).map_err(runtime_error)
    }
}
