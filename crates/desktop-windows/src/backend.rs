use artifact_core::ArtifactStore;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
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
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UISelectionItemPattern, UITogglePattern,
    UIValuePattern, UIWindowPattern,
};
use uiautomation::types::{ExpandCollapseState, Handle, ToggleState};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsWindow, PostMessageW, SetCursorPos, SetForegroundWindow, SetWindowPos,
    ShowWindow, HWND_TOP, SWP_NOACTIVATE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE,
    WM_CLOSE,
};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{ImageEncoder, ImageEncoderPixelFormat, ImageFormat};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

const MAX_UI_NODES: usize = 2_048;
const FRAME_QUEUE_CAPACITY: usize = 4;

#[derive(Debug, Clone)]
struct RawFrame {
    width: u32,
    height: u32,
    timestamp_ms: u64,
    rgba: Vec<u8>,
}

struct CaptureHandler {
    sender: SyncSender<RawFrame>,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = SyncSender<RawFrame>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { sender: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let mut no_padding = Vec::new();
        let timestamp_ms = frame
            .timestamp()
            .map(|timestamp| (timestamp.Duration.max(0) as u64) / 10_000)
            .unwrap_or_else(|_| now_ms());
        let rgba = {
            let buffer = frame.buffer().map_err(|error| error.to_string())?;
            buffer.as_nopadding_buffer(&mut no_padding).to_vec()
        };
        match self.sender.try_send(RawFrame {
            width,
            height,
            timestamp_ms,
            rgba,
        }) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err("capture consumer stopped".to_string()),
        }
    }
}

enum NativeCapture {
    Monitor(CaptureControl<CaptureHandler, String>),
    Window(CaptureControl<CaptureHandler, String>),
}

struct WindowsCaptureSession {
    native: Option<NativeCapture>,
    receiver: Receiver<RawFrame>,
    artifacts: Arc<dyn ArtifactStore>,
    config: CaptureConfig,
    frame_id: u64,
    stopped: bool,
}

impl WindowsCaptureSession {
    fn encode_frame(&self, raw: RawFrame) -> Result<(u32, u32, Vec<u8>), DesktopError> {
        let (width, height, rgba) =
            downscale_rgba(raw.width, raw.height, &raw.rgba, self.config.max_width);
        let encoder = ImageEncoder::new(ImageFormat::Png, ImageEncoderPixelFormat::Rgba8)
            .map_err(|error| DesktopError::ActionFailed(format!("PNG encoder: {error}")))?;
        let png = encoder
            .encode(&rgba, width, height)
            .map_err(|error| DesktopError::ActionFailed(format!("PNG encode: {error}")))?;
        Ok((width, height, png))
    }
}

