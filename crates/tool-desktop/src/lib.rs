//! Desktop / computer-use core (plan Phase 27): the `DesktopBackend`
//! trait plus permissioned `desktop.*` agent tools.
//!
//! Strategy from the plan: accessibility APIs first (native element
//! actions), screenshots as fallback — never default to coordinate
//! clicking. Every tool declares its capability so the agent + policy
//! engine approve each call; tickets are re-validated inside `invoke`.
//!
//! Platform backends live in `desktop-linux`, `desktop-linux-wayland`,
//! `desktop-windows`, and `desktop-macos`. This crate ships the
//! platform-neutral trait, the [`plugin`] model (manifest + capability
//! registry), the tools, and a stub backend that reports unavailability
//! when no native backend is available on the current host.

pub mod plugin;

use artifact_core::{ArtifactStore, ImageArtifactRef};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A visible top-level window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub app: String,
}

/// One node of an accessibility tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ElementNode {
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub bounds: Option<Rect>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub focused: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub child_ids: Vec<String>,
    /// Action names the element supports (`invoke`, `set_value`, …).
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DisplayInfo {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_scale_factor")]
    pub scale_factor: f32,
}

fn default_scale_factor() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaptureTarget {
    Desktop,
    Display(String),
    Window(String),
}

impl CaptureTarget {
    pub fn resource_id(&self) -> String {
        match self {
            Self::Desktop => String::new(),
            Self::Display(id) => format!("display:{id}"),
            Self::Window(id) => id.clone(),
        }
    }

    pub fn resource(&self) -> capability_core::Resource {
        match self {
            Self::Desktop => capability_core::Resource::Window(String::new()),
            Self::Display(id) => capability_core::Resource::Display(id.clone()),
            Self::Window(id) => capability_core::Resource::Window(id.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureQuality {
    Draft,
    Normal,
    High,
}

impl Default for CaptureQuality {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureConfig {
    pub target: CaptureTarget,
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,
    #[serde(default)]
    pub include_cursor: bool,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub quality: CaptureQuality,
}

fn default_max_fps() -> u32 {
    30
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::Desktop,
            max_fps: default_max_fps(),
            include_cursor: false,
            max_width: None,
            quality: CaptureQuality::Normal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CaptureSessionId(pub String);

impl CaptureSessionId {
    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VideoFrame {
    pub frame_id: u64,
    pub timestamp_ms: u64,
    pub width: u32,
    pub height: u32,
    pub image: ImageArtifactRef,
}

#[async_trait::async_trait]
pub trait CaptureSession: Send {
    async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError>;
    async fn stop(&mut self) -> Result<(), DesktopError>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccessibilitySnapshot {
    pub snapshot_id: String,
    pub window_id: String,
    pub generation: u64,
    pub nodes: Vec<ElementNode>,
    #[serde(default)]
    pub removed_node_ids: Vec<String>,
    #[serde(default)]
    pub focused_node_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComputerSessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComputerSession {
    pub id: ComputerSessionId,
    pub capture_target: CaptureTarget,
    pub control_enabled: bool,
    #[serde(default)]
    pub paused: bool,
    pub allowed_applications: Vec<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// A captured screenshot: raw PNG bytes plus dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub png_bytes: Vec<u8>,
}

/// A point in screen coordinates (fallback path only — element actions
/// stay preferred; see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("no desktop backend on this platform/session: {0}")]
    BackendUnavailable(String),
    #[error("window '{0}' no longer exists")]
    StaleWindow(String),
    #[error("unknown window '{0}'")]
    UnknownWindow(String),
    #[error("unknown element '{0}'")]
    UnknownElement(String),
    #[error("action failed: {0}")]
    ActionFailed(String),
    #[error("capture session '{0}' not found")]
    UnknownCaptureSession(String),
}

/// High-level desktop interface (plan Phase 27). Async because real
/// backends cross IPC (portals, UI Automation COM, AX). Implementations
/// must never synthesize success: without OS access they return
/// [`DesktopError::BackendUnavailable`].
#[async_trait::async_trait]
pub trait DesktopBackend: Send + Sync {
    /// False when no OS access exists (stub, missing session). The host
    /// registers only the read-only status/inspect facts for an unavailable
    /// backend; mutating and lower-level actions stay hidden.
    fn is_available(&self) -> bool {
        true
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError>;
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "display enumeration is not available on this backend".to_string(),
        ))
    }
    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError>;
    async fn accessibility_snapshot(
        &self,
        window_id: &str,
        _since: Option<&str>,
    ) -> Result<AccessibilitySnapshot, DesktopError> {
        Ok(AccessibilitySnapshot {
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            window_id: window_id.to_string(),
            generation: 0,
            nodes: self.accessibility_tree(window_id).await?,
            removed_node_ids: Vec::new(),
            focused_node_id: None,
        })
    }
    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError>;
    async fn focus_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.invoke_element(window_id, element_id).await
    }
    async fn set_value(
        &self,
        window_id: &str,
        element_id: &str,
        value: &str,
    ) -> Result<(), DesktopError>;
    async fn select_element(
        &self,
        _window_id: &str,
        _element_id: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "semantic selection is not available on this backend".to_string(),
        ))
    }
    async fn expand_element(
        &self,
        _window_id: &str,
        _element_id: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "semantic expansion is not available on this backend".to_string(),
        ))
    }
    async fn collapse_element(
        &self,
        _window_id: &str,
        _element_id: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "semantic collapse is not available on this backend".to_string(),
        ))
    }
    async fn screenshot(&self, window_id: Option<&str>) -> Result<Screenshot, DesktopError>;
    async fn screenshot_target(&self, target: CaptureTarget) -> Result<Screenshot, DesktopError> {
        match target {
            CaptureTarget::Desktop => self.screenshot(None).await,
            CaptureTarget::Window(window_id) => self.screenshot(Some(&window_id)).await,
            CaptureTarget::Display(display_id) => Err(DesktopError::BackendUnavailable(format!(
                "display capture '{display_id}' is not available on this backend"
            ))),
        }
    }
    /// Capture one still image with the same target/cursor/size options used
    /// by a sampled session. Backends that cannot honor an option should
    /// return an honest error instead of silently widening or changing the
    /// requested target.
    async fn screenshot_with_config(
        &self,
        config: CaptureConfig,
    ) -> Result<Screenshot, DesktopError> {
        self.screenshot_target(config.target).await
    }
    async fn start_capture(
        &self,
        _config: CaptureConfig,
        _artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Box<dyn CaptureSession>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "continuous capture is not available on this backend".to_string(),
        ))
    }
    /// Enable or disable the host's emergency control gate. This is
    /// deliberately independent from screen capture: a backend may keep
    /// observing while rejecting every pointer/keyboard/window action.
    /// Implementations should default to enabled for sessions that are not
    /// participating in the Share Screen flow; the host explicitly disables
    /// it when a share starts and restores it when the share ends.
    async fn set_control_enabled(&self, _enabled: bool) -> Result<(), DesktopError> {
        Ok(())
    }
    async fn focus_window(&self, _window_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window focusing is not available on this backend".to_string(),
        ))
    }
    async fn close_window(&self, _window_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window closing is not available on this backend".to_string(),
        ))
    }
    async fn move_window(&self, _window_id: &str, _at: Point) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window moving is not available on this backend".to_string(),
        ))
    }
    async fn resize_window(
        &self,
        _window_id: &str,
        _width: u32,
        _height: u32,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window resizing is not available on this backend".to_string(),
        ))
    }
    async fn minimize_window(&self, _window_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window minimizing is not available on this backend".to_string(),
        ))
    }
    async fn maximize_window(&self, _window_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window maximizing is not available on this backend".to_string(),
        ))
    }
    async fn restore_window(&self, _window_id: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "window restoring is not available on this backend".to_string(),
        ))
    }
    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError>;
    async fn move_pointer(&self, _at: Point) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "pointer motion is not available on this backend".to_string(),
        ))
    }
    async fn double_click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.click(window_id, at).await?;
        self.click(window_id, at).await
    }
    async fn mouse_down(&self, _button: MouseButton) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "mouse button control is not available on this backend".to_string(),
        ))
    }
    async fn mouse_up(&self, _button: MouseButton) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "mouse button control is not available on this backend".to_string(),
        ))
    }
    async fn drag(
        &self,
        _from: Point,
        _to: Point,
        _button: MouseButton,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "pointer dragging is not available on this backend".to_string(),
        ))
    }
    async fn scroll(
        &self,
        _window_id: Option<&str>,
        _delta_x: i32,
        _delta_y: i32,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "scrolling is not available on this backend".to_string(),
        ))
    }
    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError>;
    async fn key_down(&self, _key: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "keyboard control is not available on this backend".to_string(),
        ))
    }
    async fn key_up(&self, _key: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "keyboard control is not available on this backend".to_string(),
        ))
    }
    async fn press_key(&self, key: &str) -> Result<(), DesktopError> {
        self.key_down(key).await?;
        self.key_up(key).await
    }
    async fn hotkey(&self, keys: &[String]) -> Result<(), DesktopError> {
        for key in keys {
            self.key_down(key).await?;
        }
        for key in keys.iter().rev() {
            self.key_up(key).await?;
        }
        Ok(())
    }
    async fn clipboard_read(&self, _mime_type: &str) -> Result<String, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "clipboard access is not available on this backend".to_string(),
        ))
    }
    async fn clipboard_write(&self, _mime_type: &str, _text: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "clipboard access is not available on this backend".to_string(),
        ))
    }
    async fn launch_application(&self, _application: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "application launch is not available on this backend".to_string(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

struct ManagedCapture {
    capture: Box<dyn CaptureSession>,
    artifacts: Arc<dyn ArtifactStore>,
    last_fingerprint: Option<[u8; 32]>,
    last_observation: Option<Instant>,
    boost_until: Option<Instant>,
    paused: bool,
    session: ComputerSession,
}

type ManagedCaptureHandle = Arc<tokio::sync::Mutex<ManagedCapture>>;

struct AccessibilitySnapshotRecord {
    snapshot_id: String,
    generation: u64,
    nodes: Vec<ElementNode>,
}

/// Produce a small pixel fingerprint for model-facing frame deduplication.
///
/// Native backends all expose PNG artifacts, but a backend or test double may
/// return an opaque byte payload while it is being brought up. Decode valid
/// PNGs into a bounded 32×32 nearest-neighbour sample so equivalent pixels do
/// not depend on PNG compression/filter choices; fall back to an exact byte
/// hash when decoding is unavailable. The decoder has its own allocation and
/// pixel-count bounds so an untrusted artifact cannot turn observation into an
/// unbounded image decode.
fn frame_fingerprint(bytes: &[u8]) -> [u8; 32] {
    scaled_png_fingerprint(bytes).unwrap_or_else(|| Sha256::digest(bytes).into())
}

fn scaled_png_fingerprint(bytes: &[u8]) -> Option<[u8; 32]> {
    const GRID: u32 = 32;
    const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;
    const MAX_PIXELS: u64 = 16 * 1024 * 1024;

    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODE_BYTES,
    });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let source = reader.info();
    let width = source.width;
    let height = source.height;
    if width == 0 || height == 0 || u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS
    {
        return None;
    }
    let output_size = reader.output_buffer_size()?;
    let mut decoded = vec![0; output_size];
    let info = reader.next_frame(&mut decoded).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let channels = info.color_type.samples();
    if !matches!(channels, 1..=4) {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(b"utsuwa-scaled-png-fingerprint-v1");
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    for grid_y in 0..GRID {
        let y = (u64::from(grid_y) * u64::from(height) / u64::from(GRID)) as usize;
        let row_offset = y.checked_mul(info.line_size)?;
        for grid_x in 0..GRID {
            let x = (u64::from(grid_x) * u64::from(width) / u64::from(GRID)) as usize;
            let pixel_offset = row_offset.checked_add(x.checked_mul(channels)?)?;
            let pixel = decoded.get(pixel_offset..pixel_offset.checked_add(channels)?)?;
            let (red, green, blue, alpha) = match info.color_type {
                png::ColorType::Grayscale => (pixel[0], pixel[0], pixel[0], 255),
                png::ColorType::GrayscaleAlpha => (pixel[0], pixel[0], pixel[0], pixel[1]),
                png::ColorType::Rgb => (pixel[0], pixel[1], pixel[2], 255),
                png::ColorType::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
                // EXPAND converts indexed PNGs to RGB/RGBA. Keep this arm
                // defensive in case a future decoder transformation changes.
                png::ColorType::Indexed => return None,
            };
            hasher.update([red, green, blue, alpha]);
        }
    }
    Some(hasher.finalize().into())
}

/// Host-owned capture sessions. Native capture may run at the backend's
/// configured rate, but [`Self::next_observation`] emits at most one frame per
/// second while idle and three sampled frames per second after an action.
/// Exact duplicate frames are discarded before they reach the model.
#[derive(Clone, Default)]
pub struct ComputerSessionManager {
    captures: Arc<tokio::sync::Mutex<HashMap<CaptureSessionId, ManagedCaptureHandle>>>,
    accessibility_snapshots: Arc<tokio::sync::Mutex<HashMap<String, AccessibilitySnapshotRecord>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureObservation {
    pub changed: bool,
    pub throttled: bool,
    pub frame: Option<VideoFrame>,
}

impl ComputerSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start_capture(
        &self,
        backend: Arc<dyn DesktopBackend>,
        mut config: CaptureConfig,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<ComputerSession, DesktopError> {
        if !backend.is_available() {
            return Err(DesktopError::BackendUnavailable(
                "cannot start capture without an available desktop backend".to_string(),
            ));
        }
        config.max_fps = config.max_fps.clamp(1, 60);
        let capture = backend
            .start_capture(config.clone(), artifacts.clone())
            .await?;
        let id = CaptureSessionId::fresh();
        let session = ComputerSession {
            id: ComputerSessionId(id.0.clone()),
            capture_target: config.target.clone(),
            control_enabled: false,
            paused: false,
            allowed_applications: Vec::new(),
            started_at: chrono::Utc::now(),
        };
        self.captures.lock().await.insert(
            id,
            Arc::new(tokio::sync::Mutex::new(ManagedCapture {
                capture,
                artifacts,
                last_fingerprint: None,
                last_observation: None,
                boost_until: None,
                paused: false,
                session: session.clone(),
            })),
        );
        Ok(session)
    }

    pub async fn next_observation(
        &self,
        id: &CaptureSessionId,
    ) -> Result<CaptureObservation, DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        let mut managed = managed.lock().await;
        let now = Instant::now();
        if managed.paused {
            return Ok(CaptureObservation {
                changed: false,
                throttled: true,
                frame: None,
            });
        }
        let min_interval = if managed.boost_until.is_some_and(|until| until > now) {
            Duration::from_millis(333)
        } else {
            Duration::from_secs(1)
        };
        if managed
            .last_observation
            .is_some_and(|last| now.duration_since(last) < min_interval)
        {
            return Ok(CaptureObservation {
                changed: false,
                throttled: true,
                frame: None,
            });
        }

        let frame = managed.capture.next_frame().await?;
        managed.last_observation = Some(now);
        let bytes = managed
            .artifacts
            .get(&frame.image.artifact.id)
            .await
            .map_err(|error| DesktopError::ActionFailed(error.to_string()))?;
        let fingerprint = frame_fingerprint(&bytes);
        if managed.last_fingerprint == Some(fingerprint) {
            // A backend creates one artifact per frame. Release duplicate
            // bytes immediately instead of waiting for the store TTL.
            let _ = managed.artifacts.delete(&frame.image.artifact.id).await;
            return Ok(CaptureObservation {
                changed: false,
                throttled: false,
                frame: None,
            });
        }
        managed.last_fingerprint = Some(fingerprint);
        Ok(CaptureObservation {
            changed: true,
            throttled: false,
            frame: Some(frame),
        })
    }

    pub async fn stop_capture(&self, id: &CaptureSessionId) -> Result<(), DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        let mut managed = managed.lock().await;
        managed.capture.stop().await
    }

    pub async fn session(&self, id: &CaptureSessionId) -> Result<ComputerSession, DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        let session = managed.lock().await.session.clone();
        Ok(session)
    }

    /// Return metadata for the host-owned capture sessions without exposing
    /// frame bytes. This lets the model discover a user-started Share Screen
    /// session before asking for a separately authorized sampled frame.
    pub async fn sessions(&self) -> Vec<ComputerSession> {
        let handles = self
            .captures
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut sessions = Vec::with_capacity(handles.len());
        for handle in handles {
            sessions.push(handle.lock().await.session.clone());
        }
        sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        sessions
    }

    pub async fn set_control_enabled(
        &self,
        id: &CaptureSessionId,
        enabled: bool,
    ) -> Result<(), DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        managed.lock().await.session.control_enabled = enabled;
        Ok(())
    }

    /// Increase observation cadence for the short period after a control
    /// action. This is a hint only; it never causes a frame to be sent by
    /// itself and therefore does not turn capture into a model-visible stream.
    pub async fn notify_action(&self, id: &CaptureSessionId) -> Result<(), DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        let mut managed = managed.lock().await;
        managed.boost_until = Some(Instant::now() + Duration::from_secs(3));
        Ok(())
    }

    /// Boost every active session after a desktop-control action. Actions do
    /// not necessarily carry a capture-session id, and a user may be sharing
    /// a display while an action targets a window, so the host conservatively
    /// boosts all sessions for the short post-action observation window.
    pub async fn notify_all_actions(&self) {
        let handles = self
            .captures
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let until = Instant::now() + Duration::from_secs(3);
        for handle in handles {
            let mut managed = handle.lock().await;
            managed.boost_until = Some(until);
        }
    }

    pub async fn set_paused(
        &self,
        id: &CaptureSessionId,
        paused: bool,
    ) -> Result<(), DesktopError> {
        let managed = self
            .captures
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| DesktopError::UnknownCaptureSession(id.0.clone()))?;
        let mut managed = managed.lock().await;
        managed.paused = paused;
        managed.session.paused = paused;
        Ok(())
    }

    pub async fn len(&self) -> usize {
        self.captures.lock().await.len()
    }

    /// Build a bounded accessibility delta for one window. The native
    /// backends provide the current tree; this host-side tracker keeps only
    /// the latest full tree per window so a caller can ask for changes since
    /// the snapshot id it just received without retaining an unbounded
    /// history. An unknown or stale id intentionally returns a full snapshot.
    pub async fn accessibility_snapshot(
        &self,
        backend: &dyn DesktopBackend,
        window_id: &str,
        since_snapshot_id: Option<&str>,
    ) -> Result<AccessibilitySnapshot, DesktopError> {
        let nodes = backend.accessibility_tree(window_id).await?;
        let focused_node_id = nodes
            .iter()
            .find(|node| node.focused == Some(true))
            .map(|node| node.id.clone());
        let mut snapshots = self.accessibility_snapshots.lock().await;
        let previous = snapshots
            .get(window_id)
            .filter(|previous| since_snapshot_id.is_some_and(|id| id == previous.snapshot_id));
        let (generation, changed_nodes, removed_node_ids) = if let Some(previous) = previous {
            let old_nodes = previous
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node))
                .collect::<HashMap<_, _>>();
            let new_ids = nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            let changed = nodes
                .iter()
                .filter(|node| match old_nodes.get(node.id.as_str()) {
                    Some(old) => *old != *node,
                    None => true,
                })
                .cloned()
                .collect();
            let removed = previous
                .nodes
                .iter()
                .filter(|node| !new_ids.contains(node.id.as_str()))
                .map(|node| node.id.clone())
                .collect();
            (previous.generation.saturating_add(1), changed, removed)
        } else {
            (1, nodes.clone(), Vec::new())
        };
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        snapshots.insert(
            window_id.to_string(),
            AccessibilitySnapshotRecord {
                snapshot_id: snapshot_id.clone(),
                generation,
                nodes,
            },
        );
        Ok(AccessibilitySnapshot {
            snapshot_id,
            window_id: window_id.to_string(),
            generation,
            nodes: changed_nodes,
            removed_node_ids,
            focused_node_id,
        })
    }
}

