use artifact_core::ArtifactStore;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRelease, CFRetain, CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, KeyCode, ScrollEventUnit,
};
use core_graphics::event_source::CGEventSource;
use core_graphics::event_source::CGEventSourceStateID;
use core_graphics::geometry::CGPoint;
use screencapturekit::prelude::*;
use screencapturekit::screenshot_manager::CGImageExt;
use std::io::Write;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tool_desktop::{
    plugin::{DesktopPlugin, DesktopPluginManifest, FULL_CAPABILITIES},
    CaptureConfig, CaptureSession, CaptureTarget, DesktopBackend, DesktopError, DisplayInfo,
    ElementNode, MouseButton, Point, Rect, Screenshot, VideoFrame, WindowInfo,
};

use accessibility_sys::{
    kAXChildrenAttribute, kAXCloseButtonAttribute, kAXDescriptionAttribute, kAXEnabledAttribute,
    kAXErrorSuccess, kAXExpandedAttribute, kAXFocusedAttribute, kAXFrontmostAttribute,
    kAXMinimizedAttribute, kAXPositionAttribute, kAXPressAction, kAXRaiseAction, kAXRoleAttribute,
    kAXRoleDescriptionAttribute, kAXSelectedAttribute, kAXSizeAttribute, kAXTitleAttribute,
    kAXValueAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowsAttribute,
    AXUIElementCopyActionNames, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementPerformAction, AXUIElementRef, AXUIElementSetAttributeValue, AXValueCreate,
    AXValueGetType, AXValueGetTypeID, AXValueGetValue,
};

const MAX_UI_NODES: usize = 2_048;
const FRAME_QUEUE_CAPACITY: usize = 4;

#[derive(Debug, Clone)]
struct RawFrame {
    width: u32,
    height: u32,
    timestamp_ms: u64,
    rgba: Vec<u8>,
}

struct MacCaptureSession {
    stream: Option<SCStream>,
    receiver: Receiver<RawFrame>,
    artifacts: Arc<dyn ArtifactStore>,
    config: CaptureConfig,
    frame_id: u64,
    stopped: bool,
}

impl MacCaptureSession {
    fn encode_frame(&self, raw: RawFrame) -> Result<(u32, u32, Vec<u8>), DesktopError> {
        let (width, height, rgba) =
            downscale_rgba(raw.width, raw.height, &raw.rgba, self.config.max_width);
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|error| DesktopError::ActionFailed(format!("PNG header: {error}")))?;
            writer
                .write_image_data(&rgba)
                .map_err(|error| DesktopError::ActionFailed(format!("PNG encode: {error}")))?;
        }
        Ok((width, height, bytes))
    }
}

