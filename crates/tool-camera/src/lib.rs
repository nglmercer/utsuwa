//! Permissioned camera tools (`camera.*`).
//!
//! Every photo or frame requires a [`capability_core::Capability::CameraObserve`]
//! ticket for the device — camera access always needs explicit user
//! authorization and is never implied by screen-capture grants.
//! [`CameraState`] tracks whether capture is active so the frontend can
//! render a visible indicator (`Camera: ON`) whenever the camera runs.
//! Frames land in [`artifact_core`] as expiring sensitive artifacts and are
//! never persisted automatically.

use artifact_core::{ArtifactOwner, ArtifactSource, ArtifactStore, ContentPart, ImageArtifactRef};
use camera_capture::{CameraBackend, CameraDevice};
use capability_core::{Capability, Resource};
use std::collections::BTreeMap;
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// Frontend-visible camera indicator. Cloneable handle shared between the
/// tools and the host state publisher.
///
/// Honest lifecycle note: a camera "session" is a logical grouping owned
/// by these tools — it scopes permission checks (`capture_frame` must
/// present a ticket for the session's device), the `Camera: ON`
/// indicator, and exact-session artifact cleanup. The underlying
/// [`camera_capture::CameraBackend`] only supports snapshots, so each
/// frame opens the device independently; `capture_start` does NOT hold
/// the camera open continuously. If a backend ever offers true streaming
/// sessions, this is where the handle would live.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct CameraActivityState {
    pub active: bool,
    pub session_count: usize,
    pub device: Option<String>,
    /// Unix milliseconds. The host and frontend treat this as metadata only.
    pub started_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct CameraState {
    active: Arc<std::sync::atomic::AtomicBool>,
    sessions: Arc<tokio::sync::Mutex<std::collections::HashMap<String, CameraSession>>>,
    activity_sessions: Arc<std::sync::Mutex<BTreeMap<String, CameraActivitySession>>>,
    activity: Arc<std::sync::RwLock<CameraActivityState>>,
    activity_changes: tokio::sync::watch::Sender<CameraActivityState>,
}

#[derive(Debug, Clone)]
struct CameraActivitySession {
    camera_id: String,
    started_at: u64,
}

#[derive(Debug, Clone)]
struct CameraSession {
    camera_id: String,
    started_at_ms: u64,
    frame_count: u64,
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl CameraState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any camera capture is currently active (frontend indicator).
    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of active logical camera sessions represented by the capture
    /// state. Snapshot backends open the device for each frame.
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Subscribe to authoritative camera activity transitions for the host's
    /// IPC publisher. The receiver is event-driven; callers do not need to
    /// poll the model-facing `camera.status` tool.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<CameraActivityState> {
        self.activity_changes.subscribe()
    }

    pub fn activity_state(&self) -> CameraActivityState {
        self.activity
            .read()
            .map(|state| state.clone())
            .unwrap_or_else(|_| CameraActivityState {
                active: true,
                session_count: 0,
                device: None,
                started_at: None,
            })
    }

    fn begin_activity(&self, session_id: String, camera_id: String, started_at: u64) {
        if let Ok(mut sessions) = self.activity_sessions.lock() {
            sessions.insert(
                session_id,
                CameraActivitySession {
                    camera_id,
                    started_at,
                },
            );
        }
        self.publish_activity();
    }

    fn end_activity(&self, session_id: &str) {
        if let Ok(mut sessions) = self.activity_sessions.lock() {
            sessions.remove(session_id);
        }
        self.publish_activity();
    }

    fn publish_activity(&self) {
        let state = match self.activity_sessions.lock() {
            Ok(sessions) => {
                let first = sessions.values().min_by_key(|session| session.started_at);
                CameraActivityState {
                    active: !sessions.is_empty(),
                    session_count: sessions.len(),
                    device: first.map(|session| session.camera_id.clone()),
                    started_at: first.map(|session| session.started_at),
                }
            }
            Err(_) => CameraActivityState {
                // The host cannot prove that the native camera is idle after
                // a poisoned state lock, so keep the privacy indicator on.
                active: true,
                session_count: 0,
                device: None,
                started_at: None,
            },
        };
        self.active
            .store(state.active, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut current) = self.activity.write() {
            *current = state.clone();
        }
        let _ = self.activity_changes.send(state);
    }
}

