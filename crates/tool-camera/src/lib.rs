//! Permissioned camera tools (`camera.*`).
//!
//! Every photo or frame requires a [`capability_core::Capability::CameraObserve`]
//! ticket for the device — camera access always needs explicit user
//! authorization and is never implied by screen-capture grants.
//! [`CameraState`] tracks whether capture is active so the frontend can
//! render a visible indicator (`Camera: ON`) whenever the camera runs.
//! Frames land in [`artifact_core`] as expiring sensitive artifacts and are
//! never persisted automatically.

use artifact_core::{ArtifactSource, ArtifactStore, ContentPart, ImageArtifactRef};
use camera_capture::{CameraBackend, CameraDevice};
use capability_core::{Capability, Resource};
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// Frontend-visible camera indicator. Cloneable handle shared between the
/// tools and the host state publisher.
#[derive(Clone, Default, Debug)]
pub struct CameraState {
    active: Arc<std::sync::atomic::AtomicBool>,
    sessions: Arc<tokio::sync::Mutex<std::collections::HashMap<String, CameraSession>>>,
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

    /// Number of active streaming sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
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
        let photo =
            tokio::task::spawn_blocking(move || backend.capture_photo(&camera_id, max_width))
                .await
                .map_err(|error| {
                    ToolError::structured(
                        "camera.capture_photo",
                        "action_failed",
                        error.to_string(),
                    )
                })?
                .map_err(|error| camera_error("camera.capture_photo", error))?;
        let artifact = artifacts
            .put_with_source("image/png", photo.png_bytes, ArtifactSource::Camera, true)
            .await
            .map_err(|error| {
                ToolError::structured("camera.capture_photo", "action_failed", error.to_string())
            })?;
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
                "Start a camera frame session. The frontend must show Camera: ON while it runs."
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
        state.sessions.lock().await.insert(
            session_id.clone(),
            CameraSession {
                camera_id: camera_id.clone(),
                started_at_ms: unix_millis(),
                frame_count: 0,
            },
        );
        state
            .active
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
            tokio::task::spawn_blocking(move || backend.capture_photo(&camera_id, max_width))
                .await
                .map_err(|error| {
                    ToolError::structured(
                        "camera.capture_frame",
                        "action_failed",
                        error.to_string(),
                    )
                })?
                .map_err(|error| camera_error("camera.capture_frame", error))?;
        let artifact = artifacts
            .put_with_source("image/png", photo.png_bytes, ArtifactSource::Camera, true)
            .await
            .map_err(|error| {
                ToolError::structured("camera.capture_frame", "action_failed", error.to_string())
            })?;
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
        let Some(session) = sessions.remove(&session_id) else {
            return Err(ToolError::structured_with_details(
                "camera.capture_stop",
                "session_not_found",
                format!("camera session '{session_id}' not found"),
                serde_json::json!({ "session_id": session_id }),
            ));
        };
        require_camera("camera.capture_stop", &ctx, &session.camera_id)?;
        if sessions.is_empty() {
            state
                .active
                .store(false, std::sync::atomic::Ordering::SeqCst);
            // Deterministic cleanup: camera frames never outlive capture.
            artifacts.delete_source(ArtifactSource::Camera).await;
        }
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

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        // One deps bundle per tool, all sharing the backend, store, and
        // visible-capture state.
        vec![
            Arc::new(CameraListTool { deps: self.deps() }),
            Arc::new(CameraStatusTool { deps: self.deps() }),
            Arc::new(CameraCapturePhotoTool { deps: self.deps() }),
            Arc::new(CameraCaptureStartTool { deps: self.deps() }),
            Arc::new(CameraCaptureFrameTool { deps: self.deps() }),
            Arc::new(CameraCaptureStopTool { deps: self.deps() }),
        ]
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
    }

    impl FakeCamera {
        fn new() -> Self {
            Self {
                devices: vec![CameraDevice {
                    id: "cam-0".to_string(),
                    label: "Fake Cam".to_string(),
                    description: "test device".to_string(),
                }],
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
        CameraToolPack::with_services(
            Arc::new(FakeCamera::new()),
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
    }

    #[tokio::test]
    async fn session_lifecycle_drives_the_visible_indicator() {
        let pack = pack();
        assert!(!pack.state.is_active());
        let start = find(&pack, "camera.capture_start");
        let out = start
            .invoke(
                ctx_for_camera("cam-0"),
                serde_json::json!({"camera_id": "cam-0"}),
            )
            .await
            .unwrap();
        assert!(pack.state.is_active());
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
}