#[async_trait::async_trait]
impl CaptureSession for MacCaptureSession {
    async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError> {
        if self.stopped {
            return Err(DesktopError::BackendUnavailable(
                "ScreenCaptureKit session has stopped".to_string(),
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let first = loop {
            match self.receiver.try_recv() {
                Ok(frame) => break frame,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(DesktopError::ActionFailed(
                            "ScreenCaptureKit produced no frame within 5 seconds".to_string(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(DesktopError::ActionFailed(
                        "ScreenCaptureKit stream ended".to_string(),
                    ));
                }
            }
        };
        let mut raw = first;
        while let Ok(newest) = self.receiver.try_recv() {
            raw = newest;
        }
        let timestamp_ms = raw.timestamp_ms;
        let (width, height, png) = self.encode_frame(raw)?;
        let artifact = self
            .artifacts
            .put("image/png", png)
            .await
            .map_err(|error| DesktopError::ActionFailed(format!("artifact store: {error}")))?;
        self.frame_id = self.frame_id.saturating_add(1);
        Ok(VideoFrame {
            frame_id: self.frame_id,
            timestamp_ms,
            width,
            height,
            image: artifact_core::ImageArtifactRef::new(artifact, width, height),
        })
    }

    async fn stop(&mut self) -> Result<(), DesktopError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(stream) = self.stream.take() {
            stream.stop_capture().map_err(|error| {
                DesktopError::ActionFailed(format!("stop ScreenCaptureKit: {error}"))
            })?;
        }
        Ok(())
    }
}

impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.stop_capture();
        }
    }
}

#[derive(Clone)]
pub struct MacBackend {
    control_enabled: Arc<AtomicBool>,
}

pub fn plugin() -> Option<DesktopPlugin> {
    // Do not use ScreenCaptureKit permission state as plugin discovery. The
    // host must be able to show Share Screen and let the user grant the OS
    // permission before the first capture request.
    Some(DesktopPlugin::new(
        DesktopPluginManifest {
            id: "desktop.macos-ax".to_string(),
            name: "macOS Accessibility and ScreenCaptureKit".to_string(),
            version: "0.1.0".to_string(),
            platforms: vec!["macos".to_string()],
            capabilities: FULL_CAPABILITIES.to_vec(),
            description: "macOS Accessibility (AX) semantic controls, ScreenCaptureKit display/window frames, and CGEvent pointer/keyboard fallback. Desktop capture selects the first display because ScreenCaptureKit has no virtual-desktop filter.".to_string(),
        },
        Arc::new(MacBackend {
            control_enabled: Arc::new(AtomicBool::new(true)),
        }),
    ))
}

impl MacBackend {
    fn ensure_control(&self) -> Result<(), DesktopError> {
        if self.control_enabled.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(
                "desktop control is disabled; enable Allow Control or use the emergency stop to keep it revoked".to_string(),
            ))
        }
    }

    fn content(&self) -> Result<SCShareableContent, DesktopError> {
        SCShareableContent::get()
            .map_err(|error| DesktopError::BackendUnavailable(format!("ScreenCaptureKit: {error}")))
    }

    fn content_target(
        &self,
        target: &CaptureTarget,
    ) -> Result<(SCContentFilter, u32, u32), DesktopError> {
        let content = self.content()?;
        match target {
            CaptureTarget::Desktop => {
                let display = content.displays().into_iter().next().ok_or_else(|| {
                    DesktopError::BackendUnavailable("no macOS display".to_string())
                })?;
                let filter = SCContentFilter::create()
                    .with_display(&display)
                    .with_excluding_windows(&[])
                    .build();
                Ok((filter, display.width().max(1), display.height().max(1)))
            }
            CaptureTarget::Display(id) => {
                let display_id = parse_u32_id(id, "display")?;
                let display = content
                    .displays()
                    .into_iter()
                    .find(|display| display.display_id() == display_id)
                    .ok_or_else(|| DesktopError::UnknownWindow(id.clone()))?;
                let filter = SCContentFilter::create()
                    .with_display(&display)
                    .with_excluding_windows(&[])
                    .build();
                Ok((filter, display.width().max(1), display.height().max(1)))
            }
            CaptureTarget::Window(id) => {
                let window_id = parse_u32_id(id, "window")?;
                let window = content
                    .windows()
                    .into_iter()
                    .find(|window| window.window_id() == window_id)
                    .ok_or_else(|| DesktopError::StaleWindow(id.clone()))?;
                let frame = window.frame();
                let width = frame.size.width.max(1.0) as u32;
                let height = frame.size.height.max(1.0) as u32;
                let filter = SCContentFilter::create().with_window(&window).build();
                Ok((filter, width, height))
            }
        }
    }

    fn start_native_capture(
        &self,
        config: &CaptureConfig,
    ) -> Result<(SCStream, Receiver<RawFrame>), DesktopError> {
        let (filter, source_width, source_height) = self.content_target(&config.target)?;
        let width = config
            .max_width
            .unwrap_or(source_width)
            .min(source_width)
            .max(1);
        let height = ((source_height as u64 * width as u64) / source_width as u64)
            .max(1)
            .min(u32::MAX as u64) as u32;
        let interval = CMTime::new(1, config.max_fps.clamp(1, 60) as i32);
        let stream_config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(config.include_cursor)
            .with_queue_depth(3)
            .with_minimum_frame_interval(&interval);
        let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
        let handler_sender = sender;
        let mut stream = SCStream::new(&filter, &stream_config);
        let registered = stream.add_output_handler(
            move |sample: CMSampleBuffer, output_type: SCStreamOutputType| {
                if output_type != SCStreamOutputType::Screen {
                    return;
                }
                let Ok(image) = sample.cg_image() else {
                    return;
                };
                let Ok(rgba) = image.rgba_data() else {
                    return;
                };
                let frame = RawFrame {
                    width: image.width() as u32,
                    height: image.height() as u32,
                    timestamp_ms: now_ms(),
                    rgba,
                };
                match handler_sender.try_send(frame) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => {}
                }
            },
            SCStreamOutputType::Screen,
        );
        if registered.is_none() {
            return Err(DesktopError::ActionFailed(
                "ScreenCaptureKit rejected the screen output handler".to_string(),
            ));
        }
        stream.start_capture().map_err(|error| {
            DesktopError::BackendUnavailable(format!("start ScreenCaptureKit: {error}"))
        })?;
        Ok((stream, receiver))
    }

    async fn capture_one(&self, config: CaptureConfig) -> Result<Screenshot, DesktopError> {
        let artifacts: Arc<dyn ArtifactStore> =
            Arc::new(artifact_core::InMemoryArtifactStore::new());
        let mut session = self.start_capture(config, artifacts.clone()).await?;
        let frame = session.next_frame().await?;
        let bytes = artifacts
            .get(&frame.image.artifact.id)
            .await
            .map_err(|error| DesktopError::ActionFailed(format!("artifact store: {error}")))?;
        session.stop().await?;
        let _ = artifacts.delete(&frame.image.artifact.id).await;
        Ok(Screenshot {
            width: frame.width,
            height: frame.height,
            png_bytes: bytes,
        })
    }

    fn ax_window_and_app(&self, window_id: &str) -> Result<(AxElement, AxElement), DesktopError> {
        let target_id = parse_u32_id(window_id, "window")?;
        let content = self.content()?;
        let sc_window = content
            .windows()
            .into_iter()
            .find(|window| window.window_id() == target_id)
            .ok_or_else(|| DesktopError::StaleWindow(window_id.to_string()))?;
        let app = sc_window.owning_application().ok_or_else(|| {
            DesktopError::BackendUnavailable("window has no owning application".to_string())
        })?;
        let app_element = AxElement::application(app.process_id())?;
        let title = sc_window.title().unwrap_or_default();
        let windows = app_element
            .children(kAXWindowsAttribute)
            .map_err(|error| DesktopError::ActionFailed(format!("AX windows: {error}")))?;
        let mut windows = windows.into_iter();
        let window = windows
            .find(|candidate| {
                candidate.string(kAXTitleAttribute).as_deref() == Some(title.as_str())
            })
            .or_else(|| windows.next())
            .ok_or_else(|| {
                DesktopError::BackendUnavailable("AX returned no app windows".to_string())
            })?;
        Ok((window, app_element))
    }

    fn ax_root(&self, window_id: &str) -> Result<AxElement, DesktopError> {
        self.ax_window_and_app(window_id).map(|(window, _)| window)
    }

    fn ax_element_by_id(
        &self,
        window_id: &str,
        element_id: &str,
    ) -> Result<AxElement, DesktopError> {
        let prefix = format!("ax:window:{window_id}:root");
        let route = element_id
            .strip_prefix(&prefix)
            .ok_or_else(|| DesktopError::UnknownElement(element_id.to_string()))?;
        let mut current = self.ax_root(window_id)?;
        for component in route.split('/').filter(|part| !part.is_empty()) {
            let index = component
                .parse::<usize>()
                .map_err(|_| DesktopError::UnknownElement(element_id.to_string()))?;
            current = current
                .children(kAXChildrenAttribute)
                .map_err(|error| DesktopError::ActionFailed(format!("AX children: {error}")))?
                .into_iter()
                .nth(index)
                .ok_or_else(|| DesktopError::UnknownElement(element_id.to_string()))?;
        }
        Ok(current)
    }

    fn ax_node(&self, element: &AxElement, id: String, parent_id: Option<String>) -> ElementNode {
        let role = element
            .string(kAXRoleDescriptionAttribute)
            .or_else(|| element.string(kAXRoleAttribute))
            .unwrap_or_else(|| "unknown".to_string());
        let name = element.string(kAXTitleAttribute).unwrap_or_default();
        let description = element
            .string(kAXDescriptionAttribute)
            .or_else(|| element.string("AXHelp"));
        let value = element.string(kAXValueAttribute);
        let bounds = element.bounds();
        let enabled = element.boolean(kAXEnabledAttribute);
        let focused = element.boolean(kAXFocusedAttribute);
        let selected = element.boolean(kAXSelectedAttribute);
        let expanded = element.boolean(kAXExpandedAttribute);
        let actions = element.semantic_actions();
        ElementNode {
            id,
            role,
            name,
            description,
            value,
            bounds,
            enabled,
            focused,
            selected,
            checked: None,
            expanded,
            parent_id,
            child_ids: Vec::new(),
            actions,
        }
    }

    fn collect_ax_tree(
        &self,
        element: AxElement,
        id: String,
        parent_id: Option<String>,
        nodes: &mut Vec<ElementNode>,
    ) {
        if nodes.len() >= MAX_UI_NODES {
            return;
        }
        let node_index = nodes.len();
        nodes.push(self.ax_node(&element, id.clone(), parent_id));
        let children = element.children(kAXChildrenAttribute).unwrap_or_default();
        let mut child_ids = Vec::new();
        for (index, child) in children.into_iter().enumerate() {
            if nodes.len() >= MAX_UI_NODES {
                break;
            }
            let child_id = format!("{id}/{index}");
            child_ids.push(child_id.clone());
            self.collect_ax_tree(child, child_id, Some(id.clone()), nodes);
        }
        nodes[node_index].child_ids = child_ids;
    }

    fn focus_window_sync(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, app) = self.ax_window_and_app(window_id)?;
        app.set_bool(kAXFrontmostAttribute, true)?;
        let result = window.perform(kAXRaiseAction);
        if result == kAXErrorSuccess {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(format!(
                "AX raise failed: {result}"
            )))
        }
    }

    fn click_sync(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if let Some(window_id) = window_id {
            self.focus_window_sync(window_id)?;
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| DesktopError::ActionFailed("cannot create CGEvent source".to_string()))?;
        let location = CGPoint {
            x: at.x as f64,
            y: at.y as f64,
        };
        let down = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::LeftMouseDown,
            location,
            CGMouseButton::Left,
        )
        .map_err(|_| DesktopError::ActionFailed("cannot create mouse-down event".to_string()))?;
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_mouse_event(
            source,
            CGEventType::LeftMouseUp,
            location,
            CGMouseButton::Left,
        )
        .map_err(|_| DesktopError::ActionFailed("cannot create mouse-up event".to_string()))?;
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn pointer_event(
        &self,
        event_type: CGEventType,
        button: CGMouseButton,
        at: Option<Point>,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| DesktopError::ActionFailed("cannot create CGEvent source".to_string()))?;
        let location = at
            .map(|point| CGPoint {
                x: point.x as f64,
                y: point.y as f64,
            })
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        let event = CGEvent::new_mouse_event(source, event_type, location, button)
            .map_err(|_| DesktopError::ActionFailed("cannot create mouse event".to_string()))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn key_event(&self, key: &str, down: bool) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| DesktopError::ActionFailed("cannot create CGEvent source".to_string()))?;
        let event = CGEvent::new_keyboard_event(source, key_code(key)?, down)
            .map_err(|_| DesktopError::ActionFailed("cannot create keyboard event".to_string()))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