#[async_trait::async_trait]
impl CaptureSession for WindowsCaptureSession {
    async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError> {
        if self.stopped {
            return Err(DesktopError::BackendUnavailable(
                "Windows capture session has stopped".to_string(),
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let first = loop {
            match self.receiver.try_recv() {
                Ok(frame) => break frame,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(DesktopError::ActionFailed(
                            "Windows Graphics Capture produced no frame within 5 seconds"
                                .to_string(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(DesktopError::ActionFailed(
                        "Windows Graphics Capture stream ended".to_string(),
                    ));
                }
            }
        };
        // Drain queued frames so model observations use the newest available
        // state after a slow tool/model round trip.
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
        let native = self.native.take();
        let result = match native {
            Some(NativeCapture::Monitor(control)) => control.stop(),
            Some(NativeCapture::Window(control)) => control.stop(),
            None => Ok(()),
        };
        result.map_err(|error| DesktopError::ActionFailed(format!("stop capture: {error}")))
    }
}

#[derive(Clone)]
pub struct WindowsBackend {
    control_enabled: Arc<AtomicBool>,
}

pub fn plugin() -> Option<DesktopPlugin> {
    if Monitor::enumerate().ok()?.is_empty() {
        return None;
    }
    let backend = WindowsBackend {
        control_enabled: Arc::new(AtomicBool::new(true)),
    };
    Some(DesktopPlugin::new(
        DesktopPluginManifest {
            id: "desktop.windows-uia".to_string(),
            name: "Windows UI Automation and Graphics Capture".to_string(),
            version: "0.1.0".to_string(),
            platforms: vec!["windows".to_string()],
            capabilities: FULL_CAPABILITIES.to_vec(),
            description: "Windows UI Automation semantic controls, Windows Graphics Capture monitor/window frames, and SendInput fallback controls. Desktop capture maps to the primary display because the Windows Graphics Capture API exposes monitors and windows rather than a virtual-desktop item.".to_string(),
        },
        Arc::new(backend),
    ))
}

impl WindowsBackend {
    fn ensure_control(&self) -> Result<(), DesktopError> {
        if self.control_enabled.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(
                "desktop control is disabled; enable Allow Control or use the emergency stop to keep it revoked".to_string(),
            ))
        }
    }

    fn window(&self, window_id: &str) -> Result<Window, DesktopError> {
        let raw = parse_hwnd(window_id)?;
        let window = Window::from_raw_hwnd(raw as *mut std::ffi::c_void);
        if !window.is_valid() {
            return Err(DesktopError::StaleWindow(window_id.to_string()));
        }
        Ok(window)
    }

    fn hwnd(&self, window_id: &str) -> Result<HWND, DesktopError> {
        let raw = parse_hwnd(window_id)?;
        let hwnd = HWND(raw as *mut std::ffi::c_void);
        if unsafe { !IsWindow(Some(hwnd)).as_bool() } {
            return Err(DesktopError::StaleWindow(window_id.to_string()));
        }
        Ok(hwnd)
    }

    fn automation(&self) -> Result<UIAutomation, DesktopError> {
        UIAutomation::new()
            .map_err(|error| DesktopError::BackendUnavailable(format!("UI Automation: {error}")))
    }

    fn element_for_window(
        &self,
        automation: &UIAutomation,
        window_id: &str,
    ) -> Result<UIElement, DesktopError> {
        let hwnd = self.hwnd(window_id)?;
        automation
            .element_from_handle(Handle::from(hwnd))
            .map_err(|error| DesktopError::ActionFailed(format!("UI Automation window: {error}")))
    }

    fn element_by_id(
        &self,
        automation: &UIAutomation,
        window_id: &str,
        element_id: &str,
    ) -> Result<UIElement, DesktopError> {
        let prefix = format!("uia:{window_id}:root");
        let route = element_id
            .strip_prefix(&prefix)
            .ok_or_else(|| DesktopError::UnknownElement(element_id.to_string()))?;
        let root = self.element_for_window(automation, window_id)?;
        let walker = automation.create_tree_walker().map_err(|error| {
            DesktopError::ActionFailed(format!("UI Automation walker: {error}"))
        })?;
        let mut current = root;
        for component in route.split('/').filter(|part| !part.is_empty()) {
            let index = component
                .parse::<usize>()
                .map_err(|_| DesktopError::UnknownElement(element_id.to_string()))?;
            current = child_at(&walker, &current, index)
                .ok_or_else(|| DesktopError::UnknownElement(element_id.to_string()))?;
        }
        Ok(current)
    }

    fn check_element(&self, element: &UIElement, element_id: &str) -> Result<(), DesktopError> {
        if element.is_enabled().map_err(|error| {
            DesktopError::ActionFailed(format!("UI Automation enabled state: {error}"))
        })? {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(format!(
                "element '{element_id}' is disabled"
            )))
        }
    }

    fn send_input(&self, inputs: &[INPUT]) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == inputs.len() as u32 {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(format!(
                "SendInput accepted {sent} of {} events",
                inputs.len()
            )))
        }
    }