/// The always-available placeholder backend: honest failure instead
/// of fake control. Hosts inject the real platform backend at startup;
/// `tool-desktop` itself depends on no platform crate so backends can
/// implement this trait without a dependency cycle.
pub fn stub() -> Arc<dyn DesktopBackend> {
    Arc::new(StubBackend)
}

/// Largest typed-text payload per call (Phase 36 thrift).
pub const MAX_TYPE_CHARS: usize = 4_096;
/// Largest screenshot the tools will pass to the model.
pub const MAX_SCREENSHOT_BYTES: usize = 2 << 20;

/// Agent tools over a [`DesktopBackend`]. Every action declares an exact
/// capability + resource so policy approves per call; `invoke`
/// re-validates the ticket (broker pattern). An empty window id means
/// "the whole desktop" for observe tools — it never matches a real
/// window grant, so unfiltered observation always prompts.
pub mod tools {
    use super::{
        CaptureConfig, CaptureSessionId, CaptureTarget, ComputerSessionManager, DesktopBackend,
        DesktopError, MouseButton, Point, MAX_SCREENSHOT_BYTES, MAX_TYPE_CHARS,
    };
    use artifact_core::{ArtifactStore, ContentPart, ImageArtifactRef};
    use capability_core::{Capability, Resource};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tool_core::{
        CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
    };