impl Default for CameraState {
    fn default() -> Self {
        let (activity_changes, _) = tokio::sync::watch::channel(CameraActivityState::default());
        Self {
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            activity_sessions: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            activity: Arc::new(std::sync::RwLock::new(CameraActivityState::default())),
            activity_changes,
        }
    }
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn camera_error(tool: &str, error: camera_capture::CameraError) -> ToolError {
    match error {
        camera_capture::CameraError::BackendUnavailable(detail) => {
            ToolError::structured_with_details(
                tool,
                "backend_unavailable",
                detail.clone(),
                serde_json::json!({"detail": detail}),
            )
        }
        camera_capture::CameraError::UnknownCamera(id) => ToolError::structured_with_details(
            tool,
            "invalid_target",
            format!("unknown camera '{id}'"),
            serde_json::json!({"camera_id": id, "next_tool": "camera.list"}),
        ),
        camera_capture::CameraError::CaptureFailed(detail) => ToolError::structured_with_details(
            tool,
            "action_failed",
            detail.clone(),
            serde_json::json!({"detail": detail}),
        ),
    }
}

fn require_camera(tool: &str, ctx: &ToolContext, camera_id: &str) -> Result<(), ToolError> {
    if ctx.has_ticket(
        Capability::CameraObserve,
        Resource::Camera(camera_id.to_string()),
    ) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "camera access needs explicit user authorization for this device",
            serde_json::json!({ "capability": "CameraObserve", "camera_id": camera_id }),
        ))
    }
}

fn camera_arg(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
    args.get("camera_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'camera_id'"))
}

fn max_width_arg(args: &serde_json::Value, tool: &str) -> Result<Option<u32>, ToolError> {
    args.get("max_width")
        .map(|value| {
            value
                .as_u64()
                .and_then(|width| u32::try_from(width).ok())
                .filter(|width| (1..=8192).contains(width))
                .ok_or_else(|| invalid(tool, "max_width must be between 1 and 8192"))
        })
        .transpose()
}

pub struct CameraDeps {
    pub backend: Arc<dyn CameraBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub state: CameraState,
}

pub struct CameraListTool {
    pub deps: CameraDeps,
}
pub struct CameraStatusTool {
    pub deps: CameraDeps,
}
pub struct CameraCapturePhotoTool {
    pub deps: CameraDeps,
}
pub struct CameraCaptureStartTool {
    pub deps: CameraDeps,
}
pub struct CameraCaptureFrameTool {
    pub deps: CameraDeps,
}
pub struct CameraCaptureStopTool {
    pub deps: CameraDeps,
}

impl CameraDeps {
    fn shared(&self) -> (Arc<dyn CameraBackend>, Arc<dyn ArtifactStore>, CameraState) {
        (
            self.backend.clone(),
            self.artifacts.clone(),
            self.state.clone(),
        )
    }
}

#[async_trait::async_trait]
impl Tool for CameraListTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.list"),
            description:
                "List camera devices (id, label). Observation still needs per-device authorization."
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
            return Err(invalid("camera.list", "args must be a JSON object"));
        }
        let (backend, _, _) = self.deps.shared();
        let devices: Vec<CameraDevice> =
            tokio::task::spawn_blocking(move || backend.list_cameras())
                .await
                .map_err(|error| {
                    ToolError::structured("camera.list", "action_failed", error.to_string())
                })?
                .map_err(|error| camera_error("camera.list", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "cameras": devices })))
    }
}

