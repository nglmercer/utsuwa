//! Microphone audio tools (`audio.*`) over `audio-capture`.
//!
//! Capture requires a [`capability_core::Capability::MicrophoneCapture`]
//! ticket — microphone access always needs explicit user authorization.
//! [`AudioState`] tracks whether the microphone is active so the frontend
//! can render a visible indicator (`Microphone: ON`). Recordings land in
//! [`artifact_core`] as expiring sensitive audio artifacts; transcription
//! is intentionally not hardwired to any cloud provider (a model provider
//! or plugin consumes the artifact instead).

use artifact_core::{ArtifactSource, ArtifactStore, ContentPart};
use audio_capture::{AudioCapture, AudioCaptureConfig, AudioError, RecordedAudio};
use capability_core::{Capability, Resource};
use std::collections::HashMap;
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// Frontend-visible microphone indicator plus session registry.
#[derive(Clone, Default)]
pub struct AudioState {
    active: Arc<std::sync::atomic::AtomicBool>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, AudioSession>>>,
}

struct AudioSession {
    device: String,
    started_at_ms: u64,
    capture: Arc<std::sync::Mutex<AudioCapture>>,
    finished: Option<RecordedAudio>,
}

impl AudioState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn audio_error(tool: &str, error: AudioError) -> ToolError {
    match error {
        AudioError::NoInputDevice => ToolError::structured(
            tool,
            "backend_unavailable",
            "no microphone input device found",
        ),
        AudioError::AlreadyRunning => ToolError::structured(
            tool,
            "action_failed",
            "a capture session is already running",
        ),
        other => ToolError::structured_with_details(
            tool,
            "action_failed",
            other.to_string(),
            serde_json::json!({ "detail": other.to_string() }),
        ),
    }
}

fn require_microphone(tool: &str, ctx: &ToolContext, device: &str) -> Result<(), ToolError> {
    if ctx.has_ticket(
        Capability::MicrophoneCapture,
        Resource::Application(device.to_string()),
    ) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "microphone capture needs explicit user authorization for this device",
            serde_json::json!({ "capability": "MicrophoneCapture", "device": device }),
        ))
    }
}

fn device_scope(device: &str) -> Resource {
    Resource::Application(device.to_string())
}

pub struct AudioDeps {
    pub artifacts: Arc<dyn ArtifactStore>,
    pub state: AudioState,
}

pub struct AudioListDevicesTool;
pub struct AudioStatusTool {
    pub deps: AudioDeps,
}
pub struct AudioCaptureStartTool {
    pub deps: AudioDeps,
}
pub struct AudioCaptureStatusTool {
    pub deps: AudioDeps,
}
pub struct AudioCaptureStopTool {
    pub deps: AudioDeps,
}
pub struct AudioRecordTool {
    pub deps: AudioDeps,
}

fn list_input_devices() -> Result<Vec<serde_json::Value>, AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|error| AudioError::DeviceName(error.to_string()))?;
    let mut out = Vec::new();
    for device in devices {
        let name = device
            .name()
            .map_err(|error| AudioError::DeviceName(error.to_string()))?;
        let config = device
            .default_input_config()
            .map(|config| config.config())
            .ok();
        out.push(serde_json::json!({
            "device": name,
            "sample_rate": config.as_ref().map(|config| config.sample_rate.0),
            "channels": config.as_ref().map(|config| config.channels),
        }));
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Tool for AudioListDevicesTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.list_devices"),
            description:
                "List microphone input devices. Capturing still needs per-device authorization."
                    .to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        if !args.is_object() {
            return Err(invalid("audio.list_devices", "args must be a JSON object"));
        }
        let devices = tokio::task::spawn_blocking(list_input_devices)
            .await
            .map_err(|error| {
                ToolError::structured("audio.list_devices", "action_failed", error.to_string())
            })?
            .map_err(|error| audio_error("audio.list_devices", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "devices": devices })))
    }
}