#[async_trait::async_trait]
impl DesktopBackend for MacBackend {
    fn is_available(&self) -> bool {
        true
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        let content = self.content()?;
        Ok(content
            .windows()
            .into_iter()
            .filter(|window| window.is_on_screen())
            .filter_map(|window| {
                let title = window.title().filter(|title| !title.is_empty())?;
                let app = window
                    .owning_application()
                    .map(|application| application.application_name())
                    .unwrap_or_default();
                Some(WindowInfo {
                    id: format!("window:{}", window.window_id()),
                    title,
                    app,
                })
            })
            .collect())
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, DesktopError> {
        let content = self.content()?;
        Ok(content
            .displays()
            .into_iter()
            .map(|display| DisplayInfo {
                id: display.display_id().to_string(),
                name: format!("Display {}", display.display_id()),
                width: display.width(),
                height: display.height(),
                scale_factor: 1.0,
            })
            .collect())
    }

    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        let root = self.ax_root(window_id)?;
        let mut nodes = Vec::new();
        self.collect_ax_tree(
            root,
            format!("ax:window:{window_id}:root"),
            None,
            &mut nodes,
        );
        Ok(nodes)
    }

    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let element = self.ax_element_by_id(window_id, element_id)?;
        let result = element.perform(kAXPressAction);
        if result == kAXErrorSuccess {
            return Ok(());
        }
        let bounds = element.bounds().ok_or_else(|| {
            DesktopError::BackendUnavailable("element has no AXPress action or bounds".to_string())
        })?;
        self.click_sync(
            Some(window_id),
            Point {
                x: bounds.x + bounds.width / 2,
                y: bounds.y + bounds.height / 2,
            },
        )
    }

    async fn focus_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let element = self.ax_element_by_id(window_id, element_id)?;
        element.set_bool(kAXFocusedAttribute, true)
    }

    async fn set_value(
        &self,
        window_id: &str,
        element_id: &str,
        value: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let element = self.ax_element_by_id(window_id, element_id)?;
        element.set_string(kAXValueAttribute, value)
    }

    async fn select_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        self.ax_element_by_id(window_id, element_id)?
            .set_bool(kAXSelectedAttribute, true)
    }

    async fn expand_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        self.ax_element_by_id(window_id, element_id)?
            .set_bool(kAXExpandedAttribute, true)
    }

    async fn collapse_element(
        &self,
        window_id: &str,
        element_id: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        self.ax_element_by_id(window_id, element_id)?
            .set_bool(kAXExpandedAttribute, false)
    }

    async fn screenshot(&self, window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        self.capture_one(CaptureConfig {
            target: window_id
                .map(|id| CaptureTarget::Window(id.to_string()))
                .unwrap_or(CaptureTarget::Desktop),
            ..CaptureConfig::default()
        })
        .await
    }

    async fn screenshot_target(&self, target: CaptureTarget) -> Result<Screenshot, DesktopError> {
        self.capture_one(CaptureConfig {
            target,
            ..CaptureConfig::default()
        })
        .await
    }

    async fn screenshot_with_config(
        &self,
        config: CaptureConfig,
    ) -> Result<Screenshot, DesktopError> {
        self.capture_one(config).await
    }

    async fn start_capture(
        &self,
        config: CaptureConfig,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Box<dyn CaptureSession>, DesktopError> {
        let (stream, receiver) = self.start_native_capture(&config)?;
        Ok(Box::new(MacCaptureSession {
            stream: Some(stream),
            receiver,
            artifacts,
            config,
            frame_id: 0,
            stopped: false,
        }))
    }

    async fn set_control_enabled(&self, enabled: bool) -> Result<(), DesktopError> {
        self.control_enabled.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    async fn focus_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.focus_window_sync(window_id)
    }

    async fn close_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        if let Some(close_button) = window.element(kAXCloseButtonAttribute) {
            let result = close_button.perform(kAXPressAction);
            if result == kAXErrorSuccess {
                return Ok(());
            }
        }
        Err(DesktopError::BackendUnavailable(
            "AX close button is not available for this window".to_string(),
        ))
    }

    async fn move_window(&self, window_id: &str, at: Point) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        window.set_point(kAXPositionAttribute, at)
    }

    async fn resize_window(
        &self,
        window_id: &str,
        width: u32,
        height: u32,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        window.set_size(kAXSizeAttribute, width, height)
    }

    async fn minimize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        window.set_bool(kAXMinimizedAttribute, true)
    }

    async fn maximize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        window
            .element("AXZoomButton")
            .ok_or_else(|| {
                DesktopError::BackendUnavailable("AX zoom button is unavailable".to_string())
            })?
            .perform_checked(kAXPressAction)
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let (window, _) = self.ax_window_and_app(window_id)?;
        window.set_bool(kAXMinimizedAttribute, false)
    }

    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.click_sync(window_id, at)
    }

    async fn move_pointer(&self, at: Point) -> Result<(), DesktopError> {
        self.pointer_event(CGEventType::MouseMoved, CGMouseButton::Left, Some(at))
    }

    async fn double_click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.click_sync(window_id, at)?;
        self.click_sync(None, at)
    }

    async fn mouse_down(&self, button: MouseButton) -> Result<(), DesktopError> {
        self.pointer_event(mouse_type(button, true), cg_button(button), None)
    }

    async fn mouse_up(&self, button: MouseButton) -> Result<(), DesktopError> {
        self.pointer_event(mouse_type(button, false), cg_button(button), None)
    }

    async fn drag(&self, from: Point, to: Point, button: MouseButton) -> Result<(), DesktopError> {
        self.move_pointer(from).await?;
        self.mouse_down(button).await?;
        self.move_pointer(to).await?;
        self.mouse_up(button).await
    }

    async fn scroll(
        &self,
        window_id: Option<&str>,
        delta_x: i32,
        delta_y: i32,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if let Some(window_id) = window_id {
            self.focus_window_sync(window_id)?;
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| DesktopError::ActionFailed("cannot create CGEvent source".to_string()))?;
        let event =
            CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, delta_y, delta_x, 0)
                .map_err(|_| {
                    DesktopError::ActionFailed("cannot create scroll event".to_string())
                })?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if let Some(window_id) = window_id {
            self.focus_window_sync(window_id)?;
        }
        if text.is_empty() {
            return Ok(());
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| DesktopError::ActionFailed("cannot create CGEvent source".to_string()))?;
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true).map_err(|_| {
            DesktopError::ActionFailed("cannot create text key-down event".to_string())
        })?;
        down.set_string(text);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source, 0, false).map_err(|_| {
            DesktopError::ActionFailed("cannot create text key-up event".to_string())
        })?;
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    async fn key_down(&self, key: &str) -> Result<(), DesktopError> {
        self.key_event(key, true)
    }

    async fn key_up(&self, key: &str) -> Result<(), DesktopError> {
        self.key_event(key, false)
    }

    async fn clipboard_read(&self, mime_type: &str) -> Result<String, DesktopError> {
        if mime_type != "text/plain" {
            return Err(DesktopError::BackendUnavailable(
                "macOS backend currently exposes only text/plain clipboard data".to_string(),
            ));
        }
        let output = Command::new("pbpaste")
            .output()
            .map_err(|error| DesktopError::ActionFailed(format!("pbpaste: {error}")))?;
        if !output.status.success() {
            return Err(DesktopError::ActionFailed("pbpaste failed".to_string()));
        }
        String::from_utf8(output.stdout).map_err(|error| {
            DesktopError::ActionFailed(format!("clipboard is not UTF-8 text: {error}"))
        })
    }

    async fn clipboard_write(&self, mime_type: &str, text: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if mime_type != "text/plain" {
            return Err(DesktopError::BackendUnavailable(
                "macOS backend currently exposes only text/plain clipboard data".to_string(),
            ));
        }
        let mut child = Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| DesktopError::ActionFailed(format!("pbcopy: {error}")))?;
        child
            .stdin
            .take()
            .ok_or_else(|| DesktopError::ActionFailed("pbcopy stdin unavailable".to_string()))?
            .write_all(text.as_bytes())
            .map_err(|error| DesktopError::ActionFailed(format!("pbcopy write: {error}")))?;
        let status = child
            .wait()
            .map_err(|error| DesktopError::ActionFailed(format!("pbcopy wait: {error}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed("pbcopy failed".to_string()))
        }
    }

    async fn launch_application(&self, application: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        validate_application(application)?;
        Command::new("open")
            .arg("-a")
            .arg(application)
            .spawn()
            .map(|_| ())
            .map_err(|error| DesktopError::ActionFailed(format!("launch '{application}': {error}")))
    }
}