    fn native_capture(
        &self,
        config: &CaptureConfig,
    ) -> Result<(NativeCapture, Receiver<RawFrame>), DesktopError> {
        let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
        let cursor = if config.include_cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        let interval = Duration::from_millis((1_000 / config.max_fps.max(1)) as u64);
        match &config.target {
            CaptureTarget::Desktop => {
                let monitor = Monitor::primary().map_err(|error| {
                    DesktopError::BackendUnavailable(format!("primary monitor: {error}"))
                })?;
                let settings = Settings::new(
                    monitor,
                    cursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Exclude,
                    MinimumUpdateIntervalSettings::Custom(interval),
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    sender,
                );
                let control = CaptureHandler::start_free_threaded(settings).map_err(|error| {
                    DesktopError::BackendUnavailable(format!("Windows Graphics Capture: {error}"))
                })?;
                Ok((NativeCapture::Monitor(control), receiver))
            }
            CaptureTarget::Display(id) => {
                let index = parse_display_index(id)?;
                let monitor = Monitor::from_index(index).map_err(|error| {
                    DesktopError::UnknownWindow(format!("display {id}: {error}"))
                })?;
                let settings = Settings::new(
                    monitor,
                    cursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Exclude,
                    MinimumUpdateIntervalSettings::Custom(interval),
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    sender,
                );
                let control = CaptureHandler::start_free_threaded(settings).map_err(|error| {
                    DesktopError::BackendUnavailable(format!("Windows Graphics Capture: {error}"))
                })?;
                Ok((NativeCapture::Monitor(control), receiver))
            }
            CaptureTarget::Window(id) => {
                let window = self.window(id)?;
                let settings = Settings::new(
                    window,
                    cursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Exclude,
                    MinimumUpdateIntervalSettings::Custom(interval),
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    sender,
                );
                let control = CaptureHandler::start_free_threaded(settings).map_err(|error| {
                    DesktopError::BackendUnavailable(format!("Windows Graphics Capture: {error}"))
                })?;
                Ok((NativeCapture::Window(control), receiver))
            }
        }
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

    fn node(&self, element: &UIElement, id: String, parent_id: Option<String>) -> ElementNode {
        let role = element
            .get_localized_control_type()
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                element
                    .get_classname()
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let name = element.get_name().unwrap_or_default();
        let description = element
            .get_help_text()
            .ok()
            .filter(|value| !value.is_empty());
        let value = element
            .get_pattern::<UIValuePattern>()
            .ok()
            .and_then(|pattern| pattern.get_value().ok())
            .filter(|value| !value.is_empty());
        let bounds = element.get_bounding_rectangle().ok().map(|rect| Rect {
            x: rect.get_left(),
            y: rect.get_top(),
            width: rect.get_width().max(0),
            height: rect.get_height().max(0),
        });
        let enabled = element.is_enabled().ok();
        let focused = element.has_keyboard_focus().ok();
        let selected = element
            .get_pattern::<UISelectionItemPattern>()
            .ok()
            .and_then(|pattern| pattern.is_selected().ok());
        let checked = element
            .get_pattern::<UITogglePattern>()
            .ok()
            .and_then(|pattern| pattern.get_toggle_state().ok())
            .map(|state| state == ToggleState::On);
        let expanded = element
            .get_pattern::<UIExpandCollapsePattern>()
            .ok()
            .and_then(|pattern| pattern.get_state().ok())
            .and_then(|state| match state {
                ExpandCollapseState::Expanded => Some(true),
                ExpandCollapseState::Collapsed => Some(false),
                _ => None,
            });
        let mut actions = Vec::new();
        if element.get_pattern::<UIInvokePattern>().is_ok() {
            actions.push("invoke".to_string());
        }
        if element.get_pattern::<UISelectionItemPattern>().is_ok() {
            actions.push("select".to_string());
        }
        if element.get_pattern::<UIValuePattern>().is_ok() {
            actions.push("set_value".to_string());
        }
        if element.get_pattern::<UIExpandCollapsePattern>().is_ok() {
            actions.extend(["expand".to_string(), "collapse".to_string()]);
        }
        if element.get_pattern::<UIWindowPattern>().is_ok() {
            actions.push("window".to_string());
        }
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
            checked,
            expanded,
            parent_id,
            child_ids: Vec::new(),
            actions,
        }
    }

    fn collect_tree(
        &self,
        element: UIElement,
        id: String,
        parent_id: Option<String>,
        walker: &UITreeWalker,
        nodes: &mut Vec<ElementNode>,
    ) {
        if nodes.len() >= MAX_UI_NODES {
            return;
        }
        let node_index = nodes.len();
        nodes.push(self.node(&element, id.clone(), parent_id));
        let mut child_ids = Vec::new();
        let mut child = walker.get_first_child(&element).ok();
        let mut child_index = 0usize;
        while let Some(current) = child {
            if nodes.len() >= MAX_UI_NODES {
                break;
            }
            let child_id = format!("{id}/{child_index}");
            child_ids.push(child_id.clone());
            self.collect_tree(current.clone(), child_id, Some(id.clone()), walker, nodes);
            child = walker.get_next_sibling(&current).ok();
            child_index += 1;
        }
        nodes[node_index].child_ids = child_ids;
    }
}