#[async_trait::async_trait]
impl Tool for CameraStatusTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.status"),
            description: "Report whether camera capture is active (frontend indicator state)."
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
        let (_, _, state) = self.deps.shared();
        let sessions = state.sessions.lock().await;
        let detail = sessions
            .iter()
            .map(|(id, session)| {
                serde_json::json!({
                    "session_id": id,
                    "camera_id": session.camera_id,
                    "started_at_ms": session.started_at_ms,
                    "frame_count": session.frame_count,
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolOutput::json(serde_json::json!({
            "camera_on": state.is_active(),
            "sessions": detail.len(),
            "detail": detail,
        })))
    }
}

#[async_trait::async_trait]
impl Tool for CameraCapturePhotoTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.capture_photo"),
            description: "Capture one still photo as an expiring sensitive artifact. Requires explicit per-device authorization; never persists automatically.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "camera_id": {"type": "string"},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 8192},
                },
                "required": ["camera_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::CameraObserve,
            resource: Resource::Camera(camera_arg(args, "camera.capture_photo").ok()?),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let camera_id = camera_arg(&args, "camera.capture_photo")?;
        let max_width = max_width_arg(&args, "camera.capture_photo")?;
        require_camera("camera.capture_photo", &ctx, &camera_id)?;
        let (backend, artifacts, _) = self.deps.shared();
        let state = self.deps.state.clone();
        let activity_id = format!("photo:{}", uuid::Uuid::new_v4());
        state.begin_activity(activity_id.clone(), camera_id.clone(), unix_millis());
        let photo =
            match tokio::task::spawn_blocking(move || backend.capture_photo(&camera_id, max_width))
                .await
                .map_err(|error| {
                    ToolError::structured(
                        "camera.capture_photo",
                        "action_failed",
                        error.to_string(),
                    )
                })?
                .map_err(|error| camera_error("camera.capture_photo", error))
            {
                Ok(photo) => photo,
                Err(error) => {
                    state.end_activity(&activity_id);
                    return Err(error);
                }
            };
        // The native snapshot device is closed when the backend call returns;
        // artifact persistence is not camera activity and must not delay the
        // human-facing indicator from clearing.
        state.end_activity(&activity_id);
        let artifact = artifacts
            .put_with_source("image/png", photo.png_bytes, ArtifactSource::Camera, true)
            .await
            .map_err(|error| {
                ToolError::structured("camera.capture_photo", "action_failed", error.to_string())
            });
        let artifact = artifact?;
        let image = ImageArtifactRef::new(artifact.clone(), photo.width, photo.height);
        Ok(ToolOutput::multipart(
            serde_json::json!({
                "camera_id": args.get("camera_id"),
                "artifact_id": artifact.id,
                "width": photo.width,
                "height": photo.height,
            }),
            vec![ContentPart::Image(image)],
        ))
    }
}