#[derive(Clone)]
struct AxElement(AXUIElementRef);

unsafe impl Send for AxElement {}
unsafe impl Sync for AxElement {}

impl AxElement {
    fn application(pid: i32) -> Result<Self, DesktopError> {
        let element = unsafe { AXUIElementCreateApplication(pid) };
        if element.is_null() {
            Err(DesktopError::BackendUnavailable(format!(
                "AX application element unavailable for pid {pid}"
            )))
        } else {
            Ok(Self(element))
        }
    }

    fn attr(&self, name: &str) -> Option<CFType> {
        let name = CFString::new(name);
        let mut value: CFTypeRef = std::ptr::null();
        let result = unsafe {
            AXUIElementCopyAttributeValue(self.0, name.as_concrete_TypeRef(), &mut value)
        };
        if result != kAXErrorSuccess || value.is_null() {
            None
        } else {
            Some(unsafe { CFType::wrap_under_create_rule(value) })
        }
    }

    fn element(&self, name: &str) -> Option<Self> {
        let value = self.attr(name)?;
        let raw = value.as_CFTypeRef() as AXUIElementRef;
        if raw.is_null() {
            return None;
        }
        unsafe { CFRetain(raw as *const std::ffi::c_void) };
        Some(Self(raw))
    }

    fn string(&self, name: &str) -> Option<String> {
        self.attr(name)?
            .downcast::<CFString>()
            .map(|value| value.to_string())
    }