#[async_trait::async_trait]
impl DesktopBackend for WindowsBackend {
    fn is_available(&self) -> bool {
        Monitor::enumerate()
            .map(|monitors| !monitors.is_empty())
            .unwrap_or(false)
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        let windows = Window::enumerate().map_err(|error| {
            DesktopError::BackendUnavailable(format!("window enumeration: {error}"))
        })?;
        Ok(windows
            .into_iter()
            .filter_map(|window| {
                let id = format!("hwnd:0x{:x}", window.as_raw_hwnd() as usize);
                let title = window.title().ok().filter(|title| !title.is_empty())?;
                let app = window.process_name().unwrap_or_default();
                Some(WindowInfo { id, title, app })
            })
            .collect())
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, DesktopError> {
        let monitors = Monitor::enumerate().map_err(|error| {
            DesktopError::BackendUnavailable(format!("monitor enumeration: {error}"))
        })?;
        Ok(monitors
            .into_iter()
            .enumerate()
            .map(|(offset, monitor)| {
                let index = monitor.index().unwrap_or(offset + 1);
                DisplayInfo {
                    id: index.to_string(),
                    name: monitor
                        .name()
                        .unwrap_or_else(|_| format!("Display {index}")),
                    width: monitor.width().unwrap_or_default(),
                    height: monitor.height().unwrap_or_default(),
                    scale_factor: 1.0,
                }
            })
            .collect())
    }

    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        let automation = self.automation()?;
        let root = self.element_for_window(&automation, window_id)?;
        let walker = automation.create_tree_walker().map_err(|error| {
            DesktopError::ActionFailed(format!("UI Automation walker: {error}"))
        })?;
        let mut nodes = Vec::new();
        self.collect_tree(
            root,
            format!("uia:{window_id}:root"),
            None,
            &walker,
            &mut nodes,
        );
        Ok(nodes)
    }

    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        if let Ok(pattern) = element.get_pattern::<UIInvokePattern>() {
            return pattern
                .invoke()
                .map_err(|error| DesktopError::ActionFailed(format!("UIA invoke: {error}")));
        }
        let bounds = element
            .get_bounding_rectangle()
            .map_err(|error| DesktopError::ActionFailed(format!("element bounds: {error}")))?;
        let point = Point {
            x: bounds.get_left() + bounds.get_width() / 2,
            y: bounds.get_top() + bounds.get_height() / 2,
        };
        drop(element);
        drop(automation);
        self.click_sync(Some(window_id), point)
    }

    async fn focus_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        element
            .set_focus()
            .map_err(|error| DesktopError::ActionFailed(format!("UIA focus: {error}")))
    }

    async fn set_value(
        &self,
        window_id: &str,
        element_id: &str,
        value: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        element
            .get_pattern::<UIValuePattern>()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!("element has no value pattern: {error}"))
            })?
            .set_value(value)
            .map_err(|error| DesktopError::ActionFailed(format!("UIA set value: {error}")))
    }

    async fn select_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        element
            .get_pattern::<UISelectionItemPattern>()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!(
                    "element has no selection pattern: {error}"
                ))
            })?
            .select()
            .map_err(|error| DesktopError::ActionFailed(format!("UIA select: {error}")))
    }

    async fn expand_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        element
            .get_pattern::<UIExpandCollapsePattern>()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!(
                    "element has no expansion pattern: {error}"
                ))
            })?
            .expand()
            .map_err(|error| DesktopError::ActionFailed(format!("UIA expand: {error}")))
    }

    async fn collapse_element(
        &self,
        window_id: &str,
        element_id: &str,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let automation = self.automation()?;
        let element = self.element_by_id(&automation, window_id, element_id)?;
        self.check_element(&element, element_id)?;
        element
            .get_pattern::<UIExpandCollapsePattern>()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!(
                    "element has no expansion pattern: {error}"
                ))
            })?
            .collapse()
            .map_err(|error| DesktopError::ActionFailed(format!("UIA collapse: {error}")))
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
        let (native, receiver) = self.native_capture(&config)?;
        Ok(Box::new(WindowsCaptureSession {
            native: Some(native),
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
        self.ensure_control()?;
        let hwnd = self.hwnd(window_id)?;
        if unsafe { SetForegroundWindow(hwnd).as_bool() } {
            Ok(())
        } else {
            Err(DesktopError::ActionFailed(
                "SetForegroundWindow rejected the window".to_string(),
            ))
        }
    }

    async fn close_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let hwnd = self.hwnd(window_id)?;
        unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
            .map_err(|error| DesktopError::ActionFailed(format!("WM_CLOSE: {error}")))
    }

    async fn move_window(&self, window_id: &str, at: Point) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let hwnd = self.hwnd(window_id)?;
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect) }
            .map_err(|error| DesktopError::ActionFailed(format!("GetWindowRect: {error}")))?;
        let width = rect.right.saturating_sub(rect.left).max(1);
        let height = rect.bottom.saturating_sub(rect.top).max(1);
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                at.x,
                at.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| DesktopError::ActionFailed(format!("SetWindowPos: {error}")))
    }

    async fn resize_window(
        &self,
        window_id: &str,
        width: u32,
        height: u32,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let hwnd = self.hwnd(window_id)?;
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect) }
            .map_err(|error| DesktopError::ActionFailed(format!("GetWindowRect: {error}")))?;
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                rect.left,
                rect.top,
                width.min(i32::MAX as u32) as i32,
                height.min(i32::MAX as u32) as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| DesktopError::ActionFailed(format!("SetWindowPos: {error}")))
    }

    async fn minimize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.show_window(window_id, SW_MINIMIZE).await
    }

    async fn maximize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.show_window(window_id, SW_MAXIMIZE).await
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.show_window(window_id, SW_RESTORE).await
    }

    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.click_sync(window_id, at)
    }

    async fn move_pointer(&self, at: Point) -> Result<(), DesktopError> {
        self.ensure_control()?;
        unsafe { SetCursorPos(at.x, at.y) }
            .map_err(|error| DesktopError::ActionFailed(format!("SetCursorPos: {error}")))
    }

    async fn double_click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.click(window_id, at).await?;
        self.click(None, at).await
    }

    async fn mouse_down(&self, button: MouseButton) -> Result<(), DesktopError> {
        let (down, _) = mouse_flags(button);
        self.send_input(&[mouse_input(down)])
    }

    async fn mouse_up(&self, button: MouseButton) -> Result<(), DesktopError> {
        let (_, up) = mouse_flags(button);
        self.send_input(&[mouse_input(up)])
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
            self.focus_window(window_id).await?;
        }
        let mut inputs = Vec::new();
        if delta_y != 0 {
            inputs.push(mouse_input_with_data(
                MOUSEEVENTF_WHEEL,
                delta_y.saturating_mul(120) as u32,
            ));
        }
        if delta_x != 0 {
            inputs.push(mouse_input_with_data(
                MOUSEEVENTF_HWHEEL,
                delta_x.saturating_mul(120) as u32,
            ));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        self.send_input(&inputs)
    }

    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if let Some(window_id) = window_id {
            self.focus_window(window_id).await?;
        }
        let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
        for unit in text.encode_utf16() {
            inputs.push(keyboard_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
            inputs.push(keyboard_input(
                VIRTUAL_KEY(0),
                unit,
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
            ));
        }
        self.send_input(&inputs)
    }

    async fn key_down(&self, key: &str) -> Result<(), DesktopError> {
        self.send_input(&[keyboard_input(key_code(key)?, 0, Default::default())])
    }

    async fn key_up(&self, key: &str) -> Result<(), DesktopError> {
        self.send_input(&[keyboard_input(key_code(key)?, 0, KEYEVENTF_KEYUP)])
    }

    async fn clipboard_read(&self, mime_type: &str) -> Result<String, DesktopError> {
        if mime_type != "text/plain" {
            return Err(DesktopError::BackendUnavailable(
                "Windows backend currently exposes only text/plain clipboard data".to_string(),
            ));
        }
        uiautomation::clipboards::Clipboard::open()
            .map_err(|error| DesktopError::ActionFailed(format!("open clipboard: {error}")))?
            .get_text()
            .map_err(|error| DesktopError::ActionFailed(format!("read clipboard: {error}")))
    }

    async fn clipboard_write(&self, mime_type: &str, text: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if mime_type != "text/plain" {
            return Err(DesktopError::BackendUnavailable(
                "Windows backend currently exposes only text/plain clipboard data".to_string(),
            ));
        }
        uiautomation::clipboards::Clipboard::open()
            .map_err(|error| DesktopError::ActionFailed(format!("open clipboard: {error}")))?
            .set_text(text)
            .map_err(|error| DesktopError::ActionFailed(format!("write clipboard: {error}")))
    }

    async fn launch_application(&self, application: &str) -> Result<(), DesktopError> {
        self.ensure_control()?;
        validate_application(application)?;
        Command::new(application)
            .spawn()
            .map(|_| ())
            .map_err(|error| DesktopError::ActionFailed(format!("launch '{application}': {error}")))
    }
}