#[async_trait::async_trait]
impl Tool for CameraCaptureStartTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.capture_start"),
            description:
                "Start a camera frame session (logical grouping: scopes permission checks, the Camera: ON indicator, and per-session frame cleanup; each frame still opens the device independently). The frontend must show Camera: ON while it runs."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "camera_id": {"type": "string"} },
                "required": ["camera_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::CameraObserve,
            resource: Resource::Camera(camera_arg(args, "camera.capture_start").ok()?),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let camera_id = camera_arg(&args, "camera.capture_start")?;
        require_camera("camera.capture_start", &ctx, &camera_id)?;
        // Validate the device exists before advertising a session.
        let (backend, _, state) = self.deps.shared();
        let camera_id_for_check = camera_id.clone();
        tokio::task::spawn_blocking(move || backend.list_cameras())
            .await
            .map_err(|error| {
                ToolError::structured("camera.capture_start", "action_failed", error.to_string())
            })?
            .map_err(|error| camera_error("camera.capture_start", error))?
            .iter()
            .find(|device| device.id == camera_id_for_check || device.label == camera_id_for_check)
            .ok_or_else(|| {
                camera_error(
                    "camera.capture_start",
                    camera_capture::CameraError::UnknownCamera(camera_id.clone()),
                )
            })?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let started_at_ms = unix_millis();
        state.sessions.lock().await.insert(
            session_id.clone(),
            CameraSession {
                camera_id: camera_id.clone(),
                started_at_ms,
                frame_count: 0,
            },
        );
        state.begin_activity(session_id.clone(), camera_id.clone(), started_at_ms);
        Ok(ToolOutput::json(
            serde_json::json!({ "session_id": session_id, "camera_id": camera_id }),
        ))
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
impl Tool for CameraCaptureFrameTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.capture_frame"),
            description:
                "Capture one frame from a camera session as an expiring sensitive artifact."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "session_id": {"type": "string"},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 8192},
                },
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
        let session_id = session_arg(&args, "camera.capture_frame")?;
        let max_width = max_width_arg(&args, "camera.capture_frame")?;
        let (backend, artifacts, state) = self.deps.shared();
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return Err(ToolError::structured_with_details(
                "camera.capture_frame",
                "session_not_found",
                format!("camera session '{session_id}' not found"),
                serde_json::json!({ "session_id": session_id }),
            ));
        };
        require_camera("camera.capture_frame", &ctx, &session.camera_id)?;
        let camera_id = session.camera_id.clone();
        session.frame_count += 1;
        drop(sessions);
        let photo =
            match tokio::task::spawn_blocking(move || backend.capture_photo(&camera_id, max_width))
                .await
                .map_err(|error| {
                    ToolError::structured(
                        "camera.capture_frame",
                        "action_failed",
                        error.to_string(),
                    )
                })?
                .map_err(|error| camera_error("camera.capture_frame", error))
            {
                Ok(photo) => photo,
                Err(error) => {
                    // A failed backend capture means the logical camera session
                    // is no longer trustworthy. Tear down exactly this session
                    // so the human indicator cannot remain stale.
                    state.sessions.lock().await.remove(&session_id);
                    state.end_activity(&session_id);
                    return Err(error);
                }
            };
        // Frames belong to their session: stopping this session deletes
        // exactly these artifacts, never another session's frames.
        let artifact = match artifacts
            .put_with_owner(
                "image/png",
                photo.png_bytes,
                ArtifactSource::Camera,
                true,
                Some(ArtifactOwner::CameraSession(session_id.clone())),
            )
            .await
        {
            Ok(artifact) => artifact,
            Err(error) => {
                // Artifact persistence failed after the snapshot completed,
                // but the logical camera session is still active. Keep its
                // privacy indicator on until the caller explicitly stops it.
                return Err(ToolError::structured(
                    "camera.capture_frame",
                    "action_failed",
                    error.to_string(),
                ));
            }
        };
        let image = ImageArtifactRef::new(artifact.clone(), photo.width, photo.height);
        Ok(ToolOutput::multipart(
            serde_json::json!({
                "session_id": session_id, "artifact_id": artifact.id,
                "width": photo.width, "height": photo.height,
            }),
            vec![ContentPart::Image(image)],
        ))
    }
}

#[async_trait::async_trait]
impl Tool for CameraCaptureStopTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("camera.capture_stop"),
            description:
                "Stop a camera frame session and clear the Camera: ON indicator when none remain."
                    .to_string(),
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
        let session_id = session_arg(&args, "camera.capture_stop")?;
        let (_, artifacts, state) = self.deps.shared();
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get(&session_id) else {
            return Err(ToolError::structured_with_details(
                "camera.capture_stop",
                "session_not_found",
                format!("camera session '{session_id}' not found"),
                serde_json::json!({ "session_id": session_id }),
            ));
        };
        require_camera("camera.capture_stop", &ctx, &session.camera_id)?;
        let session = sessions
            .remove(&session_id)
            .expect("camera session was checked while holding the map lock");
        drop(sessions);
        // Deterministic exact-session cleanup: this session's frames are
        // deleted; concurrent sessions' frames always survive.
        artifacts
            .delete_owner(&ArtifactOwner::CameraSession(session_id.clone()))
            .await;
        let _ = session;
        state.end_activity(&session_id);
        Ok(ToolOutput::json(
            serde_json::json!({ "ok": true, "session_id": session_id }),
        ))
    }
}

/// Static camera tool group.
pub struct CameraToolPack {
    pub backend: Arc<dyn CameraBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub state: CameraState,
}

impl CameraToolPack {
    pub fn stub() -> Self {
        Self {
            backend: Arc::new(camera_capture::StubBackend),
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
            state: CameraState::new(),
        }
    }

    pub fn with_services(
        backend: Arc<dyn CameraBackend>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            backend,
            artifacts,
            state: CameraState::new(),
        }
    }

    fn deps(&self) -> CameraDeps {
        CameraDeps {
            backend: self.backend.clone(),
            artifacts: self.artifacts.clone(),
            state: self.state.clone(),
        }
    }
}