    fn boolean(&self, name: &str) -> Option<bool> {
        self.attr(name)?.downcast::<CFBoolean>().map(bool::from)
    }

    fn children(&self, name: &str) -> Result<Vec<Self>, DesktopError> {
        let Some(value) = self.attr(name) else {
            return Ok(Vec::new());
        };
        let Some(array) = value.downcast::<CFArray<*const std::ffi::c_void>>() else {
            return Err(DesktopError::ActionFailed(format!(
                "AX attribute {name} is not an array"
            )));
        };
        let mut result = Vec::new();
        for pointer in array.get_all_values() {
            if pointer.is_null() {
                continue;
            }
            let raw = pointer as AXUIElementRef;
            unsafe { CFRetain(raw as *const std::ffi::c_void) };
            result.push(Self(raw));
        }
        Ok(result)
    }

    fn bounds(&self) -> Option<Rect> {
        let position = self.attr(kAXPositionAttribute)?;
        let size = self.attr(kAXSizeAttribute)?;
        let position = ax_point(&position, kAXValueTypeCGPoint)?;
        let size = ax_size(&size, kAXValueTypeCGSize)?;
        Some(Rect {
            x: position.0.round() as i32,
            y: position.1.round() as i32,
            width: size.0.max(0.0).round() as i32,
            height: size.1.max(0.0).round() as i32,
        })
    }