impl WindowsBackend {
    fn click_sync(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.ensure_control()?;
        if let Some(window_id) = window_id {
            let hwnd = self.hwnd(window_id)?;
            if unsafe { !SetForegroundWindow(hwnd).as_bool() } {
                return Err(DesktopError::ActionFailed(
                    "SetForegroundWindow rejected the window".to_string(),
                ));
            }
        }
        unsafe { SetCursorPos(at.x, at.y) }
            .map_err(|error| DesktopError::ActionFailed(format!("SetCursorPos: {error}")))?;
        let (down, up) = mouse_flags(MouseButton::Left);
        self.send_input(&[mouse_input(down), mouse_input(up)])
    }

    async fn show_window(
        &self,
        window_id: &str,
        command: windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD,
    ) -> Result<(), DesktopError> {
        self.ensure_control()?;
        let hwnd = self.hwnd(window_id)?;
        let _ = unsafe { ShowWindow(hwnd, command) };
        Ok(())
    }
}

fn child_at(walker: &UITreeWalker, parent: &UIElement, index: usize) -> Option<UIElement> {
    let mut current = walker.get_first_child(parent).ok()?;
    for _ in 0..index {
        current = walker.get_next_sibling(&current).ok()?;
    }
    Some(current)
}

fn parse_hwnd(id: &str) -> Result<usize, DesktopError> {
    let raw = id
        .strip_prefix("hwnd:0x")
        .or_else(|| id.strip_prefix("0x"))
        .ok_or_else(|| DesktopError::UnknownWindow(id.to_string()))?;
    let value =
        usize::from_str_radix(raw, 16).map_err(|_| DesktopError::UnknownWindow(id.to_string()))?;
    if value == 0 {
        return Err(DesktopError::UnknownWindow(id.to_string()));
    }
    Ok(value)
}