    fn failed(tool: &str, message: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    fn artifact_failed(tool: &str, error: impl std::fmt::Display) -> ToolError {
        failed(tool, format!("artifact store: {error}"))
    }

    fn denied(tool: &str, reason: impl Into<String>) -> ToolError {
        ToolError::Denied {
            tool: tool.to_string(),
            reason: reason.into(),
        }
    }

    fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
        ToolError::InvalidArgs {
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    fn require_ticket(
        tool: &str,
        ctx: &ToolContext,
        capability: Capability,
        resource: Resource,
    ) -> Result<(), ToolError> {
        if ctx.has_ticket(capability, resource) {
            Ok(())
        } else {
            Err(denied(
                tool,
                "no capability ticket authorizes this desktop call: route access through the agent + policy engine",
            ))
        }
    }

    fn backend_error(tool: &str, e: DesktopError) -> ToolError {
        match e {
            DesktopError::BackendUnavailable(detail) => failed(tool, detail),
            DesktopError::StaleWindow(window_id) => failed(
                tool,
                serde_json::json!({
                    "error": "stale_window_id",
                    "window_id": window_id,
                    "message": "The window no longer exists.",
                    "next_tool": "desktop.inspect",
                })
                .to_string(),
            ),
            other => failed(tool, other.to_string()),
        }
    }

    fn opt_window(args: &serde_json::Value) -> String {
        args.get("window_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn checked_opt_window(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
        let id = opt_window(args);
        if !id.is_empty() {
            validate_window_id(tool, &id, false)?;
        }
        Ok(id)
    }

    fn req_window(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
        let id = opt_window(args);
        if id.is_empty() {
            return Err(invalid(tool, "missing string 'window_id'"));
        }
        validate_window_id(tool, &id, false)?;
        Ok(id)
    }

    fn screenshot_target(args: &serde_json::Value, tool: &str) -> Result<CaptureTarget, ToolError> {
        let Some(target) = args.get("target") else {
            if args.get("display_id").is_some() {
                let display_id = args
                    .get("display_id")
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        invalid(tool, "display target needs a non-empty 'display_id'")
                    })?;
                return Ok(CaptureTarget::Display(display_id.to_string()));
            }
            let window_id = checked_opt_window(args, tool)?;
            return Ok(if window_id.is_empty() {
                CaptureTarget::Desktop
            } else {
                CaptureTarget::Window(window_id)
            });
        };
        let object = target
            .as_object()
            .ok_or_else(|| invalid(tool, "'target' must be an object"))?;
        match object.get("type").and_then(|value| value.as_str()) {
            None | Some("desktop") => Ok(CaptureTarget::Desktop),
            Some("window") => {
                let id = object
                    .get("window_id")
                    .and_then(|value| value.as_str())
                    .or_else(|| args.get("window_id").and_then(|value| value.as_str()))
                    .unwrap_or("");
                if id.is_empty() {
                    return Err(invalid(tool, "window target needs a string 'window_id'"));
                }
                validate_window_id(tool, id, false)?;
                Ok(CaptureTarget::Window(id.to_string()))
            }
            Some("display") => {
                let id = object
                    .get("display_id")
                    .and_then(|value| value.as_str())
                    .or_else(|| args.get("display_id").and_then(|value| value.as_str()))
                    .ok_or_else(|| invalid(tool, "display target needs a string 'display_id'"))?;
                if id.is_empty() {
                    return Err(invalid(
                        tool,
                        "display target needs a non-empty 'display_id'",
                    ));
                }
                Ok(CaptureTarget::Display(id.to_string()))
            }
            Some(other) => Err(invalid(
                tool,
                format!("unknown screenshot target type '{other}'"),
            )),
        }
    }

    macro_rules! simple_tool {
        ($name:ident, $id:literal, $desc:literal, $effect:expr) => {
            pub struct $name {
                pub backend: Arc<dyn DesktopBackend>,
            }
            impl $name {
                const TOOL: &'static str = $id;
            }
        };
    }

    simple_tool!(
        ListWindowsTool,
        "desktop.list_windows",
        "List visible top-level windows (id, title, app). Filter by app when possible: unfiltered listing always asks approval.",
        ToolEffect::ReadOnly
    );

    #[async_trait::async_trait]
    impl Tool for ListWindowsTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "List visible top-level windows (id, title, app). Filter by app when possible: unfiltered listing always asks approval.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "app": { "type": "string" } },
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Application(app.to_string()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopObserve,
                Resource::Application(app.to_string()),
            )?;
            let mut windows = self
                .backend
                .list_windows()
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            if !app.is_empty() {
                windows.retain(|w| w.app == app);
            }
            Ok(ToolOutput::new(serde_json::json!({ "windows": windows })))
        }
    }

    /// Read one exact native window's tree. On the Linux X11 backend the
    /// native id policy is hexadecimal; other backends may use their own
    /// native identifier format.
    pub struct AccessibilityTreeTool {
        pub backend: Arc<dyn DesktopBackend>,
    }

    fn invalid_window_id(tool: &str, received: &str) -> ToolError {
        invalid(
            tool,
            serde_json::json!({
                "error": "invalid_window_id",
                "received": received,
                "expected": "exact id returned by desktop.inspect or desktop.list_windows",
                "next_tool": "desktop.inspect",
            })
            .to_string(),
        )
    }

    fn validate_window_id(
        tool: &str,
        id: &str,
        require_hex_window_id: bool,
    ) -> Result<(), ToolError> {
        if id.eq_ignore_ascii_case("desktop") || id.eq_ignore_ascii_case("escritorio") {
            return Err(invalid_window_id(tool, id));
        }
        if require_hex_window_id {
            let digits = id.strip_prefix("0x").or_else(|| id.strip_prefix("0X"));
            if digits.is_none()
                || digits.is_some_and(|digits| {
                    digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_hexdigit())
                })
            {
                return Err(invalid_window_id(tool, id));
            }
        }
        Ok(())
    }

    fn accessibility_tree_metadata() -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("desktop.accessibility_tree"),
            description: "Read one window's native hierarchy (element ids, roles, names, actions). 'window_id' must be the exact native window id returned by desktop.list_windows (or desktop.inspect). On the Linux X11 backend it is a hexadecimal id such as 0x3400012; do not use a window title, app name, 'Desktop', or 'Escritorio'. This Linux backend exposes the X11 window hierarchy, not a semantic AT-SPI accessibility tree. Prefer element actions over coordinates.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "window_id": {
                        "type": "string",
                        "description": "Exact native window id returned by desktop.list_windows (or desktop.inspect). Linux X11 ids look like 0x3400012; never use a title or app name."
                    }
                },
                "required": ["window_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn accessibility_tree_requirement(
        args: &serde_json::Value,
        require_hex_window_id: bool,
    ) -> Option<CapabilityRequirement> {
        req_window(args, "desktop.accessibility_tree")
            .ok()
            .and_then(|id| {
                validate_window_id("desktop.accessibility_tree", &id, require_hex_window_id)
                    .ok()?;
                Some(CapabilityRequirement {
                    capability: Capability::DesktopObserve,
                    resource: Resource::Window(id),
                })
            })
    }

    async fn invoke_accessibility_tree(
        backend: &Arc<dyn DesktopBackend>,
        ctx: ToolContext,
        args: serde_json::Value,
        require_hex_window_id: bool,
    ) -> Result<ToolOutput, ToolError> {
        let window_id = req_window(&args, "desktop.accessibility_tree")?;
        validate_window_id(
            "desktop.accessibility_tree",
            &window_id,
            require_hex_window_id,
        )?;
        require_ticket(
            "desktop.accessibility_tree",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(window_id.clone()),
        )?;
        let tree = backend
            .accessibility_tree(&window_id)
            .await
            .map_err(|e| backend_error("desktop.accessibility_tree", e))?;
        Ok(ToolOutput::new(
            serde_json::json!({ "window_id": window_id, "elements": tree }),
        ))
    }

    #[async_trait::async_trait]
    impl Tool for AccessibilityTreeTool {
        fn metadata(&self) -> ToolMetadata {
            accessibility_tree_metadata()
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            accessibility_tree_requirement(args, false)
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            invoke_accessibility_tree(&self.backend, ctx, args, false).await
        }
    }

    pub struct NativeAccessibilityTreeTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub require_hex_window_id: bool,
    }

    #[async_trait::async_trait]
    impl Tool for NativeAccessibilityTreeTool {
        fn metadata(&self) -> ToolMetadata {
            accessibility_tree_metadata()
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            accessibility_tree_requirement(args, self.require_hex_window_id)
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            invoke_accessibility_tree(&self.backend, ctx, args, self.require_hex_window_id).await
        }
    }

    simple_tool!(
        InvokeElementTool,
        "desktop.invoke_element",
        "Invoke one accessibility element (press a button, toggle a checkbox). Element actions beat coordinate clicking.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for InvokeElementTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Invoke one accessibility element (press a button, toggle a checkbox). Element actions beat coordinate clicking.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "element_id": { "type": "string" },
                    },
                    "required": ["window_id", "element_id"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            req_window(args, Self::TOOL)
                .ok()
                .map(|id| CapabilityRequirement {
                    capability: Capability::DesktopControl,
                    resource: Resource::Window(id),
                })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = req_window(&args, Self::TOOL)?;
            let element_id = args
                .get("element_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'element_id'"))?;
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .invoke_element(&window_id, element_id)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(
                serde_json::json!({ "ok": true, "element_id": element_id }),
            ))
        }
    }

    simple_tool!(
        SetValueTool,
        "desktop.set_value",
        "Set an accessibility element's value (text field contents, slider position).",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for SetValueTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description:
                    "Set an accessibility element's value (text field contents, slider position)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "element_id": { "type": "string" },
                        "value": { "type": "string" },
                    },
                    "required": ["window_id", "element_id", "value"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            req_window(args, Self::TOOL)
                .ok()
                .map(|id| CapabilityRequirement {
                    capability: Capability::DesktopControl,
                    resource: Resource::Window(id),
                })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = req_window(&args, Self::TOOL)?;
            let element_id = args
                .get("element_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'element_id'"))?;
            let value = args
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'value'"))?;
            if value.chars().count() > MAX_TYPE_CHARS {
                return Err(invalid(Self::TOOL, "value is too long"));
            }
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .set_value(&window_id, element_id, value)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }

    pub struct ScreenshotTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub artifacts: Arc<dyn ArtifactStore>,
    }

    impl ScreenshotTool {
        const TOOL: &'static str = "desktop.screenshot";
    }

    #[async_trait::async_trait]
    impl Tool for ScreenshotTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Capture a PNG screenshot (whole desktop or one window). Vision fallback — accessibility actions come first.".to_string(),
                input_schema: serde_json::json!({
                    "additionalProperties": false,
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "type": { "enum": ["desktop", "display", "window"] },
                                "display_id": { "type": "string" },
                                "window_id": { "type": "string" }
                            }
                        },
                        "window_id": { "type": "string", "description": "Legacy shorthand for target.window_id." },
                        "include_cursor": { "type": "boolean" },
                        "max_width": { "type": "integer", "minimum": 1, "maximum": 8192 },
                        "quality": { "enum": ["draft", "normal", "high"] }
                    }
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let target = screenshot_target(args, Self::TOOL).ok()?;
            Some(CapabilityRequirement {
                capability: Capability::ScreenCapture,
                resource: target.resource(),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let config = capture_config(&args, Self::TOOL)?;
            let target = config.target.clone();
            let window_id = match &target {
                CaptureTarget::Window(window_id) => window_id.clone(),
                CaptureTarget::Desktop | CaptureTarget::Display(_) => String::new(),
            };
            let display_id = match &target {
                CaptureTarget::Display(display_id) => serde_json::Value::String(display_id.clone()),
                CaptureTarget::Desktop | CaptureTarget::Window(_) => serde_json::Value::Null,
            };
            let max_width = args.get("max_width").and_then(|value| value.as_u64());
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::ScreenCapture,
                target.resource(),
            )?;
            let shot = self
                .backend
                .screenshot_with_config(config)
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            if shot.png_bytes.len() > MAX_SCREENSHOT_BYTES {
                return Err(failed(Self::TOOL, "screenshot exceeds 2 MiB"));
            }
            if max_width.is_some_and(|width| shot.width > width as u32) {
                return Err(invalid(
                    Self::TOOL,
                    "max_width requested but this backend cannot resize the capture yet",
                ));
            }
            let artifact = self
                .artifacts
                .put("image/png", shot.png_bytes)
                .await
                .map_err(|error| artifact_failed(Self::TOOL, error))?;
            let image = ImageArtifactRef::new(artifact.clone(), shot.width, shot.height);
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            Ok(ToolOutput::multipart(
                serde_json::json!({
                    "capture_id": artifact.id,
                    "width": shot.width,
                    "height": shot.height,
                    "display_id": display_id,
                    "window_id": if window_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(window_id) },
                    "timestamp_ms": timestamp_ms,
                }),
                vec![ContentPart::Image(image)],
            ))
        }
    }

    simple_tool!(
        ClickTool,
        "desktop.click",
        "Click at screen coordinates. Last resort: prefer desktop.invoke_element on an accessibility element.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for ClickTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Click at screen coordinates. Last resort: prefer desktop.invoke_element on an accessibility element.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                    },
                    "required": ["x", "y"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(checked_opt_window(args, Self::TOOL).ok()?),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = checked_opt_window(&args, Self::TOOL)?;
            let x = args
                .get("x")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| invalid(Self::TOOL, "missing integer 'x'"))?;
            let y = args
                .get("y")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| invalid(Self::TOOL, "missing integer 'y'"))?;
            let (x, y) = (
                i32::try_from(x).map_err(|_| invalid(Self::TOOL, "'x' out of range"))?,
                i32::try_from(y).map_err(|_| invalid(Self::TOOL, "'y' out of range"))?,
            );
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .click(
                    if window_id.is_empty() {
                        None
                    } else {
                        Some(&window_id)
                    },
                    Point { x, y },
                )
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }

    simple_tool!(
        TypeTextTool,
        "desktop.type_text",
        "Type text into the focused control (optionally scoped to a window). Prefer desktop.set_value on a named element.",
        ToolEffect::DesktopControl
    );

    #[async_trait::async_trait]
    impl Tool for TypeTextTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(Self::TOOL),
                description: "Type text into the focused control (optionally scoped to a window). Prefer desktop.set_value on a named element.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": { "type": "string" },
                        "text": { "type": "string" },
                    },
                    "required": ["text"],
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(checked_opt_window(args, Self::TOOL).ok()?),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = checked_opt_window(&args, Self::TOOL)?;
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid(Self::TOOL, "missing string 'text'"))?;
            if text.chars().count() > MAX_TYPE_CHARS {
                return Err(invalid(Self::TOOL, "text is too long"));
            }
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            self.backend
                .type_text(
                    if window_id.is_empty() {
                        None
                    } else {
                        Some(&window_id)
                    },
                    text,
                )
                .await
                .map_err(|e| backend_error(Self::TOOL, e))?;
            Ok(ToolOutput::new(serde_json::json!({ "ok": true })))
        }
    }

    pub struct ListDisplaysTool {
        pub backend: Arc<dyn DesktopBackend>,
    }

    #[async_trait::async_trait]
    impl Tool for ListDisplaysTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.list_displays"),
                description: "List displays that can be selected for a screen-sharing session. This does not start capture.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Application(String::new()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            if !args.is_object() {
                return Err(invalid(
                    "desktop.list_displays",
                    "args must be a JSON object",
                ));
            }
            require_ticket(
                "desktop.list_displays",
                &ctx,
                Capability::DesktopObserve,
                Resource::Application(String::new()),
            )?;
            let displays = self
                .backend
                .list_displays()
                .await
                .map_err(|error| backend_error("desktop.list_displays", error))?;
            Ok(ToolOutput::json(
                serde_json::json!({ "displays": displays }),
            ))
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum ElementOperation {
        Focus,
        Select,
        Expand,
        Collapse,
    }

    impl ElementOperation {
        fn tool(self) -> &'static str {
            match self {
                Self::Focus => "desktop.focus_element",
                Self::Select => "desktop.select_element",
                Self::Expand => "desktop.expand_element",
                Self::Collapse => "desktop.collapse_element",
            }
        }

        fn description(self) -> &'static str {
            match self {
                Self::Focus => "Focus a named native accessibility element.",
                Self::Select => "Select a native accessibility element.",
                Self::Expand => "Expand a native accessibility element.",
                Self::Collapse => "Collapse a native accessibility element.",
            }
        }
    }

    pub struct ElementOperationTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub operation: ElementOperation,
    }

    #[async_trait::async_trait]
    impl Tool for ElementOperationTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(self.operation.tool()),
                description: self.operation.description().to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "window_id": { "type": "string" },
                        "element_id": { "type": "string" }
                    },
                    "required": ["window_id", "element_id"]
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            req_window(args, self.operation.tool())
                .ok()
                .map(|window_id| CapabilityRequirement {
                    capability: Capability::DesktopControl,
                    resource: Resource::Window(window_id),
                })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let tool = self.operation.tool();
            let window_id = req_window(&args, tool)?;
            let element_id = args
                .get("element_id")
                .and_then(|value| value.as_str())
                .ok_or_else(|| invalid(tool, "missing string 'element_id'"))?;
            require_ticket(
                tool,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            let result = match self.operation {
                ElementOperation::Focus => self.backend.focus_element(&window_id, element_id).await,
                ElementOperation::Select => {
                    self.backend.select_element(&window_id, element_id).await
                }
                ElementOperation::Expand => {
                    self.backend.expand_element(&window_id, element_id).await
                }
                ElementOperation::Collapse => {
                    self.backend.collapse_element(&window_id, element_id).await
                }
            };
            result.map_err(|error| backend_error(tool, error))?;
            Ok(ToolOutput::json(serde_json::json!({
                "ok": true,
                "window_id": window_id,
                "element_id": element_id,
            })))
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum PointerOperation {
        Move,
        DoubleClick,
        MouseDown,
        MouseUp,
        Drag,
        Scroll,
    }

    impl PointerOperation {
        fn tool(self) -> &'static str {
            match self {
                Self::Move => "desktop.move_pointer",
                Self::DoubleClick => "desktop.double_click",
                Self::MouseDown => "desktop.mouse_down",
                Self::MouseUp => "desktop.mouse_up",
                Self::Drag => "desktop.drag",
                Self::Scroll => "desktop.scroll",
            }
        }

        fn description(self) -> &'static str {
            match self {
                Self::Move => "Move the pointer as a last-resort computer-use action.",
                Self::DoubleClick => "Double-click at coordinates as a last resort.",
                Self::MouseDown => "Press a pointer button at the current location.",
                Self::MouseUp => "Release a pointer button at the current location.",
                Self::Drag => "Drag the pointer between two coordinates as a last resort.",
                Self::Scroll => "Scroll a window by a bounded pixel delta.",
            }
        }
    }

    pub struct PointerTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub operation: PointerOperation,
    }

    fn point_arg(args: &serde_json::Value, tool: &str, prefix: &str) -> Result<Point, ToolError> {
        let x_key = if prefix.is_empty() {
            "x".to_string()
        } else {
            format!("{prefix}x")
        };
        let y_key = if prefix.is_empty() {
            "y".to_string()
        } else {
            format!("{prefix}y")
        };
        let x = args
            .get(&x_key)
            .and_then(|value| value.as_i64())
            .ok_or_else(|| invalid(tool, format!("missing integer '{x_key}'")))?;
        let y = args
            .get(&y_key)
            .and_then(|value| value.as_i64())
            .ok_or_else(|| invalid(tool, format!("missing integer '{y_key}'")))?;
        Ok(Point {
            x: i32::try_from(x).map_err(|_| invalid(tool, format!("'{x_key}' out of range")))?,
            y: i32::try_from(y).map_err(|_| invalid(tool, format!("'{y_key}' out of range")))?,
        })
    }

    fn mouse_button(args: &serde_json::Value, tool: &str) -> Result<MouseButton, ToolError> {
        match args.get("button").and_then(|value| value.as_str()) {
            None | Some("left") => Ok(MouseButton::Left),
            Some("middle") => Ok(MouseButton::Middle),
            Some("right") => Ok(MouseButton::Right),
            Some(other) => Err(invalid(tool, format!("unknown mouse button '{other}'"))),
        }
    }

    #[async_trait::async_trait]
    impl Tool for PointerTool {
        fn metadata(&self) -> ToolMetadata {
            let mut properties = serde_json::Map::new();
            properties.insert(
                "window_id".to_string(),
                serde_json::json!({"type":"string"}),
            );
            match self.operation {
                PointerOperation::Move | PointerOperation::DoubleClick => {
                    properties.insert("x".to_string(), serde_json::json!({"type":"integer"}));
                    properties.insert("y".to_string(), serde_json::json!({"type":"integer"}));
                }
                PointerOperation::MouseDown | PointerOperation::MouseUp => {
                    properties.insert(
                        "button".to_string(),
                        serde_json::json!({"enum":["left","middle","right"]}),
                    );
                }
                PointerOperation::Drag => {
                    for key in ["from_x", "from_y", "to_x", "to_y"] {
                        properties.insert(key.to_string(), serde_json::json!({"type":"integer"}));
                    }
                    properties.insert(
                        "button".to_string(),
                        serde_json::json!({"enum":["left","middle","right"]}),
                    );
                }
                PointerOperation::Scroll => {
                    properties.insert("delta_x".to_string(), serde_json::json!({"type":"integer"}));
                    properties.insert("delta_y".to_string(), serde_json::json!({"type":"integer"}));
                }
            }
            ToolMetadata {
                id: capability_core::ToolId::new(self.operation.tool()),
                description: self.operation.description().to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties":properties
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(checked_opt_window(args, self.operation.tool()).ok()?),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let tool = self.operation.tool();
            let window_id = checked_opt_window(&args, tool)?;
            let resource = Resource::Window(window_id.clone());
            require_ticket(tool, &ctx, Capability::DesktopControl, resource)?;
            let window = if window_id.is_empty() {
                None
            } else {
                Some(window_id.as_str())
            };
            match self.operation {
                PointerOperation::Move => {
                    self.backend.move_pointer(point_arg(&args, tool, "")?).await
                }
                PointerOperation::DoubleClick => {
                    self.backend
                        .double_click(window, point_arg(&args, tool, "")?)
                        .await
                }
                PointerOperation::MouseDown => {
                    self.backend.mouse_down(mouse_button(&args, tool)?).await
                }
                PointerOperation::MouseUp => {
                    self.backend.mouse_up(mouse_button(&args, tool)?).await
                }
                PointerOperation::Drag => {
                    self.backend
                        .drag(
                            point_arg(&args, tool, "from_")?,
                            point_arg(&args, tool, "to_")?,
                            mouse_button(&args, tool)?,
                        )
                        .await
                }
                PointerOperation::Scroll => {
                    let dx = args
                        .get("delta_x")
                        .and_then(|value| value.as_i64())
                        .ok_or_else(|| invalid(tool, "missing integer 'delta_x'"))?;
                    let dy = args
                        .get("delta_y")
                        .and_then(|value| value.as_i64())
                        .ok_or_else(|| invalid(tool, "missing integer 'delta_y'"))?;
                    self.backend
                        .scroll(
                            window,
                            i32::try_from(dx)
                                .map_err(|_| invalid(tool, "'delta_x' out of range"))?,
                            i32::try_from(dy)
                                .map_err(|_| invalid(tool, "'delta_y' out of range"))?,
                        )
                        .await
                }
            }
            .map_err(|error| backend_error(tool, error))?;
            Ok(ToolOutput::json(serde_json::json!({"ok":true})))
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum KeyboardOperation {
        KeyDown,
        KeyUp,
        Hotkey,
        PressKey,
    }

    impl KeyboardOperation {
        fn tool(self) -> &'static str {
            match self {
                Self::KeyDown => "desktop.key_down",
                Self::KeyUp => "desktop.key_up",
                Self::Hotkey => "desktop.hotkey",
                Self::PressKey => "desktop.press_key",
            }
        }
    }

    pub struct KeyboardTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub operation: KeyboardOperation,
    }

    #[async_trait::async_trait]
    impl Tool for KeyboardTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(self.operation.tool()),
                description: "Keyboard input using provider-neutral key names (CTRL, ALT, SHIFT, META, letters, and named keys).".to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties": {
                        "window_id":{"type":"string"},
                        "key":{"type":"string"},
                        "keys":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8}
                    }
                }),
            effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopControl,
                resource: Resource::Window(checked_opt_window(args, self.operation.tool()).ok()?),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let tool = self.operation.tool();
            let window_id = checked_opt_window(&args, tool)?;
            require_ticket(
                tool,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id),
            )?;
            let result = match self.operation {
                KeyboardOperation::KeyDown => {
                    self.backend
                        .key_down(
                            args.get("key")
                                .and_then(|value| value.as_str())
                                .ok_or_else(|| invalid(tool, "missing string 'key'"))?,
                        )
                        .await
                }
                KeyboardOperation::KeyUp => {
                    self.backend
                        .key_up(
                            args.get("key")
                                .and_then(|value| value.as_str())
                                .ok_or_else(|| invalid(tool, "missing string 'key'"))?,
                        )
                        .await
                }
                KeyboardOperation::PressKey => {
                    self.backend
                        .press_key(
                            args.get("key")
                                .and_then(|value| value.as_str())
                                .ok_or_else(|| invalid(tool, "missing string 'key'"))?,
                        )
                        .await
                }
                KeyboardOperation::Hotkey => {
                    let keys = args
                        .get("keys")
                        .and_then(|value| value.as_array())
                        .ok_or_else(|| invalid(tool, "missing array 'keys'"))?;
                    if keys.is_empty() || keys.len() > 8 {
                        return Err(invalid(tool, "'keys' must contain between 1 and 8 keys"));
                    }
                    let keys = keys
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_string)
                                .ok_or_else(|| invalid(tool, "all keys must be strings"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    self.backend.hotkey(&keys).await
                }
            };
            result.map_err(|error| backend_error(tool, error))?;
            Ok(ToolOutput::json(serde_json::json!({"ok":true})))
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum WindowOperation {
        Focus,
        Close,
        Move,
        Resize,
        Minimize,
        Maximize,
        Restore,
    }

    impl WindowOperation {
        fn tool(self) -> &'static str {
            match self {
                Self::Focus => "desktop.focus_window",
                Self::Close => "desktop.close_window",
                Self::Move => "desktop.move_window",
                Self::Resize => "desktop.resize_window",
                Self::Minimize => "desktop.minimize_window",
                Self::Maximize => "desktop.maximize_window",
                Self::Restore => "desktop.restore_window",
            }
        }
    }

    pub struct WindowOperationTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub operation: WindowOperation,
    }

    #[async_trait::async_trait]
    impl Tool for WindowOperationTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new(self.operation.tool()),
                description: "Manage one exact native window id returned by desktop.inspect."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties": {
                        "window_id":{"type":"string"},
                        "x":{"type":"integer"}, "y":{"type":"integer"},
                        "width":{"type":"integer","minimum":1}, "height":{"type":"integer","minimum":1}
                    },
                    "required":["window_id"]
                }),
                effects: vec![ToolEffect::DesktopControl],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            req_window(args, self.operation.tool())
                .ok()
                .map(|window_id| CapabilityRequirement {
                    capability: Capability::DesktopControl,
                    resource: Resource::Window(window_id),
                })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let tool = self.operation.tool();
            let window_id = req_window(&args, tool)?;
            require_ticket(
                tool,
                &ctx,
                Capability::DesktopControl,
                Resource::Window(window_id.clone()),
            )?;
            let result = match self.operation {
                WindowOperation::Focus => self.backend.focus_window(&window_id).await,
                WindowOperation::Close => self.backend.close_window(&window_id).await,
                WindowOperation::Move => {
                    self.backend
                        .move_window(&window_id, point_arg(&args, tool, "")?)
                        .await
                }
                WindowOperation::Resize => {
                    let width = args
                        .get("width")
                        .and_then(|value| value.as_u64())
                        .ok_or_else(|| invalid(tool, "missing unsigned 'width'"))?;
                    let height = args
                        .get("height")
                        .and_then(|value| value.as_u64())
                        .ok_or_else(|| invalid(tool, "missing unsigned 'height'"))?;
                    self.backend
                        .resize_window(
                            &window_id,
                            u32::try_from(width)
                                .map_err(|_| invalid(tool, "'width' out of range"))?,
                            u32::try_from(height)
                                .map_err(|_| invalid(tool, "'height' out of range"))?,
                        )
                        .await
                }
                WindowOperation::Minimize => self.backend.minimize_window(&window_id).await,
                WindowOperation::Maximize => self.backend.maximize_window(&window_id).await,
                WindowOperation::Restore => self.backend.restore_window(&window_id).await,
            };
            result.map_err(|error| backend_error(tool, error))?;
            Ok(ToolOutput::json(
                serde_json::json!({"ok":true,"window_id":window_id}),
            ))
        }
    }

    pub struct ClipboardReadTool {
        pub backend: Arc<dyn DesktopBackend>,
    }

    #[async_trait::async_trait]
    impl Tool for ClipboardReadTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.clipboard_read"),
                description: "Read the text/plain clipboard through the native clipboard broker."
                    .to_string(),
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"mime_type":{"const":"text/plain"}}}),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::ClipboardRead,
                resource: Resource::Application("clipboard".to_string()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let mime_type = clipboard_mime(&args, Self::TOOL)?;
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::ClipboardRead,
                Resource::Application("clipboard".to_string()),
            )?;
            let text = self
                .backend
                .clipboard_read(&mime_type)
                .await
                .map_err(|error| backend_error(Self::TOOL, error))?;
            Ok(ToolOutput::json(
                serde_json::json!({"mime_type":mime_type,"text":text}),
            ))
        }
    }

    impl ClipboardReadTool {
        const TOOL: &'static str = "desktop.clipboard_read";
    }

    pub struct ClipboardWriteTool {
        pub backend: Arc<dyn DesktopBackend>,
    }

    #[async_trait::async_trait]
    impl Tool for ClipboardWriteTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.clipboard_write"),
                description: "Write text/plain to the native clipboard.".to_string(),
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"text":{"type":"string"},"mime_type":{"const":"text/plain"}},"required":["text"]}),
                effects: vec![ToolEffect::ExternalSideEffect],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::ClipboardWrite,
                resource: Resource::Application("clipboard".to_string()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let mime_type = clipboard_mime(&args, "desktop.clipboard_write")?;
            let text = args
                .get("text")
                .and_then(|value| value.as_str())
                .ok_or_else(|| invalid("desktop.clipboard_write", "missing string 'text'"))?;
            if text.chars().count() > MAX_TYPE_CHARS * 4 {
                return Err(invalid(
                    "desktop.clipboard_write",
                    "clipboard text is too long",
                ));
            }
            require_ticket(
                "desktop.clipboard_write",
                &ctx,
                Capability::ClipboardWrite,
                Resource::Application("clipboard".to_string()),
            )?;
            self.backend
                .clipboard_write(&mime_type, text)
                .await
                .map_err(|error| backend_error("desktop.clipboard_write", error))?;
            Ok(ToolOutput::json(
                serde_json::json!({"ok":true,"mime_type":mime_type,"bytes":text.len()}),
            ))
        }
    }

    fn clipboard_mime(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
        let mime_type = args
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or("text/plain");
        if mime_type != "text/plain" {
            return Err(invalid(
                tool,
                "only text/plain clipboard access is supported",
            ));
        }
        Ok(mime_type.to_string())
    }

    pub struct LaunchApplicationTool {
        pub backend: Arc<dyn DesktopBackend>,
    }

    #[async_trait::async_trait]
    impl Tool for LaunchApplicationTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.launch_application"),
                description: "Launch one validated application identity. This API accepts no shell command or argument string.".to_string(),
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"application":{"type":"string","minLength":1}},"required":["application"]}),
                effects: vec![ToolEffect::ExternalSideEffect],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            validated_application(args, "desktop.launch_application")
                .ok()
                .map(|application| CapabilityRequirement {
                    capability: Capability::ApplicationLaunch,
                    resource: Resource::Application(application),
                })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let application = validated_application(&args, "desktop.launch_application")?;
            require_ticket(
                "desktop.launch_application",
                &ctx,
                Capability::ApplicationLaunch,
                Resource::Application(application.clone()),
            )?;
            self.backend
                .launch_application(&application)
                .await
                .map_err(|error| backend_error("desktop.launch_application", error))?;
            Ok(ToolOutput::json(
                serde_json::json!({"ok":true,"application":application}),
            ))
        }
    }

    fn validated_application(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
        let application = args
            .get("application")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid(tool, "missing non-empty string 'application'"))?;
        if application.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, ';' | '|' | '&' | '$' | '>' | '<' | '`')
        }) {
            return Err(invalid(
                tool,
                "application must be one identity, not a shell command or argument string",
            ));
        }
        Ok(application.to_string())
    }

    fn capture_config(args: &serde_json::Value, tool: &str) -> Result<CaptureConfig, ToolError> {
        let target = screenshot_target(args, tool)?;
        let max_fps = args
            .get("max_fps")
            .and_then(|value| value.as_u64())
            .unwrap_or(30);
        if !(1..=60).contains(&max_fps) {
            return Err(invalid(tool, "max_fps must be between 1 and 60"));
        }
        let include_cursor = args
            .get("include_cursor")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let max_width = args
            .get("max_width")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|width| u32::try_from(width).ok())
                    .filter(|width| (1..=8192).contains(width))
                    .ok_or_else(|| invalid(tool, "max_width must be between 1 and 8192"))
            })
            .transpose()?;
        let quality = match args
            .get("quality")
            .and_then(|value| value.as_str())
            .unwrap_or("normal")
        {
            "draft" => super::CaptureQuality::Draft,
            "normal" => super::CaptureQuality::Normal,
            "high" => super::CaptureQuality::High,
            other => return Err(invalid(tool, format!("unknown capture quality '{other}'"))),
        };
        Ok(CaptureConfig {
            target,
            max_fps: max_fps as u32,
            include_cursor,
            max_width,
            quality,
        })
    }

    pub struct CaptureStartTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub artifacts: Arc<dyn ArtifactStore>,
        pub captures: ComputerSessionManager,
    }

    #[async_trait::async_trait]
    impl Tool for CaptureStartTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.capture_start"),
                description: "Start a user-authorized sampled screen/window capture session. Native capture may run faster internally, but model observations are rate-limited and deduplicated.".to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties": {
                        "target":{"type":"object"},
                        "window_id":{"type":"string"},
                        "display_id":{"type":"string"},
                        "max_fps":{"type":"integer","minimum":1,"maximum":60},
                        "include_cursor":{"type":"boolean"},
                        "max_width":{"type":"integer","minimum":1,"maximum":8192},
                        "quality":{"enum":["draft","normal","high"]}
                    }
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let target = capture_config(args, "desktop.capture_start").ok()?.target;
            Some(CapabilityRequirement {
                capability: Capability::ScreenCapture,
                resource: target.resource(),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let config = capture_config(&args, "desktop.capture_start")?;
            let resource = config.target.resource();
            require_ticket(
                "desktop.capture_start",
                &ctx,
                Capability::ScreenCapture,
                resource,
            )?;
            let session = self
                .captures
                .start_capture(self.backend.clone(), config, self.artifacts.clone())
                .await
                .map_err(|error| backend_error("desktop.capture_start", error))?;
            Ok(ToolOutput::json(serde_json::json!({
                "session_id": session.id.0,
                "capture_target": session.capture_target,
                "control_enabled": session.control_enabled,
                "started_at": session.started_at,
            })))
        }
    }

    fn capture_session_id(
        args: &serde_json::Value,
        tool: &str,
    ) -> Result<CaptureSessionId, ToolError> {
        let id = args
            .get("session_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid(tool, "missing non-empty string 'session_id'"))?;
        Ok(CaptureSessionId(id.to_string()))
    }

    pub struct CaptureFrameTool {
        pub captures: ComputerSessionManager,
    }

    #[async_trait::async_trait]
    impl Tool for CaptureFrameTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.capture_frame"),
                description: "Request one sampled frame from a capture session. Identical frames return changed:false and no image part.".to_string(),
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"}},"required":["session_id"]}),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let id = capture_session_id(args, "desktop.capture_frame").ok()?;
            Some(CapabilityRequirement {
                capability: Capability::ScreenCapture,
                resource: Resource::Window(id.0),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let id = capture_session_id(&args, "desktop.capture_frame")?;
            require_ticket(
                "desktop.capture_frame",
                &ctx,
                Capability::ScreenCapture,
                Resource::Window(id.0.clone()),
            )?;
            let observation = self
                .captures
                .next_observation(&id)
                .await
                .map_err(|error| backend_error("desktop.capture_frame", error))?;
            let Some(frame) = observation.frame else {
                return Ok(ToolOutput::json(serde_json::json!({
                    "session_id": id.0,
                    "changed": false,
                    "throttled": observation.throttled,
                })));
            };
            let metadata = serde_json::json!({
                "session_id": id.0,
                "changed": true,
                "throttled": false,
                "frame_id": frame.frame_id,
                "timestamp_ms": frame.timestamp_ms,
                "width": frame.width,
                "height": frame.height,
                "capture_id": frame.image.artifact.id,
            });
            Ok(ToolOutput::multipart(
                metadata,
                vec![ContentPart::Image(frame.image)],
            ))
        }
    }

    pub struct CaptureStopTool {
        pub captures: ComputerSessionManager,
    }

    #[async_trait::async_trait]
    impl Tool for CaptureStopTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.capture_stop"),
                description:
                    "Stop a sampled screen/window capture session and release its native stream."
                        .to_string(),
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"}},"required":["session_id"]}),
                effects: vec![ToolEffect::ExternalSideEffect],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let id = capture_session_id(args, "desktop.capture_stop").ok()?;
            Some(CapabilityRequirement {
                capability: Capability::ScreenCapture,
                resource: Resource::Window(id.0),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let id = capture_session_id(&args, "desktop.capture_stop")?;
            require_ticket(
                "desktop.capture_stop",
                &ctx,
                Capability::ScreenCapture,
                Resource::Window(id.0.clone()),
            )?;
            self.captures
                .stop_capture(&id)
                .await
                .map_err(|error| backend_error("desktop.capture_stop", error))?;
            Ok(ToolOutput::json(
                serde_json::json!({"ok":true,"session_id":id.0}),
            ))
        }
    }

    pub struct CaptureStatusTool {
        pub captures: ComputerSessionManager,
    }

    #[async_trait::async_trait]
    impl Tool for CaptureStatusTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.capture_status"),
                description: "List active capture session metadata without returning frame bytes. Use this to find a user-started Share Screen session before requesting a sampled frame.".to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{}
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Application(String::new()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            if !args.is_object() {
                return Err(invalid(
                    "desktop.capture_status",
                    "args must be a JSON object",
                ));
            }
            require_ticket(
                "desktop.capture_status",
                &ctx,
                Capability::DesktopObserve,
                Resource::Application(String::new()),
            )?;
            Ok(ToolOutput::json(serde_json::json!({
                "sessions": self.captures.sessions().await,
                "sampling": {
                    "idle_max_fps": 1,
                    "after_action_max_fps": 3,
                    "deduplicates_identical_frames": true,
                    "model_receives_frames_on_request": true,
                },
            })))
        }
    }

    pub struct ObserveTool {
        pub backend: Arc<dyn DesktopBackend>,
        pub captures: ComputerSessionManager,
    }

    #[async_trait::async_trait]
    impl Tool for ObserveTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.observe"),
                description: "Observe the current desktop state. Prefer semantic accessibility snapshots; pass an active capture session id when a sampled image is also needed. Screen visibility is separately authorized by desktop.capture_start.".to_string(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties": {
                        "window_id":{"type":"string"},
                        "since_snapshot_id":{"type":"string"},
                        "session_id":{"type":"string"},
                        "include_accessibility":{"type":"boolean"},
                        "include_image":{"type":"boolean"}
                    }
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
            let window_id = args
                .get("window_id")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if !window_id.is_empty() {
                validate_window_id("desktop.observe", window_id, false).ok()?;
            }
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Window(window_id.to_string()),
            })
        }

        fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
            let Some(observe) = self.required_capability(args) else {
                return Vec::new();
            };
            let mut requirements = vec![observe];
            let include_image = args
                .get("include_image")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if include_image {
                if let Some(session_id) = args
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                {
                    requirements.push(CapabilityRequirement {
                        capability: Capability::ScreenCapture,
                        resource: Resource::Window(session_id.to_string()),
                    });
                }
            }
            requirements
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            let window_id = args
                .get("window_id")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            if !window_id.is_empty() {
                validate_window_id("desktop.observe", &window_id, false)?;
            }
            require_ticket(
                "desktop.observe",
                &ctx,
                Capability::DesktopObserve,
                Resource::Window(window_id.clone()),
            )?;
            let include_accessibility = args
                .get("include_accessibility")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            let mut content = serde_json::json!({
                "changed": true,
                "window_id": if window_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(window_id.clone()) },
                "accessibility": serde_json::Value::Null,
            });
            if include_accessibility {
                let since = args
                    .get("since_snapshot_id")
                    .and_then(|value| value.as_str());
                let accessibility = if window_id.is_empty() {
                    serde_json::json!({"windows": self.backend.list_windows().await.map_err(|error| backend_error("desktop.observe", error))?})
                } else {
                    serde_json::to_value(
                        self.captures
                            .accessibility_snapshot(self.backend.as_ref(), &window_id, since)
                            .await
                            .map_err(|error| backend_error("desktop.observe", error))?,
                    )
                    .map_err(|error| failed("desktop.observe", error.to_string()))?
                };
                content["accessibility"] = accessibility;
            }
            let include_image = args
                .get("include_image")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if include_image {
                let Some(session_raw) = args.get("session_id").and_then(|value| value.as_str())
                else {
                    content["image_available"] = serde_json::Value::Bool(false);
                    content["image_reason"] = serde_json::Value::String(
                        "start a separately authorized capture session first".to_string(),
                    );
                    return Ok(ToolOutput::json(content));
                };
                let id = CaptureSessionId(session_raw.to_string());
                require_ticket(
                    "desktop.observe",
                    &ctx,
                    Capability::ScreenCapture,
                    Resource::Window(id.0.clone()),
                )?;
                let observation = self
                    .captures
                    .next_observation(&id)
                    .await
                    .map_err(|error| backend_error("desktop.observe", error))?;
                content["changed"] = serde_json::Value::Bool(observation.changed);
                content["throttled"] = serde_json::Value::Bool(observation.throttled);
                if let Some(frame) = observation.frame {
                    content["frame_id"] = serde_json::json!(frame.frame_id);
                    content["timestamp_ms"] = serde_json::json!(frame.timestamp_ms);
                    content["width"] = serde_json::json!(frame.width);
                    content["height"] = serde_json::json!(frame.height);
                    return Ok(ToolOutput::multipart(
                        content,
                        vec![ContentPart::Image(frame.image)],
                    ));
                }
            }
            Ok(ToolOutput::json(content))
        }
    }

    fn session_name(plugin_id: &str, available: bool) -> &'static str {
        if !available {
            "unavailable"
        } else if plugin_id == "desktop.linux-x11" {
            "x11-or-xwayland"
        } else {
            "native"
        }
    }

    fn accessibility_backend_name(plugin_id: &str, available: bool) -> &'static str {
        if !available {
            "none"
        } else if plugin_id == "desktop.linux-x11" {
            "x11-window-hierarchy"
        } else {
            "native-accessibility"
        }
    }

    fn desktop_status_content(
        plugin: &super::plugin::DesktopPlugin,
        filesystem_desktop: Option<&str>,
    ) -> serde_json::Value {
        let available = plugin.is_available();
        let capabilities: Vec<&str> = plugin
            .manifest
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect();
        serde_json::json!({
            "available": available,
            "backend": plugin.manifest.id,
            "session": session_name(&plugin.manifest.id, available),
            "capabilities": capabilities,
            "filesystem_desktop": filesystem_desktop,
            "gui_desktop_backend": plugin.manifest.id,
            "accessibility_backend": accessibility_backend_name(&plugin.manifest.id, available),
        })
    }

    pub struct DesktopStatusTool {
        pub plugin: super::plugin::DesktopPlugin,
        pub filesystem_desktop: Option<String>,
    }

    #[async_trait::async_trait]
    impl Tool for DesktopStatusTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.status"),
                description: "Report whether the native GUI desktop backend is available and which desktop observations and controls it supports. Use this directly for questions such as whether the assistant can access, see, or control the desktop; do not inspect an arbitrary window for that answer. Read-only and requires no window id. filesystem_desktop is the OS-configured file directory and is not the GUI desktop.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {},
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            // Status contains only backend capability metadata and the
            // already host-resolved filesystem Desktop path. It does not
            // enumerate windows or reveal private desktop content, so it is
            // safe to answer without DesktopObserve approval.
            None
        }

        async fn invoke(
            &self,
            _ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            if !args.is_object() {
                return Err(invalid(Self::TOOL, "args must be a JSON object"));
            }
            Ok(ToolOutput::new(desktop_status_content(
                &self.plugin,
                self.filesystem_desktop.as_deref(),
            )))
        }
    }

    impl DesktopStatusTool {
        const TOOL: &'static str = "desktop.status";
    }

    pub struct DesktopInspectTool {
        pub plugin: super::plugin::DesktopPlugin,
        pub filesystem_desktop: Option<String>,
    }

    #[async_trait::async_trait]
    impl Tool for DesktopInspectTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: capability_core::ToolId::new("desktop.inspect"),
                description: "Inspect the native GUI desktop and return actual visible window ids, titles, and applications. No arguments are required. Use a returned exact id with desktop.accessibility_tree or desktop control tools; never invent an id or use a title such as Desktop.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {},
                }),
                effects: vec![ToolEffect::ReadOnly],
            }
        }

        fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
            Some(CapabilityRequirement {
                capability: Capability::DesktopObserve,
                resource: Resource::Application(String::new()),
            })
        }

        async fn invoke(
            &self,
            ctx: ToolContext,
            args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            if !args.is_object() {
                return Err(invalid(Self::TOOL, "args must be a JSON object"));
            }
            require_ticket(
                Self::TOOL,
                &ctx,
                Capability::DesktopObserve,
                Resource::Application(String::new()),
            )?;
            let available = self.plugin.is_available();
            let windows = if available {
                self.plugin
                    .backend
                    .list_windows()
                    .await
                    .map_err(|e| backend_error(Self::TOOL, e))?
            } else {
                Vec::new()
            };
            let mut content =
                desktop_status_content(&self.plugin, self.filesystem_desktop.as_deref());
            if let Some(object) = content.as_object_mut() {
                object.insert("windows".to_string(), serde_json::json!(windows));
            }
            Ok(ToolOutput::new(content))
        }
    }

    impl DesktopInspectTool {
        const TOOL: &'static str = "desktop.inspect";
    }

    /// Build the agent tools one plugin unlocks. Status and inspect are
    /// host-level read-only tools; low-level actions still follow the
    /// plugin's declared capabilities.
    pub fn for_plugin(plugin: &super::plugin::DesktopPlugin) -> Vec<Arc<dyn Tool>> {
        for_plugin_with_filesystem_desktop(plugin, None)
    }

    pub fn for_plugin_with_filesystem_desktop(
        plugin: &super::plugin::DesktopPlugin,
        filesystem_desktop: Option<String>,
    ) -> Vec<Arc<dyn Tool>> {
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(artifact_core::InMemoryArtifactStore::new());
        for_plugin_with_services(
            plugin,
            filesystem_desktop,
            artifacts,
            ComputerSessionManager::new(),
        )
    }

    pub fn for_plugin_with_services(
        plugin: &super::plugin::DesktopPlugin,
        filesystem_desktop: Option<String>,
        artifacts: Arc<dyn ArtifactStore>,
        captures: ComputerSessionManager,
    ) -> Vec<Arc<dyn Tool>> {
        use super::plugin::DesktopCapability;
        let backend = plugin.backend.clone();
        let mut out: Vec<Arc<dyn Tool>> = vec![
            Arc::new(DesktopStatusTool {
                plugin: plugin.clone(),
                filesystem_desktop: filesystem_desktop.clone(),
            }),
            Arc::new(DesktopInspectTool {
                plugin: plugin.clone(),
                filesystem_desktop,
            }),
        ];
        if !plugin.is_available() {
            return out;
        }
        if plugin.supports(DesktopCapability::ListWindows) {
            out.push(Arc::new(ListWindowsTool {
                backend: backend.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::ListDisplays) {
            out.push(Arc::new(ListDisplaysTool {
                backend: backend.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::Observe) {
            out.push(Arc::new(ObserveTool {
                backend: backend.clone(),
                captures: captures.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::AccessibilityTree) {
            out.push(Arc::new(NativeAccessibilityTreeTool {
                backend: backend.clone(),
                require_hex_window_id: plugin.manifest.id == "desktop.linux-x11",
            }));
        }
        if plugin.supports(DesktopCapability::InvokeElement) {
            out.push(Arc::new(InvokeElementTool {
                backend: backend.clone(),
            }));
        }
        for (capability, operation) in [
            (DesktopCapability::FocusElement, ElementOperation::Focus),
            (DesktopCapability::SelectElement, ElementOperation::Select),
            (DesktopCapability::ExpandElement, ElementOperation::Expand),
            (
                DesktopCapability::CollapseElement,
                ElementOperation::Collapse,
            ),
        ] {
            if plugin.supports(capability) {
                out.push(Arc::new(ElementOperationTool {
                    backend: backend.clone(),
                    operation,
                }));
            }
        }
        if plugin.supports(DesktopCapability::SetValue) {
            out.push(Arc::new(SetValueTool {
                backend: backend.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::Screenshot) {
            out.push(Arc::new(ScreenshotTool {
                backend: backend.clone(),
                artifacts: artifacts.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::CaptureStart) {
            out.push(Arc::new(CaptureStartTool {
                backend: backend.clone(),
                artifacts: artifacts.clone(),
                captures: captures.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::CaptureFrame) {
            out.push(Arc::new(CaptureFrameTool {
                captures: captures.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::CaptureStop) {
            out.push(Arc::new(CaptureStopTool {
                captures: captures.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::CaptureStatus) {
            out.push(Arc::new(CaptureStatusTool {
                captures: captures.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::Click) {
            out.push(Arc::new(ClickTool {
                backend: backend.clone(),
            }));
        }
        for (capability, operation) in [
            (
                DesktopCapability::DoubleClick,
                PointerOperation::DoubleClick,
            ),
            (DesktopCapability::MovePointer, PointerOperation::Move),
            (DesktopCapability::MouseDown, PointerOperation::MouseDown),
            (DesktopCapability::MouseUp, PointerOperation::MouseUp),
            (DesktopCapability::Drag, PointerOperation::Drag),
            (DesktopCapability::Scroll, PointerOperation::Scroll),
        ] {
            if plugin.supports(capability) {
                out.push(Arc::new(PointerTool {
                    backend: backend.clone(),
                    operation,
                }));
            }
        }
        if plugin.supports(DesktopCapability::TypeText) {
            out.push(Arc::new(TypeTextTool {
                backend: backend.clone(),
            }));
        }
        for (capability, operation) in [
            (DesktopCapability::KeyDown, KeyboardOperation::KeyDown),
            (DesktopCapability::KeyUp, KeyboardOperation::KeyUp),
            (DesktopCapability::Hotkey, KeyboardOperation::Hotkey),
            (DesktopCapability::PressKey, KeyboardOperation::PressKey),
        ] {
            if plugin.supports(capability) {
                out.push(Arc::new(KeyboardTool {
                    backend: backend.clone(),
                    operation,
                }));
            }
        }
        for (capability, operation) in [
            (DesktopCapability::FocusWindow, WindowOperation::Focus),
            (DesktopCapability::CloseWindow, WindowOperation::Close),
            (DesktopCapability::MoveWindow, WindowOperation::Move),
            (DesktopCapability::ResizeWindow, WindowOperation::Resize),
            (DesktopCapability::MinimizeWindow, WindowOperation::Minimize),
            (DesktopCapability::MaximizeWindow, WindowOperation::Maximize),
            (DesktopCapability::RestoreWindow, WindowOperation::Restore),
        ] {
            if plugin.supports(capability) {
                out.push(Arc::new(WindowOperationTool {
                    backend: backend.clone(),
                    operation,
                }));
            }
        }
        if plugin.supports(DesktopCapability::ClipboardRead) {
            out.push(Arc::new(ClipboardReadTool {
                backend: backend.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::ClipboardWrite) {
            out.push(Arc::new(ClipboardWriteTool {
                backend: backend.clone(),
            }));
        }
        if plugin.supports(DesktopCapability::LaunchApplication) {
            out.push(Arc::new(LaunchApplicationTool { backend }));
        }
        out
    }
}

/// Static desktop tool group for the active plugin's declared
/// capabilities. Status/inspect stay available as honest read-only facts
/// even with the stub; actions the platform lacks stay invisible.
/// Collection is synchronous; model-facing profile filtering happens
/// centrally in the catalog snapshot.
pub struct DesktopToolPack {
    pub plugin: plugin::DesktopPlugin,
    pub filesystem_desktop: Option<String>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub captures: ComputerSessionManager,
}

impl DesktopToolPack {
    pub fn new(plugin: plugin::DesktopPlugin, filesystem_desktop: Option<String>) -> Self {
        Self {
            plugin,
            filesystem_desktop,
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
            captures: ComputerSessionManager::new(),
        }
    }

    pub fn new_with_services(
        plugin: plugin::DesktopPlugin,
        filesystem_desktop: Option<String>,
        artifacts: Arc<dyn ArtifactStore>,
        captures: ComputerSessionManager,
    ) -> Self {
        Self {
            plugin,
            filesystem_desktop,
            artifacts,
            captures,
        }
    }
}

impl tool_sdk::ToolPack for DesktopToolPack {
    fn id(&self) -> &'static str {
        "desktop"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn tool_core::Tool>> {
        tools::for_plugin_with_services(
            &self.plugin,
            self.filesystem_desktop.clone(),
            self.artifacts.clone(),
            self.captures.clone(),
        )
    }
}

/// Placeholder backend: honest failure instead of fake control.
pub struct StubBackend;

#[async_trait::async_trait]
impl DesktopBackend for StubBackend {
    fn is_available(&self) -> bool {
        false
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn accessibility_tree(&self, _window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn invoke_element(
        &self,
        _window_id: &str,
        _element_id: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn set_value(
        &self,
        _window_id: &str,
        _element_id: &str,
        _value: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn screenshot(&self, _window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn click(&self, _window_id: Option<&str>, _at: Point) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
    async fn type_text(&self, _window_id: Option<&str>, _text: &str) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "no desktop backend is available on this host/session".to_string(),
        ))
    }
}

/// Controllable fake backend for tool tests: scripted windows/elements,
/// captures clicks and typed text, optional screenshot bytes.
#[cfg(test)]
pub struct FakeBackend {
    pub windows: Vec<WindowInfo>,
    pub clicks: std::sync::Mutex<Vec<(Option<String>, Point)>>,
    pub typed: std::sync::Mutex<Vec<(Option<String>, String)>>,
    pub capture_frames: std::sync::Mutex<Vec<Vec<u8>>>,
    pub stale_window: Option<String>,
}

#[cfg(test)]
impl FakeBackend {
    pub(crate) fn new() -> Self {
        Self {
            windows: vec![WindowInfo {
                id: "w1".to_string(),
                title: "Notes".to_string(),
                app: "notes".to_string(),
            }],
            clicks: std::sync::Mutex::new(Vec::new()),
            typed: std::sync::Mutex::new(Vec::new()),
            capture_frames: std::sync::Mutex::new(vec![
                b"frame-one".to_vec(),
                b"frame-one".to_vec(),
                b"frame-two".to_vec(),
            ]),
            stale_window: None,
        }
    }
}

#[cfg(test)]
struct FakeCaptureSession {
    artifacts: Arc<dyn ArtifactStore>,
    frames: Vec<Vec<u8>>,
    next: usize,
    stopped: bool,
}

#[cfg(test)]
#[async_trait::async_trait]
impl CaptureSession for FakeCaptureSession {
    async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError> {
        if self.stopped {
            return Err(DesktopError::BackendUnavailable(
                "fake capture session has been stopped".to_string(),
            ));
        }
        let bytes = self
            .frames
            .get(self.next.min(self.frames.len().saturating_sub(1)))
            .cloned()
            .unwrap_or_else(|| b"empty-frame".to_vec());
        let frame_id = self.next as u64 + 1;
        self.next = self.next.saturating_add(1);
        let artifact = self
            .artifacts
            .put("image/png", bytes)
            .await
            .map_err(|error| DesktopError::ActionFailed(error.to_string()))?;
        Ok(VideoFrame {
            frame_id,
            timestamp_ms: frame_id,
            width: 2,
            height: 2,
            image: ImageArtifactRef::new(artifact, 2, 2),
        })
    }

    async fn stop(&mut self) -> Result<(), DesktopError> {
        self.stopped = true;
        Ok(())
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl DesktopBackend for FakeBackend {
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        Ok(self.windows.clone())
    }
    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        if let Some(stale_window) = &self.stale_window {
            return Err(DesktopError::StaleWindow(stale_window.clone()));
        }
        if window_id != "w1" {
            return Err(DesktopError::UnknownWindow(window_id.to_string()));
        }
        Ok(vec![ElementNode {
            id: "e1".to_string(),
            role: "button".to_string(),
            name: "Save".to_string(),
            description: None,
            value: None,
            bounds: None,
            enabled: Some(true),
            focused: None,
            selected: None,
            checked: None,
            expanded: None,
            parent_id: Some("w1".to_string()),
            child_ids: Vec::new(),
            actions: vec!["invoke".to_string()],
        }])
    }
    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        if window_id != "w1" {
            return Err(DesktopError::UnknownWindow(window_id.to_string()));
        }
        if element_id != "e1" {
            return Err(DesktopError::UnknownElement(element_id.to_string()));
        }
        Ok(())
    }
    async fn set_value(
        &self,
        window_id: &str,
        element_id: &str,
        _value: &str,
    ) -> Result<(), DesktopError> {
        self.invoke_element(window_id, element_id).await
    }
    async fn screenshot(&self, _window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        Ok(Screenshot {
            width: 2,
            height: 2,
            png_bytes: vec![0x89, b'P', b'N', b'G'],
        })
    }
    async fn start_capture(
        &self,
        _config: CaptureConfig,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Box<dyn CaptureSession>, DesktopError> {
        Ok(Box::new(FakeCaptureSession {
            artifacts,
            frames: self.capture_frames.lock().unwrap().clone(),
            next: 0,
            stopped: false,
        }))
    }
    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.clicks
            .lock()
            .unwrap()
            .push((window_id.map(|s| s.to_string()), at));
        Ok(())
    }
    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
        self.typed
            .lock()
            .unwrap()
            .push((window_id.map(|s| s.to_string()), text.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::tools::*;
    use super::FakeBackend;
    use super::{CaptureConfig, CaptureSessionId, ComputerSessionManager};
    use capability_core::{
        Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tool_core::{Tool, ToolContext};

    fn ticket(
        capability: Capability,
        resource: Resource,
        invocation: &InvocationId,
    ) -> CapabilityTicket {
        CapabilityTicket::mint(
            Principal::User,
            capability,
            ResourceScope::new(vec![resource]),
            invocation.clone(),
            Duration::from_secs(120),
        )
    }

    fn ctx_for(capability: Capability, resource: Resource) -> ToolContext {
        let ctx = ToolContext::new(Principal::User);
        let t = ticket(capability, resource, &ctx.invocation_id);
        ctx.with_ticket(t)
    }

    #[tokio::test]
    async fn stub_backend_is_honest() {
        let tool = ListWindowsTool {
            backend: super::stub(),
        };
        let ctx = ctx_for(
            Capability::DesktopObserve,
            Resource::Application("notes".to_string()),
        );
        let err = tool
            .invoke(ctx, serde_json::json!({"app": "notes"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, tool_core::ToolError::Failed { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn calls_without_tickets_are_denied() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let tool = ListWindowsTool { backend };
        let err = tool
            .invoke(ToolContext::new(Principal::User), serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, tool_core::ToolError::Denied { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn list_and_tree_with_fake_backend() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let list = ListWindowsTool {
            backend: backend.clone(),
        };
        // Unfiltered listing declares the whole-desktop resource.
        let req = list.required_capability(&serde_json::json!({})).unwrap();
        assert_eq!(req.capability, Capability::DesktopObserve);
        let out = list
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Application(String::new()),
                ),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["windows"][0]["title"], "Notes");

        let tree = AccessibilityTreeTool {
            backend: backend.clone(),
        };
        let out = tree
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id": "w1"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["elements"][0]["name"], "Save");

        // Unknown windows surface backend errors, never panics.
        let err = tree
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Window("nope".to_string()),
                ),
                serde_json::json!({"window_id": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, tool_core::ToolError::Failed { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn invoke_click_type_roundtrip() {
        let fake = Arc::new(FakeBackend::new());
        let backend: Arc<dyn super::DesktopBackend> = fake.clone();

        let invoke = InvokeElementTool {
            backend: backend.clone(),
        };
        let out = invoke
            .invoke(
                ctx_for(
                    Capability::DesktopControl,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id": "w1", "element_id": "e1"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["ok"], true);

        let click = ClickTool {
            backend: backend.clone(),
        };
        click
            .invoke(
                ctx_for(Capability::DesktopControl, Resource::Window(String::new())),
                serde_json::json!({"x": 10, "y": 20}),
            )
            .await
            .unwrap();
        assert_eq!(
            *fake.clicks.lock().unwrap(),
            vec![(None, super::Point { x: 10, y: 20 })]
        );

        let type_tool = TypeTextTool { backend };
        type_tool
            .invoke(
                ctx_for(
                    Capability::DesktopControl,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id": "w1", "text": "hello"}),
            )
            .await
            .unwrap();
        assert_eq!(
            *fake.typed.lock().unwrap(),
            vec![(Some("w1".to_string()), "hello".to_string())]
        );

        // Oversized text is rejected before touching the backend.
        let err = type_tool
            .invoke(
                ctx_for(
                    Capability::DesktopControl,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id": "w1", "text": "x".repeat(5000)}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, tool_core::ToolError::InvalidArgs { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn for_plugin_exposes_only_declared_capabilities() {
        use super::plugin::{
            DesktopCapability, DesktopPlugin, DesktopPluginManifest, FULL_CAPABILITIES,
        };
        let manifest = |capabilities: Vec<DesktopCapability>| DesktopPluginManifest {
            id: "desktop.test".to_string(),
            name: "test".to_string(),
            version: "0.1.0".to_string(),
            platforms: vec![std::env::consts::OS.to_string()],
            capabilities,
            description: "test".to_string(),
        };
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());

        // Full manifest: two high-level tools plus the declared desktop
        // operations, including capture-session status.
        let full = DesktopPlugin::new(manifest(FULL_CAPABILITIES.to_vec()), backend.clone());
        let ids: Vec<String> = super::tools::for_plugin(&full)
            .iter()
            .map(|t| t.metadata().id.0.clone())
            .collect();
        assert_eq!(ids.len(), 39);
        assert!(ids.contains(&"desktop.status".to_string()));
        assert!(ids.contains(&"desktop.inspect".to_string()));
        assert!(ids.contains(&"desktop.set_value".to_string()));
        assert!(ids.contains(&"desktop.capture_status".to_string()));

        // X11-shaped manifest (no set_value): two high-level tools plus six
        // low-level tools, with no set_value.
        let partial_caps: Vec<DesktopCapability> = FULL_CAPABILITIES
            .iter()
            .copied()
            .filter(|c| *c != DesktopCapability::SetValue)
            .collect();
        let partial = DesktopPlugin::new(manifest(partial_caps), backend);
        let ids: Vec<String> = super::tools::for_plugin(&partial)
            .iter()
            .map(|t| t.metadata().id.0.clone())
            .collect();
        assert_eq!(ids.len(), 38);
        assert!(!ids.contains(&"desktop.set_value".to_string()));

        // The stub still exposes honest status/inspect facts.
        let ids: Vec<String> = super::tools::for_plugin(&DesktopPlugin::stub())
            .iter()
            .map(|t| t.metadata().id.0.clone())
            .collect();
        assert_eq!(ids, vec!["desktop.status", "desktop.inspect"]);
    }

    #[tokio::test]
    async fn status_and_inspect_are_no_argument_native_facts() {
        use super::plugin::{DesktopPlugin, DesktopPluginManifest, FULL_CAPABILITIES};
        let plugin = DesktopPlugin::new(
            DesktopPluginManifest {
                id: "desktop.test".to_string(),
                name: "test".to_string(),
                version: "0.1.0".to_string(),
                platforms: vec![std::env::consts::OS.to_string()],
                capabilities: FULL_CAPABILITIES.to_vec(),
                description: "test".to_string(),
            },
            Arc::new(FakeBackend::new()),
        );
        let status = DesktopStatusTool {
            plugin: plugin.clone(),
            filesystem_desktop: Some("/tmp/home/Escritorio".to_string()),
        };
        assert!(status.required_capability(&serde_json::json!({})).is_none());
        let output = status
            .invoke(ToolContext::new(Principal::User), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(output.content["available"], true);
        assert_eq!(output.content["filesystem_desktop"], "/tmp/home/Escritorio");
        assert_eq!(output.content["gui_desktop_backend"], "desktop.test");

        let inspect = DesktopInspectTool {
            plugin,
            filesystem_desktop: None,
        };
        let output = inspect
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Application(String::new()),
                ),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(output.content["windows"][0]["id"], "w1");
        assert_eq!(output.content["windows"][0]["title"], "Notes");
    }

    #[tokio::test]
    async fn desktop_title_is_not_accepted_as_a_window_id() {
        let tree = AccessibilityTreeTool {
            backend: Arc::new(FakeBackend::new()),
        };
        let error = tree
            .invoke(
                ToolContext::new(Principal::User),
                serde_json::json!({"window_id": "Desktop"}),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("invalid_window_id"), "{message}");
        assert!(message.contains("desktop.inspect"), "{message}");
        assert!(!message.contains("bad window id"), "{message}");

        let native_tree = NativeAccessibilityTreeTool {
            backend: Arc::new(FakeBackend::new()),
            require_hex_window_id: true,
        };
        let native_error = native_tree
            .invoke(
                ToolContext::new(Principal::User),
                serde_json::json!({"window_id": "Desktop"}),
            )
            .await
            .unwrap_err();
        assert!(native_error.to_string().contains("invalid_window_id"));
        assert!(native_error.to_string().contains("desktop.inspect"));
    }

    #[tokio::test]
    async fn stale_window_errors_point_back_to_inspect() {
        let mut fake = FakeBackend::new();
        fake.stale_window = Some("0x123".to_string());
        let tree = AccessibilityTreeTool {
            backend: Arc::new(fake),
        };
        let error = tree
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Window("0x123".to_string()),
                ),
                serde_json::json!({"window_id": "0x123"}),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("stale_window_id"), "{message}");
        assert!(message.contains("0x123"), "{message}");
        assert!(message.contains("desktop.inspect"), "{message}");
        assert!(!message.contains("X11: X error reply"), "{message}");
    }

    #[tokio::test]
    async fn linux_native_window_validation_requires_a_hexadecimal_id() {
        let tree = NativeAccessibilityTreeTool {
            backend: Arc::new(FakeBackend::new()),
            require_hex_window_id: true,
        };
        assert!(tree
            .required_capability(&serde_json::json!({"window_id": "w1"}))
            .is_none());
        let error = tree
            .invoke(
                ToolContext::new(Principal::User),
                serde_json::json!({"window_id": "w1"}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid_window_id"));
    }

    #[tokio::test]
    async fn screenshot_returns_an_artifact_reference() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let shot = ScreenshotTool {
            backend,
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
        };
        let out = shot
            .invoke(
                ctx_for(Capability::ScreenCapture, Resource::Window(String::new())),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["width"], 2);
        assert!(out.content.get("capture_id").is_some());
        assert!(matches!(
            out.parts.first(),
            Some(artifact_core::ContentPart::Image(_))
        ));
    }

    #[tokio::test]
    async fn capture_manager_rate_limits_and_deduplicates_frames() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let memory = Arc::new(artifact_core::InMemoryArtifactStore::new());
        let artifacts: Arc<dyn artifact_core::ArtifactStore> = memory.clone();
        let manager = ComputerSessionManager::new();
        let session = manager
            .start_capture(backend, CaptureConfig::default(), artifacts)
            .await
            .unwrap();
        let id = CaptureSessionId(session.id.0.clone());

        let status = CaptureStatusTool {
            captures: manager.clone(),
        };
        let status_output = status
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Application(String::new()),
                ),
                serde_json::json!({}),
            )
            .await
            .unwrap();
        assert_eq!(status_output.content["sessions"][0]["id"], session.id.0);

        let first = manager.next_observation(&id).await.unwrap();
        assert!(first.changed);
        assert!(first.frame.is_some());
        assert_eq!(memory.len(), 1);

        let throttled = manager.next_observation(&id).await.unwrap();
        assert!(!throttled.changed);
        assert!(throttled.throttled);

        manager.set_paused(&id, true).await.unwrap();
        let paused = manager.next_observation(&id).await.unwrap();
        assert!(!paused.changed);
        assert!(paused.throttled);
        manager.set_paused(&id, false).await.unwrap();

        // Idle observations are intentionally capped at one per second.
        tokio::time::sleep(Duration::from_millis(1_050)).await;
        let duplicate = manager.next_observation(&id).await.unwrap();
        assert!(!duplicate.changed);
        assert!(!duplicate.throttled);
        assert!(duplicate.frame.is_none());
        assert_eq!(memory.len(), 1, "the duplicate artifact is released");

        tokio::time::sleep(Duration::from_millis(1_050)).await;
        let changed = manager.next_observation(&id).await.unwrap();
        assert!(changed.changed);
        assert!(changed.frame.is_some());
        assert_eq!(memory.len(), 2);

        manager.stop_capture(&id).await.unwrap();
        assert_eq!(manager.len().await, 0);
    }

    #[test]
    fn frame_fingerprint_uses_sampled_pixels_not_png_encoding() {
        fn encode(color: png::ColorType, bytes: &[u8]) -> Vec<u8> {
            let mut encoded = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut encoded, 2, 2);
                encoder.set_color(color);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(bytes).unwrap();
            }
            encoded
        }

        let rgb = encode(
            png::ColorType::Rgb,
            &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120],
        );
        let rgba = encode(
            png::ColorType::Rgba,
            &[
                10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
            ],
        );
        assert_ne!(rgb, rgba);
        assert_eq!(
            super::frame_fingerprint(&rgb),
            super::frame_fingerprint(&rgba)
        );
    }

    #[tokio::test]
    async fn action_notification_boosts_sampling_cadence() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let artifacts: Arc<dyn artifact_core::ArtifactStore> =
            Arc::new(artifact_core::InMemoryArtifactStore::new());
        let manager = ComputerSessionManager::new();
        let session = manager
            .start_capture(backend, CaptureConfig::default(), artifacts)
            .await
            .unwrap();
        let id = CaptureSessionId(session.id.0);
        manager.next_observation(&id).await.unwrap();

        manager.notify_all_actions().await;
        tokio::time::sleep(Duration::from_millis(350)).await;
        let boosted = manager.next_observation(&id).await.unwrap();
        assert!(!boosted.throttled);
        manager.stop_capture(&id).await.unwrap();
    }

    #[tokio::test]
    async fn observe_returns_accessibility_deltas_since_the_latest_snapshot() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let manager = ComputerSessionManager::new();
        let tool = ObserveTool {
            backend,
            captures: manager,
        };
        let first = tool
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id":"w1"}),
            )
            .await
            .unwrap();
        let snapshot_id = first.content["accessibility"]["snapshot_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(first.content["accessibility"]["generation"], 1);
        assert_eq!(
            first.content["accessibility"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let second = tool
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Window("w1".to_string()),
                ),
                serde_json::json!({"window_id":"w1","since_snapshot_id":snapshot_id}),
            )
            .await
            .unwrap();
        assert_eq!(second.content["accessibility"]["generation"], 2);
        assert!(second.content["accessibility"]["nodes"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(second.content["accessibility"]["removed_node_ids"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn observe_requires_separate_observe_and_capture_tickets() {
        let backend: Arc<dyn super::DesktopBackend> = Arc::new(FakeBackend::new());
        let artifacts: Arc<dyn artifact_core::ArtifactStore> =
            Arc::new(artifact_core::InMemoryArtifactStore::new());
        let manager = ComputerSessionManager::new();
        let session = manager
            .start_capture(backend.clone(), CaptureConfig::default(), artifacts)
            .await
            .unwrap();
        let session_id = session.id.0.clone();
        let args = serde_json::json!({
            "session_id": session_id,
            "include_accessibility": false,
            "include_image": true
        });
        let tool = ObserveTool {
            backend,
            captures: manager,
        };
        let requirements = tool.required_capabilities(&args);
        assert_eq!(requirements.len(), 2);
        assert_eq!(requirements[0].capability, Capability::DesktopObserve);
        assert_eq!(requirements[1].capability, Capability::ScreenCapture);

        let observe_only = ctx_for(Capability::DesktopObserve, Resource::Window(String::new()));
        let denied = tool.invoke(observe_only, args.clone()).await.unwrap_err();
        assert!(matches!(denied, tool_core::ToolError::Denied { .. }));

        let mut ctx = ToolContext::new(Principal::User);
        let invocation = ctx.invocation_id.clone();
        ctx = ctx.with_ticket(ticket(
            Capability::DesktopObserve,
            Resource::Window(String::new()),
            &invocation,
        ));
        ctx = ctx.with_ticket(ticket(
            Capability::ScreenCapture,
            Resource::Window(session_id),
            &invocation,
        ));
        let output = tool.invoke(ctx, args).await.unwrap();
        assert_eq!(output.content["changed"], true);
        assert!(matches!(
            output.parts.first(),
            Some(artifact_core::ContentPart::Image(_))
        ));
    }
}