    fn semantic_actions(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut names: CFArrayRef = std::ptr::null();
        let status = unsafe { AXUIElementCopyActionNames(self.0, &mut names) };
        if status == kAXErrorSuccess && !names.is_null() {
            let names: CFArray<*const std::ffi::c_void> =
                unsafe { CFArray::wrap_under_create_rule(names) };
            for pointer in names.get_all_values() {
                if pointer.is_null() {
                    continue;
                }
                let name = unsafe {
                    CFString::wrap_under_get_rule(pointer as core_foundation::string::CFStringRef)
                }
                .to_string();
                if name == kAXPressAction {
                    result.push("invoke".to_string());
                } else if name == kAXRaiseAction {
                    result.push("focus".to_string());
                }
            }
        }
        if self.is_settable(kAXValueAttribute) {
            result.push("set_value".to_string());
        }
        if self.is_settable(kAXSelectedAttribute) {
            result.push("select".to_string());
        }
        if self.is_settable(kAXExpandedAttribute) {
            result.extend(["expand".to_string(), "collapse".to_string()]);
        }
        result.sort();
        result.dedup();
        result
    }

    fn is_settable(&self, name: &str) -> bool {
        let name = CFString::new(name);
        let mut settable = 0u8;
        unsafe {
            accessibility_sys::AXUIElementIsAttributeSettable(
                self.0,
                name.as_concrete_TypeRef(),
                &mut settable,
            ) == kAXErrorSuccess
                && settable != 0
        }
    }