#[async_trait::async_trait]
impl Tool for AudioStatusTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.status"),
            description: "Report whether microphone capture is active (frontend indicator state)."
                .to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let sessions = self.deps.state.sessions.lock().await;
        let detail = sessions
            .iter()
            .map(|(id, session)| {
                serde_json::json!({
                    "session_id": id,
                    "device": session.device,
                    "started_at_ms": session.started_at_ms,
                    "finished": session.finished.is_some(),
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolOutput::json(serde_json::json!({
            "microphone_on": self.deps.state.is_active(),
            "sessions": detail.len(),
            "detail": detail,
        })))
    }
}

fn device_arg(args: &serde_json::Value, _tool: &str) -> String {
    args.get("device")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("default")
        .to_string()
}

fn duration_arg(
    args: &serde_json::Value,
    tool: &str,
    default_ms: u64,
    max_ms: u64,
) -> Result<u64, ToolError> {
    args.get("max_duration_ms")
        .or_else(|| args.get("duration_ms"))
        .map(|value| {
            value
                .as_u64()
                .filter(|duration| (1..=max_ms).contains(duration))
                .ok_or_else(|| invalid(tool, format!("duration must be between 1 and {max_ms} ms")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default_ms))
}

#[async_trait::async_trait]
impl Tool for AudioCaptureStartTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.capture_start"),
            description:
                "Start a microphone session. The frontend must show Microphone: ON while it runs."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "device": {"type": "string"},
                    "max_duration_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
                },
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::MicrophoneCapture,
            resource: device_scope(&device_arg(args, "audio.capture_start")),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let device = device_arg(&args, "audio.capture_start");
        let max_duration_ms = duration_arg(&args, "audio.capture_start", 45_000, 300_000)?;
        require_microphone("audio.capture_start", &ctx, &device)?;
        // AudioCapture methods take `&mut self` and block: the live
        // handle lives behind a mutex and every call runs on a blocking
        // thread, so the async executor never stalls on device I/O.
        let handle: Arc<std::sync::Mutex<AudioCapture>> =
            Arc::new(std::sync::Mutex::new(AudioCapture::new()));
        let starter = handle.clone();
        let info = tokio::task::spawn_blocking(move || {
            let config = AudioCaptureConfig {
                max_duration_ms,
                ..AudioCaptureConfig::default()
            };
            starter
                .lock()
                .map_err(|_| AudioError::Worker("audio session lock failed".to_string()))?
                .start(config, |_| {})
        })
        .await
        .map_err(|error| {
            ToolError::structured("audio.capture_start", "action_failed", error.to_string())
        })?
        .map_err(|error| audio_error("audio.capture_start", error))?;
        let session_id = uuid::Uuid::new_v4().to_string();
        self.deps.state.sessions.lock().await.insert(
            session_id.clone(),
            AudioSession {
                device: device.clone(),
                started_at_ms: unix_millis(),
                capture: handle,
                finished: None,
            },
        );
        self.deps
            .state
            .active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::json(serde_json::json!({
            "session_id": session_id,
            "device": info.device,
            "sample_rate": info.sample_rate,
            "channels": info.channels,
        })))
    }
}

#[async_trait::async_trait]
impl Tool for AudioCaptureStatusTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.capture_status"),
            description: "Inspect one microphone session (device, level stats when available)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "session_id": {"type": "string"} },
                "required": ["session_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let session_id = args
            .get("session_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("audio.capture_status", "missing string 'session_id'"))?;
        let sessions = self.deps.state.sessions.lock().await;
        let Some(session) = sessions.get(session_id) else {
            return Err(ToolError::structured_with_details(
                "audio.capture_status",
                "session_not_found",
                format!("audio session '{session_id}' not found"),
                serde_json::json!({ "session_id": session_id }),
            ));
        };
        require_microphone("audio.capture_status", &ctx, &session.device)?;
        Ok(ToolOutput::json(serde_json::json!({
            "session_id": session_id,
            "device": session.device,
            "started_at_ms": session.started_at_ms,
            "finished": session.finished.is_some(),
            "stats": session.finished.as_ref().map(|finished| serde_json::json!({
                "duration_ms": finished.duration_ms,
                "bytes": finished.bytes,
            })),
        })))
    }
}

fn session_arg(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
    args.get("session_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'session_id'"))
}

#[async_trait::async_trait]
impl Tool for AudioCaptureStopTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.capture_stop"),
            description: "Stop a microphone session and return the WAV recording as an expiring sensitive artifact.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "session_id": {"type": "string"} },
                "required": ["session_id"],
            }),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let session_id = session_arg(&args, "audio.capture_stop")?;
        let handle = {
            let mut sessions = self.deps.state.sessions.lock().await;
            let Some(session) = sessions.remove(&session_id) else {
                return Err(ToolError::structured_with_details(
                    "audio.capture_stop",
                    "session_not_found",
                    format!("audio session '{session_id}' not found"),
                    serde_json::json!({ "session_id": session_id }),
                ));
            };
            require_microphone("audio.capture_stop", &ctx, &session.device)?;
            if sessions.is_empty() {
                self.deps
                    .state
                    .active
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            // Deterministic cleanup: recordings never outlive their session.
            self.deps
                .artifacts
                .delete_source(ArtifactSource::AudioCapture)
                .await;
            (session.device.clone(), session.capture.clone())
        };
        let (device, handle) = handle;
        let recorded = tokio::task::spawn_blocking(move || {
            handle
                .lock()
                .map_err(|_| AudioError::Worker("audio session lock failed".to_string()))?
                .stop()
        })
        .await
        .map_err(|error| {
            ToolError::structured("audio.capture_stop", "action_failed", error.to_string())
        })?
        .map_err(|error| audio_error("audio.capture_stop", error))?;
        finish_recording(&self.deps.artifacts, &device, &session_id, recorded).await
    }
}

