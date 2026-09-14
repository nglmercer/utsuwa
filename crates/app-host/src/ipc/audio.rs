//! Native CPAL capture IPC. The audio bytes themselves stay in the host's
//! temporary media registry; only capture metadata crosses typed IPC.

use super::Dispatcher;
use audio_capture::AudioCaptureConfig;
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use serde_json::Value;

impl Dispatcher {
    pub(crate) fn camera_activity_status(&self) -> Result<Value, IpcErrorBody> {
        if let Some(sensors) = self.sensors.as_ref() {
            return serde_json::to_value(sensors.camera_status()).map_err(|error| IpcErrorBody {
                code: ErrorCode::Internal,
                message: error.to_string(),
            });
        }
        let runtime = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        serde_json::to_value(runtime.camera_activity_status()).map_err(|error| IpcErrorBody {
            code: ErrorCode::Internal,
            message: error.to_string(),
        })
    }

    pub(crate) fn microphone_activity_status(&self) -> Result<Value, IpcErrorBody> {
        if let Some(sensors) = self.sensors.as_ref() {
            return serde_json::to_value(sensors.microphone_status()).map_err(|error| {
                IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: error.to_string(),
                }
            });
        }
        let runtime = self.agent.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "agent runtime is not attached".to_string(),
        })?;
        serde_json::to_value(runtime.microphone_activity_status()).map_err(|error| IpcErrorBody {
            code: ErrorCode::Internal,
            message: error.to_string(),
        })
    }

    pub(crate) fn audio_capture_start(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        let manager = self.audio_capture.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "native audio capture is not attached".to_string(),
        })?;
        let config_value = request
            .params
            .get("config")
            .cloned()
            .unwrap_or_else(|| request.params.clone());
        let config = if config_value.is_null() {
            AudioCaptureConfig::default()
        } else {
            serde_json::from_value(config_value).map_err(|error| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: format!("invalid audio capture config: {error}"),
            })?
        };
        manager.start(config).map_err(audio_error)
    }

    pub(crate) fn audio_capture_stop(&self) -> Result<Value, IpcErrorBody> {
        let manager = self.audio_capture.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "native audio capture is not attached".to_string(),
        })?;
        manager.stop().map_err(audio_error)
    }

    pub(crate) fn audio_capture_cancel(&self) -> Result<Value, IpcErrorBody> {
        let manager = self.audio_capture.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "native audio capture is not attached".to_string(),
        })?;
        manager.cancel().map_err(audio_error)?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

fn audio_error(error: audio_capture::AudioError) -> IpcErrorBody {
    IpcErrorBody {
        code: ErrorCode::Internal,
        message: error.to_string(),
    }
}