    fn perform(&self, name: &str) -> i32 {
        let name = CFString::new(name);
        unsafe { AXUIElementPerformAction(self.0, name.as_concrete_TypeRef()) }
    }

    fn perform_checked(&self, name: &str) -> Result<(), DesktopError> {
        let result = self.perform(name);
        if result == kAXErrorSuccess {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(format!(
                "AX action {name} failed: {result}"
            )))
        }
    }

    fn set_value_ref(&self, name: &str, value: CFTypeRef) -> Result<(), DesktopError> {
        let name = CFString::new(name);
        let result =
            unsafe { AXUIElementSetAttributeValue(self.0, name.as_concrete_TypeRef(), value) };
        if result == kAXErrorSuccess {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(format!(
                "AX set {name:?} failed: {result}"
            )))
        }
    }

    fn set_string(&self, name: &str, value: &str) -> Result<(), DesktopError> {
        let value = CFString::new(value);
        self.set_value_ref(name, value.as_CFTypeRef())
    }

    fn set_bool(&self, name: &str, value: bool) -> Result<(), DesktopError> {
        let value = CFBoolean::from(value);
        self.set_value_ref(name, value.as_CFTypeRef())
    }

    fn set_point(&self, name: &str, point: Point) -> Result<(), DesktopError> {
        let point = core_graphics::geometry::CGPoint {
            x: point.x as f64,
            y: point.y as f64,
        };
        let value = unsafe {
            AXValueCreate(
                kAXValueTypeCGPoint,
                &point as *const _ as *const std::ffi::c_void,
            )
        };
        if value.is_null() {
            return Err(DesktopError::ActionFailed(
                "AX could not create position value".to_string(),
            ));
        }
        let result = self.set_value_ref(name, value as CFTypeRef);
        unsafe { CFRelease(value as CFTypeRef) };
        result
    }

    fn set_size(&self, name: &str, width: u32, height: u32) -> Result<(), DesktopError> {
        let size = core_graphics::geometry::CGSize {
            width: width as f64,
            height: height as f64,
        };
        let value = unsafe {
            AXValueCreate(
                kAXValueTypeCGSize,
                &size as *const _ as *const std::ffi::c_void,
            )
        };
        if value.is_null() {
            return Err(DesktopError::ActionFailed(
                "AX could not create size value".to_string(),
            ));
        }
        let result = self.set_value_ref(name, value as CFTypeRef);
        unsafe { CFRelease(value as CFTypeRef) };
        result
    }
}

impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

fn ax_point(value: &CFType, expected_type: u32) -> Option<(f64, f64)> {
    let raw = value.as_CFTypeRef();
    if value.type_of() != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(raw as accessibility_sys::AXValueRef) } != expected_type
    {
        return None;
    }
    let mut point = core_graphics::geometry::CGPoint { x: 0.0, y: 0.0 };
    if unsafe {
        AXValueGetValue(
            raw as accessibility_sys::AXValueRef,
            expected_type,
            &mut point as *mut _ as *mut std::ffi::c_void,
        )
    } {
        Some((point.x, point.y))
    } else {
        None
    }
}

fn ax_size(value: &CFType, expected_type: u32) -> Option<(f64, f64)> {
    let raw = value.as_CFTypeRef();
    if value.type_of() != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(raw as accessibility_sys::AXValueRef) } != expected_type
    {
        return None;
    }
    let mut size = core_graphics::geometry::CGSize {
        width: 0.0,
        height: 0.0,
    };
    if unsafe {
        AXValueGetValue(
            raw as accessibility_sys::AXValueRef,
            expected_type,
            &mut size as *mut _ as *mut std::ffi::c_void,
        )
    } {
        Some((size.width, size.height))
    } else {
        None
    }
}

fn parse_u32_id(id: &str, prefix: &str) -> Result<u32, DesktopError> {
    let raw = id.strip_prefix(&format!("{prefix}:"));
    let raw = raw.unwrap_or(id);
    raw.parse::<u32>()
        .map_err(|_| DesktopError::UnknownWindow(id.to_string()))
}