fn parse_display_index(id: &str) -> Result<usize, DesktopError> {
    let value = id
        .strip_prefix("display:")
        .unwrap_or(id)
        .parse::<usize>()
        .map_err(|_| DesktopError::UnknownWindow(format!("invalid display id '{id}'")))?;
    if value == 0 {
        return Err(DesktopError::UnknownWindow(
            "Windows display ids are one-based".to_string(),
        ));
    }
    Ok(value)
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
    let mut out = vec![0u8; (max_width as usize) * (new_height as usize) * 4];
    for y in 0..new_height {
        let src_y = (y as u64 * height as u64 / new_height as u64) as u32;
        for x in 0..max_width {
            let src_x = (x as u64 * width as u64 / max_width as u64) as u32;
            let src = ((src_y * width + src_x) * 4) as usize;
            let dst = ((y * max_width + x) * 4) as usize;
            if src + 4 <= rgba.len() && dst + 4 <= out.len() {
                out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
            }
        }
    }
    (max_width, new_height, out)
}

fn mouse_flags(
    button: MouseButton,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    }
}

fn mouse_input(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    mouse_input_with_data(flags, 0)
}

fn mouse_input_with_data(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    data: u32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard_input(
    key: VIRTUAL_KEY,
    scan: u16,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_code(name: &str) -> Result<VIRTUAL_KEY, DesktopError> {
    let key = name.trim().to_ascii_uppercase();
    let code = match key.as_str() {
        "CTRL" | "CONTROL" => 0x11,
        "ALT" | "OPTION" | "MENU" => 0x12,
        "SHIFT" => 0x10,
        "META" | "WIN" | "WINDOWS" | "COMMAND" => 0x5B,
        "ENTER" | "RETURN" => 0x0D,
        "TAB" => 0x09,
        "ESC" | "ESCAPE" => 0x1B,
        "BACKSPACE" | "BACK" => 0x08,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PAGE_UP" => 0x21,
        "PAGEDOWN" | "PAGE_DOWN" => 0x22,
        "LEFT" => 0x25,
        "UP" => 0x26,
        "RIGHT" => 0x27,
        "DOWN" => 0x28,
        "SPACE" => 0x20,
        "INSERT" | "INS" => 0x2D,
        "CAPSLOCK" | "CAPS_LOCK" => 0x14,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        _ if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() => key.as_bytes()[0],
        _ if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() => key.as_bytes()[0],
        _ => {
            return Err(DesktopError::ActionFailed(format!(
                "unsupported Windows key name '{name}'"
            )))
        }
    };
    Ok(VIRTUAL_KEY(code as u16))
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
            "application must be one executable identity, not a shell command".to_string(),
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