async fn finish_recording(
    artifacts: &Arc<dyn ArtifactStore>,
    device: &str,
    session_id: &str,
    recorded: RecordedAudio,
) -> Result<ToolOutput, ToolError> {
    let artifact = artifacts
        .put_with_source(
            "audio/wav",
            recorded.wav_data,
            ArtifactSource::AudioCapture,
            true,
        )
        .await
        .map_err(|error| {
            ToolError::structured("audio.capture_stop", "action_failed", error.to_string())
        })?;
    Ok(ToolOutput::multipart(
        serde_json::json!({
            "session_id": session_id,
            "device": device,
            "artifact_id": artifact.id,
            "mime_type": artifact.mime_type,
            "size_bytes": artifact.size_bytes,
            "duration_ms": recorded.duration_ms,
        }),
        vec![ContentPart::Audio(artifact)],
    ))
}

#[async_trait::async_trait]
impl Tool for AudioRecordTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("audio.record"),
            description: "Record the microphone for a bounded duration and return WAV as an expiring sensitive artifact.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "device": {"type": "string"},
                    "duration_ms": {"type": "integer", "minimum": 100, "maximum": 120000},
                },
            }),
            effects: vec![ToolEffect::ExternalSideEffect],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::MicrophoneCapture,
            resource: device_scope(&device_arg(args, "audio.record")),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let device = device_arg(&args, "audio.record");
        let duration_ms = duration_arg(&args, "audio.record", 5_000, 120_000)?;
        require_microphone("audio.record", &ctx, &device)?;
        self.deps
            .state
            .active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let config = AudioCaptureConfig {
            max_duration_ms: duration_ms,
            auto_stop: true,
            retain_audio: true,
            ..AudioCaptureConfig::default()
        };
        let recorded = tokio::task::spawn_blocking(move || {
            let mut capture = AudioCapture::new();
            capture.start(config, |_| {})?;
            // Block until the worker finishes (auto-stop at max duration).
            // Poll stop(): it joins the worker and returns the recording.
            capture.stop()
        })
        .await
        .map_err(|error| ToolError::structured("audio.record", "action_failed", error.to_string()))?
        .map_err(|error| audio_error("audio.record", error))?;
        self.deps
            .state
            .active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        finish_recording(&self.deps.artifacts, &device, "one-shot", recorded).await
    }
}

/// Static audio tool group.
pub struct AudioToolPack {
    pub artifacts: Arc<dyn ArtifactStore>,
    pub state: AudioState,
}

impl AudioToolPack {
    pub fn new() -> Self {
        Self {
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
            state: AudioState::new(),
        }
    }
}

impl Default for AudioToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl tool_sdk::ToolPack for AudioToolPack {
    fn id(&self) -> &'static str {
        "audio"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        let deps = || AudioDeps {
            artifacts: self.artifacts.clone(),
            state: self.state.clone(),
        };
        vec![
            Arc::new(AudioListDevicesTool),
            Arc::new(AudioStatusTool { deps: deps() }),
            Arc::new(AudioCaptureStartTool { deps: deps() }),
            Arc::new(AudioCaptureStatusTool { deps: deps() }),
            Arc::new(AudioCaptureStopTool { deps: deps() }),
            Arc::new(AudioRecordTool { deps: deps() }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use std::time::Duration;
    use tool_sdk::ToolPack as _;

    fn ctx_for(device: &str) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            Capability::MicrophoneCapture,
            capability_core::ResourceScope::new(vec![device_scope(device)]),
            ctx.invocation_id,
            Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    #[tokio::test]
    async fn capture_start_requires_microphone_authorization() {
        let pack = AudioToolPack::new();
        let start = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .into_iter()
            .find(|tool| tool.metadata().id.0 == "audio.capture_start")
            .unwrap();
        let err = start
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        // Wrong-device tickets do not authorize the default device.
        let err = start
            .invoke(ctx_for("other"), serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
    }

    #[test]
    fn pack_registers_record_and_session_tools() {
        let pack = AudioToolPack::new();
        let mut ids = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "audio.capture_start",
                "audio.capture_status",
                "audio.capture_stop",
                "audio.list_devices",
                "audio.record",
                "audio.status",
            ]
        );
    }

    #[test]
    fn durations_are_bounded() {
        assert!(
            duration_arg(&serde_json::json!({}), "audio.record", 5_000, 120_000).unwrap() == 5_000
        );
        assert!(duration_arg(
            &serde_json::json!({"duration_ms": 0}),
            "audio.record",
            5_000,
            120_000
        )
        .is_err());
        assert!(duration_arg(
            &serde_json::json!({"duration_ms": 999_999}),
            "audio.record",
            5_000,
            120_000
        )
        .is_err());
    }
}