impl tool_sdk::ToolPack for CameraToolPack {
    fn id(&self) -> &'static str {
        "camera"
    }

    /// `camera.status` and `camera.list` stay visible so the model can
    /// report availability; capture tools are hidden while no camera
    /// backend can capture instead of failing per call.
    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        // One deps bundle per tool, all sharing the backend, store, and
        // visible-capture state.
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(CameraListTool { deps: self.deps() }),
            Arc::new(CameraStatusTool { deps: self.deps() }),
        ];
        if !self.backend.is_available() {
            return tools;
        }
        tools.extend([
            Arc::new(CameraCapturePhotoTool { deps: self.deps() }) as Arc<dyn Tool>,
            Arc::new(CameraCaptureStartTool { deps: self.deps() }),
            Arc::new(CameraCaptureFrameTool { deps: self.deps() }),
            Arc::new(CameraCaptureStopTool { deps: self.deps() }),
        ]);
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use std::time::Duration;
    use tool_sdk::ToolPack as _;

    struct FakeCamera {
        devices: Vec<CameraDevice>,
        fail_capture: bool,
    }

    impl FakeCamera {
        fn new() -> Self {
            Self {
                devices: vec![CameraDevice {
                    id: "cam-0".to_string(),
                    label: "Fake Cam".to_string(),
                    description: "test device".to_string(),
                }],
                fail_capture: false,
            }
        }

        fn failing() -> Self {
            Self {
                fail_capture: true,
                ..Self::new()
            }
        }
    }

    impl CameraBackend for FakeCamera {
        fn list_cameras(&self) -> Result<Vec<CameraDevice>, camera_capture::CameraError> {
            Ok(self.devices.clone())
        }

        fn capture_photo(
            &self,
            camera_id: &str,
            _max_width: Option<u32>,
        ) -> Result<camera_capture::Photo, camera_capture::CameraError> {
            if camera_id != "cam-0" {
                return Err(camera_capture::CameraError::UnknownCamera(
                    camera_id.to_string(),
                ));
            }
            if self.fail_capture {
                return Err(camera_capture::CameraError::CaptureFailed(
                    "simulated camera disconnect".to_string(),
                ));
            }
            // Minimal valid 1x1 PNG.
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(&[255, 0, 0]).unwrap();
            }
            Ok(camera_capture::Photo {
                width: 1,
                height: 1,
                png_bytes: bytes,
            })
        }
    }

    fn pack() -> CameraToolPack {
        pack_with_backend(Arc::new(FakeCamera::new()))
    }

    fn pack_with_backend(backend: Arc<dyn CameraBackend>) -> CameraToolPack {
        CameraToolPack::with_services(
            backend,
            Arc::new(artifact_core::InMemoryArtifactStore::new()),
        )
    }

    fn ctx_for_camera(camera_id: &str) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            Capability::CameraObserve,
            capability_core::ResourceScope::new(vec![Resource::Camera(camera_id.to_string())]),
            ctx.invocation_id,
            Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    fn find(pack: &CameraToolPack, id: &str) -> Arc<dyn Tool> {
        pack.tools(&tool_sdk::ToolLoadContext::default())
            .into_iter()
            .find(|tool| tool.metadata().id.0 == id)
            .unwrap()
    }

    #[tokio::test]
    async fn photo_requires_per_device_authorization() {
        let pack = pack();
        let photo = find(&pack, "camera.capture_photo");
        let err = photo
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({"camera_id": "cam-0"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        let out = photo
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"camera_id": "cam-0"}),
            )
            .await
            .unwrap();
        assert!(matches!(out.parts.first(), Some(ContentPart::Image(_))));
        assert!(!pack.state.activity_state().active);
    }

    #[tokio::test]
    async fn session_lifecycle_drives_the_visible_indicator() {
        let pack = pack();
        assert!(!pack.state.is_active());
        let mut changes = pack.state.subscribe();
        let start = find(&pack, "camera.capture_start");
        let out = start
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"camera_id": "cam-0"}),
            )
            .await
            .unwrap();
        assert!(pack.state.is_active());
        changes.changed().await.unwrap();
        assert_eq!(pack.state.activity_state().session_count, 1);
        let session_id = out.content["session_id"].as_str().unwrap().to_string();

        let status = find(&pack, "camera.status");
        let out = status
            .invoke(ctx_for_camera("cam-0"), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.content["camera_on"], true);

        let frame = find(&pack, "camera.capture_frame");
        frame
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"session_id": session_id}),
            )
            .await
            .unwrap();

        let stop = find(&pack, "camera.capture_stop");
        stop.invoke(
            ctx_for_camera("cam-0"),
            serde_json::json!({"session_id": session_id}),
        )
        .await
        .unwrap();
        assert!(!pack.state.is_active());
        changes.changed().await.unwrap();
        assert_eq!(pack.state.activity_state().session_count, 0);
    }

    #[tokio::test]
    async fn unknown_cameras_are_invalid_targets() {
        let pack = pack();
        let photo = find(&pack, "camera.capture_photo");
        let err = photo
            .invoke(
                ctx_for_camera("ghost"),
                serde_json::json!({"camera_id": "ghost"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("invalid_target"), "{err:?}");
    }

    #[tokio::test]
    async fn stopping_one_session_keeps_other_session_frames() {
        use artifact_core::ArtifactId;
        let pack = pack();
        let start = find(&pack, "camera.capture_start");
        let frame = find(&pack, "camera.capture_frame");
        let stop = find(&pack, "camera.capture_stop");
        let ctx = || ctx_for_camera("cam-0");
        let session = |out: &ToolOutput| out.content["session_id"].as_str().unwrap().to_string();
        let s1 = session(
            &start
                .invoke(ctx(), serde_json::json!({"camera_id": "cam-0"}))
                .await
                .unwrap(),
        );
        let s2 = session(
            &start
                .invoke(ctx(), serde_json::json!({"camera_id": "cam-0"}))
                .await
                .unwrap(),
        );
        let frame_id = |out: &ToolOutput| {
            ArtifactId::new(out.content["artifact_id"].as_str().unwrap().to_string())
        };
        let f1 = frame_id(
            &frame
                .invoke(ctx(), serde_json::json!({"session_id": s1}))
                .await
                .unwrap(),
        );
        let f2 = frame_id(
            &frame
                .invoke(ctx(), serde_json::json!({"session_id": s2}))
                .await
                .unwrap(),
        );
        // Stopping session A deletes exactly A's frame.
        stop.invoke(ctx(), serde_json::json!({"session_id": s1}))
            .await
            .unwrap();
        assert!(
            pack.artifacts.get(&f1).await.is_err(),
            "stopped session frame must be gone"
        );
        assert!(
            pack.artifacts.get(&f2).await.is_ok(),
            "concurrent session frame must survive"
        );
        assert!(pack.state.is_active());
        assert_eq!(pack.state.activity_state().session_count, 1);
        // Stopping B cleans up its own frame and clears the indicator.
        stop.invoke(ctx(), serde_json::json!({"session_id": s2}))
            .await
            .unwrap();
        assert!(pack.artifacts.get(&f2).await.is_err());
        assert!(!pack.state.is_active());
        assert_eq!(pack.state.activity_state().session_count, 0);
    }

    #[tokio::test]
    async fn backend_failure_clears_camera_activity_and_session() {
        let pack = pack_with_backend(Arc::new(FakeCamera::failing()));
        let start = find(&pack, "camera.capture_start");
        let frame = find(&pack, "camera.capture_frame");
        let session_id = start
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"camera_id": "cam-0"}),
            )
            .await
            .unwrap()
            .content["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let error = frame
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"session_id": session_id}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some("action_failed"), "{error:?}");
        assert!(!pack.state.activity_state().active);
        assert_eq!(pack.state.activity_state().session_count, 0);
        assert_eq!(pack.state.session_count().await, 0);
    }

    #[test]
    fn pack_registers_photo_and_session_tools() {
        let pack = pack();
        let mut ids = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "camera.capture_frame",
                "camera.capture_photo",
                "camera.capture_start",
                "camera.capture_stop",
                "camera.list",
                "camera.status",
            ]
        );
    }

    #[test]
    fn stub_backend_advertises_status_and_list_only() {
        let pack = CameraToolPack::stub();
        let mut ids = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["camera.list", "camera.status"]);
    }
}