fn downscale_rgba(
    width: u32,
    height: u32,
    rgba: &[u8],
    max_width: Option<u32>,
) -> (u32, u32, Vec<u8>) {
    let Some(max_width) = max_width.filter(|value| *value > 0 && *value < width) else {
        return (width, height, rgba.to_vec());
    };
    let new_height = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;
    let mut out = vec![0u8; max_width as usize * new_height as usize * 4];
    for y in 0..new_height {
        let source_y = (y as u64 * height as u64 / new_height as u64) as u32;
        for x in 0..max_width {
            let source_x = (x as u64 * width as u64 / max_width as u64) as u32;
            let source = ((source_y * width + source_x) * 4) as usize;
            let destination = ((y * max_width + x) * 4) as usize;
            if source + 4 <= rgba.len() && destination + 4 <= out.len() {
                out[destination..destination + 4].copy_from_slice(&rgba[source..source + 4]);
            }
        }
    }
    (max_width, new_height, out)
}

fn mouse_type(button: MouseButton, down: bool) -> CGEventType {
    match (button, down) {
        (MouseButton::Left, true) => CGEventType::LeftMouseDown,
        (MouseButton::Left, false) => CGEventType::LeftMouseUp,
        (MouseButton::Right, true) => CGEventType::RightMouseDown,
        (MouseButton::Right, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

fn cg_button(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Middle => CGMouseButton::Center,
        MouseButton::Right => CGMouseButton::Right,
    }
}

fn key_code(name: &str) -> Result<u16, DesktopError> {
    let key = name.trim().to_ascii_uppercase();
    let code = match key.as_str() {
        "ENTER" | "RETURN" => KeyCode::RETURN,
        "TAB" => KeyCode::TAB,
        "SPACE" => KeyCode::SPACE,
        "BACKSPACE" | "DELETE" => KeyCode::DELETE,
        "ESC" | "ESCAPE" => KeyCode::ESCAPE,
        "META" | "COMMAND" | "CMD" => KeyCode::COMMAND,
        "SHIFT" => KeyCode::SHIFT,
        "ALT" | "OPTION" => KeyCode::OPTION,
        "CTRL" | "CONTROL" => KeyCode::CONTROL,
        "HOME" => KeyCode::HOME,
        "END" => KeyCode::END,
        "PAGEUP" | "PAGE_UP" => KeyCode::PAGE_UP,
        "PAGEDOWN" | "PAGE_DOWN" => KeyCode::PAGE_DOWN,
        "LEFT" => KeyCode::LEFT_ARROW,
        "RIGHT" => KeyCode::RIGHT_ARROW,
        "UP" => KeyCode::UP_ARROW,
        "DOWN" => KeyCode::DOWN_ARROW,
        "F1" => KeyCode::F1,
        "F2" => KeyCode::F2,
        "F3" => KeyCode::F3,
        "F4" => KeyCode::F4,
        "F5" => KeyCode::F5,
        "F6" => KeyCode::F6,
        "F7" => KeyCode::F7,
        "F8" => KeyCode::F8,
        "F9" => KeyCode::F9,
        "F10" => KeyCode::F10,
        "F11" => KeyCode::F11,
        "F12" => KeyCode::F12,
        _ => match key.as_str() {
            "A" => 0,
            "S" => 1,
            "D" => 2,
            "F" => 3,
            "H" => 4,
            "G" => 5,
            "Z" => 6,
            "X" => 7,
            "C" => 8,
            "V" => 9,
            "B" => 11,
            "Q" => 12,
            "W" => 13,
            "E" => 14,
            "R" => 15,
            "Y" => 16,
            "T" => 17,
            "O" => 31,
            "U" => 32,
            "I" => 34,
            "P" => 35,
            "L" => 37,
            "J" => 38,
            "K" => 40,
            "N" => 45,
            "M" => 46,
            "0" => 29,
            "1" => 18,
            "2" => 19,
            "3" => 20,
            "4" => 21,
            "5" => 23,
            "6" => 22,
            "7" => 26,
            "8" => 28,
            "9" => 25,
            _ => {
                return Err(DesktopError::ActionFailed(format!(
                    "unsupported macOS key name '{name}'"
                )))
            }
        },
    };
    Ok(code)
}

fn validate_application(application: &str) -> Result<(), DesktopError> {
    if application.trim().is_empty()
        || application.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, ';' | '|' | '&' | '$' | '>' | '<' | '`')
        })
    {
        return Err(DesktopError::ActionFailed(
            "application must be one application identity, not a shell command".to_string(),
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
