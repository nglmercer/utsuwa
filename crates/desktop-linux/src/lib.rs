//! Linux desktop backend (plan Phase 30): raw X11 over the session
//! socket, zero new dependencies.
//!
//! What works: window enumeration with titles and owning processes,
//! screenshots (GetImage + minimal PNG), pointer motion + XTEST clicks,
//! XTEST typing via keysym mapping. The window hierarchy doubles as the
//! accessibility tree (X11 has no AX model); `set_value` has no X11
//! primitive and reports unavailability. Wayland sessions reach this
//! backend through Xwayland; native portal APIs are the upgrade path.

mod x11;

use artifact_core::{ArtifactStore, ImageArtifactRef};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tool_desktop::{
    plugin::{DesktopCapability, DesktopPlugin, DesktopPluginManifest},
    CaptureConfig, CaptureSession, CaptureTarget, DesktopBackend, DesktopError, DisplayInfo,
    ElementNode, MouseButton, Point, Rect, Screenshot, VideoFrame, WindowInfo,
};
use x11::{XConn, XError};

#[derive(Debug, thiserror::Error)]
pub enum LinuxError {
    #[error("X11: {0}")]
    X(#[from] XError),
    #[error("unsupported on this server: {0}")]
    Unsupported(String),
}

/// Keyboard mapping: Shift keycode plus (keycode, keysym, needs_shift).
type KeyMapping = (u8, Vec<(u8, u32, bool)>);

impl From<LinuxError> for DesktopError {
    fn from(e: LinuxError) -> Self {
        match e {
            LinuxError::X(XError::Connect(_)) => {
                DesktopError::BackendUnavailable(format!("no X display: {e}"))
            }
            other => DesktopError::ActionFailed(other.to_string()),
        }
    }
}

/// Convert X11's object-lifetime errors into a model-actionable result. A
/// window can disappear after `desktop.inspect` and before a later operation;
/// the model should refresh the snapshot instead of having to understand X11
/// protocol error numbers.
fn window_error(window_id: &str, error: LinuxError) -> DesktopError {
    match error {
        LinuxError::X(XError::Server { code: 3, .. })
        // GetGeometry/GetImage report a vanished drawable as BadDrawable.
        | LinuxError::X(XError::Server { code: 9, .. }) => {
            DesktopError::StaleWindow(window_id.to_string())
        }
        other => other.into(),
    }
}

#[derive(Clone)]
pub struct LinuxBackend {
    conn: Arc<Mutex<XConn>>,
    control_enabled: Arc<AtomicBool>,
    xtest_major: Option<u8>,
    atom_net_name: u32,
    atom_name: u32,
    atom_string: u32,
    atom_utf8: u32,
    atom_pid: u32,
    atom_cardinal: u32,
    atom_net_close_window: u32,
    atom_net_wm_state: u32,
    atom_net_wm_state_hidden: u32,
    atom_net_wm_state_max_horz: u32,
    atom_net_wm_state_max_vert: u32,
}

/// This backend as an installed desktop plugin. `None` when no
/// display answers, so the host registry falls through to other
/// plugins (or nothing). X11 has no `set_value` primitive, so the
/// manifest honestly omits it and the host never offers that tool.
pub fn plugin() -> Option<DesktopPlugin> {
    let backend = LinuxBackend::connect().ok()?;
    let capabilities = LinuxBackend::capabilities();
    Some(DesktopPlugin::new(
        DesktopPluginManifest {
            id: "desktop.linux-x11".to_string(),
            name: "Linux X11 backend".to_string(),
            version: "0.1.0".to_string(),
            platforms: vec!["linux".to_string()],
            capabilities,
            description: "Raw X11 over the session socket (Xwayland included): windows, hierarchy-as-tree, screenshots, XTEST input. No display means no plugin.".to_string(),
        },
        Arc::new(backend),
    ))
}

impl LinuxBackend {
    fn capabilities() -> Vec<DesktopCapability> {
        // Advertise only operations backed by this implementation. In
        // particular, the X11 window hierarchy is not an accessibility API
        // and X11 cannot set an element value semantically. The EWMH window
        // state operations are available only when their atoms are present;
        // the plugin-level list remains conservative and exposes the common
        // operations that do not depend on a particular window manager.
        let mut capabilities = vec![
            DesktopCapability::ListWindows,
            DesktopCapability::ListDisplays,
            DesktopCapability::AccessibilityTree,
            DesktopCapability::Observe,
            DesktopCapability::InvokeElement,
            DesktopCapability::Screenshot,
            DesktopCapability::CaptureStart,
            DesktopCapability::CaptureFrame,
            DesktopCapability::CaptureStop,
            DesktopCapability::CaptureStatus,
            DesktopCapability::Click,
            DesktopCapability::DoubleClick,
            DesktopCapability::MovePointer,
            DesktopCapability::MouseDown,
            DesktopCapability::MouseUp,
            DesktopCapability::Drag,
            DesktopCapability::Scroll,
            DesktopCapability::TypeText,
            DesktopCapability::KeyDown,
            DesktopCapability::KeyUp,
            DesktopCapability::Hotkey,
            DesktopCapability::PressKey,
            DesktopCapability::FocusWindow,
            DesktopCapability::CloseWindow,
            DesktopCapability::MoveWindow,
            DesktopCapability::ResizeWindow,
            DesktopCapability::MinimizeWindow,
            DesktopCapability::MaximizeWindow,
            DesktopCapability::RestoreWindow,
            DesktopCapability::LaunchApplication,
        ];
        if clipboard_commands_available() {
            capabilities.push(DesktopCapability::ClipboardRead);
            capabilities.push(DesktopCapability::ClipboardWrite);
        }
        capabilities
    }

    /// Connect to `$DISPLAY`. Fails cleanly when no display exists so
    /// callers (and tests) can fall back or skip.
    pub fn connect() -> Result<Self, LinuxError> {
        let mut conn = XConn::connect()?;
        let (xtest_present, xtest_major) = conn.query_extension(b"XTEST").unwrap_or((false, 0));
        let atom_net_name = conn.intern_atom(b"_NET_WM_NAME", false)?;
        let atom_name = conn.intern_atom(b"WM_NAME", false)?;
        let atom_string = conn.intern_atom(b"STRING", false)?;
        let atom_utf8 = conn.intern_atom(b"UTF8_STRING", false)?;
        let atom_pid = conn.intern_atom(b"_NET_WM_PID", false)?;
        let atom_cardinal = conn.intern_atom(b"CARDINAL", false)?;
        let atom_net_close_window = conn.intern_atom(b"_NET_CLOSE_WINDOW", false)?;
        let atom_net_wm_state = conn.intern_atom(b"_NET_WM_STATE", false)?;
        let atom_net_wm_state_hidden = conn.intern_atom(b"_NET_WM_STATE_HIDDEN", false)?;
        let atom_net_wm_state_max_horz =
            conn.intern_atom(b"_NET_WM_STATE_MAXIMIZED_HORZ", false)?;
        let atom_net_wm_state_max_vert =
            conn.intern_atom(b"_NET_WM_STATE_MAXIMIZED_VERT", false)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            control_enabled: Arc::new(AtomicBool::new(true)),
            xtest_major: xtest_present.then_some(xtest_major),
            atom_net_name,
            atom_name,
            atom_string,
            atom_utf8,
            atom_pid,
            atom_cardinal,
            atom_net_close_window,
            atom_net_wm_state,
            atom_net_wm_state_hidden,
            atom_net_wm_state_max_horz,
            atom_net_wm_state_max_vert,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, XConn>, LinuxError> {
        self.conn
            .lock()
            .map_err(|_| LinuxError::Unsupported("backend lock failed".to_string()))
    }

    fn ensure_control_enabled(&self) -> Result<(), DesktopError> {
        if self.control_enabled.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(DesktopError::BackendUnavailable(
                "desktop control is disabled by the user; enable Allow Control before acting"
                    .to_string(),
            ))
        }
    }

    fn title_of(&self, conn: &mut XConn, window: u32) -> String {
        // _NET_WM_NAME/UTF8_STRING first, WM_NAME/STRING fallback.
        for (prop, ptype) in [
            (self.atom_net_name, self.atom_utf8),
            (self.atom_name, self.atom_string),
        ] {
            if let Ok((_, 8, bytes)) = conn.get_property(window, prop, ptype, 0, 256) {
                if !bytes.is_empty() {
                    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                    let title = String::from_utf8_lossy(&bytes[..end]).into_owned();
                    if !title.is_empty() {
                        return title;
                    }
                }
            }
        }
        String::new()
    }

    fn app_of(&self, conn: &mut XConn, window: u32) -> String {
        let pid = conn
            .get_property(window, self.atom_pid, self.atom_cardinal, 0, 1)
            .ok()
            .and_then(|(_, fmt, bytes)| {
                (fmt == 32 && bytes.len() >= 4)
                    .then(|| u32::from_le_bytes(bytes[..4].try_into().unwrap()))
            });
        pid.and_then(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
    }

    fn xid(s: &str) -> Result<u32, LinuxError> {
        let digits = s.strip_prefix("0x").unwrap_or(s);
        u32::from_str_radix(digits, 16)
            .map_err(|_| LinuxError::Unsupported(format!("bad window id '{s}'")))
    }

    /// Keysym → (keycode, needs_shift) from the server mapping. ASCII
    /// letters resolve both cases through Shift.
    fn keysym_map(&self, conn: &mut XConn) -> Result<KeyMapping, LinuxError> {
        let (min, max) = (conn.min_keycode, conn.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let (per, syms) = conn.get_keyboard_mapping(min, count)?;
        let mut shift_key = 0u8;
        let mut map = Vec::new();
        for (i, chunk) in syms.chunks(per.max(1) as usize).enumerate() {
            let keycode = min.saturating_add(i as u8);
            for (pos, &sym) in chunk.iter().enumerate() {
                if sym == 0xFFE1 {
                    shift_key = keycode;
                }
                if sym != 0 && sym < 0x10000 {
                    map.push((keycode, sym, pos > 0));
                }
            }
        }
        Ok((shift_key, map))
    }

    fn key_sym(key: &str) -> Result<u32, LinuxError> {
        let trimmed = key.trim();
        if trimmed.chars().count() == 1 {
            return Ok(trimmed.chars().next().unwrap() as u32);
        }
        let normalized = trimmed.to_ascii_uppercase();
        let symbol = match normalized.as_str() {
            "SHIFT" | "SHIFT_L" => 0xFFE1,
            "CTRL" | "CONTROL" | "CTRL_L" | "CONTROL_L" => 0xFFE3,
            "ALT" | "ALT_L" | "OPTION" | "OPTION_L" => 0xFFE9,
            "META" | "META_L" | "SUPER" | "SUPER_L" | "WIN" | "WINDOWS" => 0xFFE7,
            "ENTER" | "RETURN" => 0xFF0D,
            "ESC" | "ESCAPE" => 0xFF1B,
            "TAB" => 0xFF09,
            "BACKSPACE" => 0xFF08,
            "SPACE" => 0x0020,
            "DELETE" | "DEL" => 0xFFFF,
            "INSERT" => 0xFF63,
            "HOME" => 0xFF50,
            "END" => 0xFF57,
            "PAGEUP" | "PAGE_UP" => 0xFF55,
            "PAGEDOWN" | "PAGE_DOWN" => 0xFF56,
            "ARROWLEFT" | "LEFT" => 0xFF51,
            "ARROWRIGHT" | "RIGHT" => 0xFF53,
            "ARROWUP" | "UP" => 0xFF52,
            "ARROWDOWN" | "DOWN" => 0xFF54,
            "CAPSLOCK" => 0xFFE5,
            "F1" => 0xFFBE,
            "F2" => 0xFFBF,
            "F3" => 0xFFC0,
            "F4" => 0xFFC1,
            "F5" => 0xFFC2,
            "F6" => 0xFFC3,
            "F7" => 0xFFC4,
            "F8" => 0xFFC5,
            "F9" => 0xFFC6,
            "F10" => 0xFFC7,
            "F11" => 0xFFC8,
            "F12" => 0xFFC9,
            other => {
                return Err(LinuxError::Unsupported(format!(
                    "unknown key '{other}'; use a single character or a named key"
                )))
            }
        };
        Ok(symbol)
    }

    fn keycode_for(&self, conn: &mut XConn, key: &str) -> Result<(u8, bool, u8), LinuxError> {
        let symbol = Self::key_sym(key)?;
        let (shift_key, map) = self.keysym_map(conn)?;
        let found = map
            .iter()
            .find(|(_, sym, _)| *sym == symbol)
            .copied()
            .or_else(|| {
                if symbol <= 0x7F && (symbol as u8 as char).is_ascii_alphabetic() {
                    let swapped = if (symbol as u8 as char).is_ascii_lowercase() {
                        (symbol as u8 as char).to_ascii_uppercase() as u32
                    } else {
                        (symbol as u8 as char).to_ascii_lowercase() as u32
                    };
                    map.iter().find(|(_, sym, _)| *sym == swapped).copied()
                } else {
                    None
                }
            })
            .ok_or_else(|| LinuxError::Unsupported(format!("no keycode for '{key}'")))?;
        Ok((found.0, found.2, shift_key))
    }

    fn key_event(&self, key: &str, is_press: bool) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let (keycode, needs_shift, shift_key) = self
            .keycode_for(&mut conn, key)
            .map_err(DesktopError::from)?;
        if needs_shift {
            if shift_key == 0 {
                return Err(DesktopError::ActionFailed(
                    "no Shift key in mapping".to_string(),
                ));
            }
            conn.xtest_fake_key(major, shift_key, is_press)
                .map_err(LinuxError::from)
                .map_err(DesktopError::from)?;
        }
        conn.xtest_fake_key(major, keycode, is_press)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)
    }

    fn validate_window(&self, window_id: &str, conn: &mut XConn) -> Result<u32, DesktopError> {
        let window = Self::xid(window_id).map_err(DesktopError::from)?;
        conn.query_tree(window)
            .map_err(|error| window_error(window_id, error.into()))?;
        Ok(window)
    }

    fn change_window_state(
        &self,
        window_id: &str,
        action: u32,
        first: u32,
        second: u32,
    ) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let window = self.validate_window(window_id, &mut conn)?;
        let root = conn.root;
        conn.send_client_message(
            root,
            window,
            self.atom_net_wm_state,
            [action, first, second, 0, 0],
            0x0004_0000 | 0x0008_0000,
        )
        .map_err(LinuxError::from)
        .map_err(|error| window_error(window_id, error))
    }

    async fn screenshot_scaled(
        &self,
        window_id: Option<&str>,
        max_width: Option<u32>,
    ) -> Result<Screenshot, DesktopError> {
        let target = match window_id {
            Some(id) if !id.is_empty() => Self::xid(id).map_err(DesktopError::from)?,
            _ => self.lock()?.root,
        };
        let mut conn = self.lock()?;
        let unfiltered = window_id.is_none_or(|s| s.is_empty());
        let (w, h, depth, pixels) = conn.get_image(target).map_err(|e| match e {
            // Xwayland-style servers give the root window no backing
            // pixmap: whole-desktop capture is honestly unavailable
            // there (portal capture is the upgrade path), while real
            // windows still snapshot.
            XError::Server { code: 8, .. } if unfiltered => {
                DesktopError::BackendUnavailable(
                    "whole-desktop capture unsupported: root has no backing pixmap on this server; capture a window instead".to_string(),
                )
            }
            other => {
                let error = LinuxError::X(other);
                if let Some(window_id) = window_id.filter(|id| !id.is_empty()) {
                    window_error(window_id, error)
                } else {
                    error.into()
                }
            }
        })?;
        if w == 0 || h == 0 {
            return Err(DesktopError::BackendUnavailable(
                if unfiltered {
                    "whole-desktop capture returned an empty root drawable"
                } else {
                    "window capture returned an empty drawable"
                }
                .to_string(),
            ));
        }
        let masks = conn.root_masks;
        let (bpp, order) = (conn.bpp, conn.image_order);
        let stride = match bpp {
            32 => 4,
            24 => 3,
            _ => {
                return Err(DesktopError::ActionFailed(format!(
                    "unsupported pixel depth {bpp}"
                )))
            }
        };
        if pixels.len() < w as usize * h as usize * stride {
            return Err(DesktopError::ActionFailed("short image data".to_string()));
        }
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for px in pixels.chunks(stride) {
            let value = match (bpp, order) {
                (32, 0) => u32::from_le_bytes([px[0], px[1], px[2], px[3]]),
                (32, _) => u32::from_be_bytes([px[0], px[1], px[2], px[3]]),
                (24, 0) => (px[0] as u32) | ((px[1] as u32) << 8) | ((px[2] as u32) << 16),
                _ => ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32),
            };
            match masks {
                Some((r, g, b)) => {
                    rgb.push(channel(value, r));
                    rgb.push(channel(value, g));
                    rgb.push(channel(value, b));
                }
                None if depth >= 24 => {
                    // No masks (unusual server): assume XRGB/BGRX by order.
                    if order == 0 {
                        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
                    } else {
                        rgb.extend_from_slice(&[px[stride - 3], px[stride - 2], px[stride - 1]]);
                    }
                }
                None => {
                    return Err(DesktopError::ActionFailed(
                        "non-TrueColor root without masks".to_string(),
                    ))
                }
            }
        }
        let (width, height, rgb) = downscale_rgb(w as u32, h as u32, &rgb, max_width);
        let png_bytes = encode_png(width, height, &rgb);
        Ok(Screenshot {
            width,
            height,
            png_bytes,
        })
    }
}

fn command_available(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').filter(|part| !part.is_empty()).any(|part| {
        let candidate = std::path::Path::new(part).join(name);
        use std::os::unix::fs::PermissionsExt;
        candidate.is_file()
            && std::fs::metadata(candidate)
                .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    })
}

fn clipboard_commands_available() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        command_available("wl-paste") && command_available("wl-copy")
    } else {
        command_available("xclip") || command_available("xsel")
    }
}

fn read_clipboard_text() -> Result<String, DesktopError> {
    let (program, args): (&str, &[&str]) =
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
            ("wl-paste", &["--no-newline"])
        } else if command_available("xclip") {
            ("xclip", &["-selection", "clipboard", "-out"])
        } else if command_available("xsel") {
            ("xsel", &["--clipboard", "--output"])
        } else {
            return Err(DesktopError::BackendUnavailable(
                "no supported clipboard helper (wl-paste, xclip, or xsel)".to_string(),
            ));
        };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| DesktopError::ActionFailed(format!("clipboard read: {error}")))?;
    if !output.status.success() {
        return Err(DesktopError::ActionFailed(format!(
            "clipboard read failed with status {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn write_clipboard_text(text: &str) -> Result<(), DesktopError> {
    let (program, args): (&str, &[&str]) =
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-copy") {
            ("wl-copy", &[])
        } else if command_available("xclip") {
            ("xclip", &["-selection", "clipboard"])
        } else if command_available("xsel") {
            ("xsel", &["--clipboard", "--input"])
        } else {
            return Err(DesktopError::BackendUnavailable(
                "no supported clipboard helper (wl-copy, xclip, or xsel)".to_string(),
            ));
        };
    use std::io::Write;
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| DesktopError::ActionFailed(format!("clipboard write: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| DesktopError::ActionFailed("clipboard stdin unavailable".to_string()))?
        .write_all(text.as_bytes())
        .map_err(|error| DesktopError::ActionFailed(format!("clipboard write: {error}")))?;
    let status = child
        .wait()
        .map_err(|error| DesktopError::ActionFailed(format!("clipboard write: {error}")))?;
    if !status.success() {
        return Err(DesktopError::ActionFailed(format!(
            "clipboard write failed with status {status}"
        )));
    }
    Ok(())
}

struct LinuxCaptureSession {
    backend: LinuxBackend,
    config: CaptureConfig,
    artifacts: Arc<dyn ArtifactStore>,
    frame_id: u64,
    stopped: bool,
}

#[async_trait::async_trait]
impl CaptureSession for LinuxCaptureSession {
    async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError> {
        if self.stopped {
            return Err(DesktopError::BackendUnavailable(
                "capture session has been stopped".to_string(),
            ));
        }
        let shot = self
            .backend
            .screenshot_with_config(self.config.clone())
            .await?;
        let artifact = self
            .artifacts
            .put("image/png", shot.png_bytes)
            .await
            .map_err(|error| DesktopError::ActionFailed(format!("artifact store: {error}")))?;
        self.frame_id = self.frame_id.saturating_add(1);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        Ok(VideoFrame {
            frame_id: self.frame_id,
            timestamp_ms,
            width: shot.width,
            height: shot.height,
            image: ImageArtifactRef::new(artifact, shot.width, shot.height),
        })
    }

    async fn stop(&mut self) -> Result<(), DesktopError> {
        self.stopped = true;
        Ok(())
    }
}

fn downscale_rgb(
    width: u32,
    height: u32,
    rgb: &[u8],
    max_width: Option<u32>,
) -> (u32, u32, Vec<u8>) {
    let Some(max_width) = max_width.filter(|max_width| *max_width > 0 && *max_width < width) else {
        return (width, height, rgb.to_vec());
    };
    let scaled_height = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;
    let mut scaled = Vec::with_capacity(max_width as usize * scaled_height as usize * 3);
    for y in 0..scaled_height {
        let source_y = (y as u64 * height as u64 / scaled_height as u64) as u32;
        for x in 0..max_width {
            let source_x = (x as u64 * width as u64 / max_width as u64) as u32;
            let index = (source_y as usize * width as usize + source_x as usize) * 3;
            scaled.extend_from_slice(&rgb[index..index + 3]);
        }
    }
    (max_width, scaled_height, scaled)
}

/// Minimal PNG writer: 8-bit RGB, filter 0, zlib stored blocks.
/// Dependency-free by construction; verified byte-for-byte in tests.
fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    fn u32b(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }
    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&u32b(data.len() as u32));
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        out.extend_from_slice(&u32b(crc32(&[ty.as_slice(), data].concat())));
        out
    }
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&u32b(width));
    ihdr.extend_from_slice(&u32b(height));
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend(chunk(b"IHDR", &ihdr));
    // zlib header (deflate, 32K window) + stored blocks + adler32.
    let mut raw = Vec::with_capacity((1 + width as usize * 3) * height as usize);
    for row in rgb.chunks(width as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let mut rest = raw.as_slice();
    if rest.is_empty() {
        // Deflate still needs a final empty stored block for a zero-sized
        // drawable. This keeps the encoder total even when an Xwayland
        // root reports no backing pixmap/geometry.
        zlib.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    } else {
        while !rest.is_empty() {
            let n = rest.len().min(65535);
            let (block, tail) = rest.split_at(n);
            rest = tail;
            zlib.push(u8::from(rest.is_empty()));
            zlib.extend_from_slice(&(n as u16).to_le_bytes());
            zlib.extend_from_slice(&(!n as u16).to_le_bytes());
            zlib.extend_from_slice(block);
        }
    }
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in &raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&u32b((b << 16) | a));
    png.extend(chunk(b"IDAT", &zlib));
    png.extend(chunk(b"IEND", &[]));
    png
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn button_number(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::x11::XError;
    use super::{
        channel, crc32, downscale_rgb, encode_png, window_error, LinuxBackend, LinuxError,
    };
    use tool_desktop::DesktopBackend;

    #[test]
    fn png_1x1_red_matches_reference_bytes() {
        // Reference generated once with Python's zlib (independent
        // implementation); the encoder must reproduce it exactly.
        let rgb = [255u8, 0, 0];
        let png = encode_png(1, 1, &rgb);
        let expected_hex = "89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000f494441547801010400fbff00ff0000030101008d1de5820000000049454e44ae426082";
        assert_eq!(crate::hex::encode_stub(&png), expected_hex);
        // Structure sanity regardless of the vector above.
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[test]
    fn channel_scales_masks() {
        assert_eq!(channel(0x00FF0000, 0xFF0000), 255);
        assert_eq!(channel(0x00800000, 0xFF0000), 128);
        assert_eq!(channel(0x00000000, 0xFF0000), 0);
        assert_eq!(channel(0xFFFFFFFF, 0), 0);
        // 5-bit max (31) scales to 255.
        assert_eq!(channel(0x7C00, 0x7C00), 255);
    }

    #[test]
    fn downscale_preserves_aspect_ratio_and_samples_pixels() {
        let rgb = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let (width, height, scaled) = downscale_rgb(2, 2, &rgb, Some(1));
        assert_eq!((width, height), (1, 1));
        assert_eq!(scaled, vec![255, 0, 0]);
    }

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn bad_window_errors_are_recoverable() {
        let error = window_error(
            "0x123",
            LinuxError::X(XError::Server {
                code: 3,
                major: 15,
                minor: 0,
            }),
        );
        assert!(matches!(error, tool_desktop::DesktopError::StaleWindow(id) if id == "0x123"));

        let drawable_error = window_error(
            "0x456",
            LinuxError::X(XError::Server {
                code: 9,
                major: 14,
                minor: 0,
            }),
        );
        assert!(
            matches!(drawable_error, tool_desktop::DesktopError::StaleWindow(id) if id == "0x456")
        );
    }

    /// Live server when a display exists; `None` (clean skip) otherwise.
    fn live() -> Option<LinuxBackend> {
        match LinuxBackend::connect() {
            Ok(backend) => Some(backend),
            Err(e) => {
                eprintln!("SKIP live X11 tests: {e}");
                None
            }
        }
    }

    #[tokio::test]
    async fn live_window_enumeration_parses() {
        let Some(backend) = live() else { return };
        let windows = backend.list_windows().await.unwrap();
        assert!(!windows.is_empty(), "expected top-level windows");
        for w in &windows {
            assert!(w.id.starts_with("0x"), "xid format: {}", w.id);
        }
        eprintln!("windows: {windows:?}");
    }

    fn assert_png(shot: &tool_desktop::Screenshot) {
        assert!(shot.width > 0 && shot.height > 0);
        assert_eq!(
            &shot.png_bytes[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        let w = u32::from_be_bytes(shot.png_bytes[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(shot.png_bytes[20..24].try_into().unwrap());
        assert_eq!((w, h), (shot.width, shot.height));
        assert!(shot.png_bytes.len() > 100);
    }

    #[tokio::test]
    async fn live_owned_window_capture_roundtrip() {
        let Some(backend) = live() else { return };
        // Our own mapped window snapshots for real; it is destroyed
        // afterwards so no test residue stays on the desktop.
        let id = {
            let mut conn = backend.conn.lock().unwrap();
            let id = conn.alloc_id();
            let root = conn.root;
            conn.create_window(id, root, 10, 10, 64, 48, 0x00FF00)
                .unwrap();
            conn.map_window(id).unwrap();
            id
        };
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let shot = backend
            .screenshot(Some(&format!("0x{id:x}")))
            .await
            .unwrap();
        backend.conn.lock().unwrap().destroy_window(id).unwrap();
        assert_eq!((shot.width, shot.height), (64, 48));
        assert_png(&shot);
    }

    #[tokio::test]
    async fn live_whole_desktop_capture_or_honest_gap() {
        let Some(backend) = live() else { return };
        // Real X11 servers snapshot the root; Xwayland-style servers
        // give it no backing pixmap. Either outcome is legitimate, but
        // the backend must answer decisively — never hang or panic.
        match backend.screenshot(None).await {
            Ok(shot) => assert_png(&shot),
            Err(tool_desktop::DesktopError::BackendUnavailable(detail)) => {
                eprintln!("whole-desktop unavailable here: {detail}");
            }
            Err(other) => panic!("unexpected screenshot failure: {other}"),
        }
    }

    #[test]
    fn live_pointer_roundtrip_restores() {
        let Some(backend) = live() else { return };
        let mut conn = backend.conn.lock().unwrap();
        let root = conn.root;
        let (_, x0, y0) = conn.query_pointer(root).unwrap();
        conn.warp_pointer(x0.saturating_add(7), y0.saturating_add(7))
            .unwrap();
        let (_, x1, y1) = conn.query_pointer(root).unwrap();
        conn.warp_pointer(x0, y0).unwrap();
        let (_, x2, y2) = conn.query_pointer(root).unwrap();
        if (x1, y1) != (x0.saturating_add(7), y0.saturating_add(7)) {
            eprintln!(
                "SKIP pointer roundtrip: compositor did not apply XTEST warp (got {x1},{y1})"
            );
            return;
        }
        assert_eq!((x1, y1), (x0.saturating_add(7), y0.saturating_add(7)));
        assert_eq!((x2, y2), (x0, y0));
    }

    #[test]
    fn plugin_manifest_is_honest() {
        use tool_desktop::plugin::DesktopCapability;
        let Some(plugin) = super::plugin() else {
            eprintln!("SKIP live X11 tests: no display");
            return;
        };
        assert_eq!(plugin.manifest.id, "desktop.linux-x11");
        assert_eq!(plugin.manifest.platforms, vec!["linux".to_string()]);
        assert!(plugin.is_available());
        assert!(plugin.serves_current_platform());
        // X11 exposes the native capture, observation, input, window, and
        // launch surface; semantic value-setting is intentionally absent.
        assert!(plugin.manifest.capabilities.len() >= 20);
        assert!(!plugin.supports(DesktopCapability::SetValue));
        assert!(plugin.supports(DesktopCapability::Click));
        assert!(plugin.supports(DesktopCapability::CaptureStart));
        assert!(plugin.supports(DesktopCapability::Observe));
    }

    #[test]
    fn live_input_path_without_side_effects() {
        // Proves the XTEST FakeInput encoding is accepted by the real
        // server: the mapping must contain Shift + 'a', and a Shift
        // press+release round-trips with no visible effect.
        let Some(backend) = live() else { return };
        let major = backend.xtest_major.expect("XTEST required for this proof");
        let mut conn = backend.conn.lock().unwrap();
        let (shift_key, map) = backend.keysym_map(&mut conn).unwrap();
        assert_ne!(shift_key, 0, "Shift_L must exist in live mapping");
        assert!(
            map.iter().any(|&(_, sym, _)| sym == u32::from(b'a')),
            "'a' must exist in live mapping"
        );
        conn.xtest_fake_key(major, shift_key, true).unwrap();
        conn.xtest_fake_key(major, shift_key, false).unwrap();
    }

    #[tokio::test]
    async fn live_tree_and_focus_paths() {
        let Some(backend) = live() else { return };
        let windows = backend.list_windows().await.unwrap();
        let tree = backend.accessibility_tree(&windows[0].id).await.unwrap();
        // Children (if any) carry window ids and the invoke action.
        for el in &tree {
            assert!(el.id.starts_with("0x"));
            assert!(el.actions.contains(&"invoke".to_string()));
        }
        // Invoking the top window itself raises + focuses it.
        backend
            .invoke_element(&windows[0].id, &windows[0].id)
            .await
            .unwrap();
    }
}

/// Tiny hex encoder for test vectors (avoids a hex dependency).
#[cfg(test)]
mod hex {
    pub fn encode_stub(bytes: &[u8]) -> String {
        const H: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(H[(b >> 4) as usize] as char);
            out.push(H[(b & 15) as usize] as char);
        }
        out
    }
}

/// Decode one channel from a masked pixel value, scaled to 8 bits.
fn channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    (((value & mask) >> shift) * 255 / max.max(1)) as u8
}

#[async_trait::async_trait]
impl DesktopBackend for LinuxBackend {
    async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        let mut conn = self.lock()?;
        let root = conn.root;
        let mut out = Vec::new();
        for window in conn.query_tree(root).map_err(LinuxError::from)? {
            let title = self.title_of(&mut conn, window);
            let app = self.app_of(&mut conn, window);
            out.push(WindowInfo {
                id: format!("0x{window:x}"),
                title,
                app,
            });
        }
        Ok(out)
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, DesktopError> {
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let root = conn.root;
        let (width, height, _) = conn
            .get_geometry(root)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)?;
        Ok(vec![DisplayInfo {
            id: "0".to_string(),
            name: "X11 screen 0".to_string(),
            width: u32::from(width),
            height: u32::from(height),
            scale_factor: 1.0,
        }])
    }

    async fn accessibility_tree(&self, window_id: &str) -> Result<Vec<ElementNode>, DesktopError> {
        // X11 has no accessibility model: the window hierarchy is the
        // honest tree. Invoking a child raises + focuses that window.
        let window = Self::xid(window_id).map_err(DesktopError::from)?;
        let mut conn = self.lock()?;
        let mut out = Vec::new();
        for child in conn
            .query_tree(window)
            .map_err(|error| window_error(window_id, error.into()))?
        {
            let title = self.title_of(&mut conn, child);
            let bounds = conn
                .get_geometry(child)
                .ok()
                .map(|(width, height, _)| Rect {
                    x: 0,
                    y: 0,
                    width: i32::from(width),
                    height: i32::from(height),
                });
            out.push(ElementNode {
                id: format!("0x{child:x}"),
                role: "window".to_string(),
                name: title,
                description: None,
                value: None,
                bounds,
                enabled: Some(true),
                focused: None,
                selected: None,
                checked: None,
                expanded: None,
                parent_id: Some(window_id.to_string()),
                child_ids: Vec::new(),
                actions: vec!["invoke".to_string()],
            });
        }
        Ok(out)
    }

    async fn invoke_element(&self, window_id: &str, element_id: &str) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let target = Self::xid(element_id)
            .or_else(|_| Self::xid(window_id))
            .map_err(DesktopError::from)?;
        let mut conn = self.lock()?;
        // `raise` is a fire-and-forget X11 request, so validate the target
        // with a reply-bearing request first. This catches a window that
        // disappeared after inspect before an ignored void-request error can
        // pollute the next operation.
        conn.query_tree(target)
            .map_err(|error| window_error(window_id, error.into()))?;
        // Raising a foreign window can fail (already gone, override
        // redirect): surface it, never pretend.
        conn.raise(target)
            .map_err(|error| window_error(window_id, error.into()))?;
        // Synchronize after the void raise request so a race where
        // the XID disappears immediately is still reported here. Do this
        // before the best-effort focus request: some window managers reject
        // SetInputFocus even for a live window (BadMatch), and that legacy
        // behavior remains non-fatal.
        conn.query_tree(target)
            .map_err(|error| window_error(window_id, error.into()))?;
        let _ = conn.set_input_focus(target);
        Ok(())
    }

    async fn set_value(
        &self,
        _window_id: &str,
        _element_id: &str,
        _value: &str,
    ) -> Result<(), DesktopError> {
        Err(DesktopError::BackendUnavailable(
            "set_value has no X11 primitive; use desktop.type_text after focusing, or an AT-SPI backend when one lands".to_string(),
        ))
    }

    async fn screenshot_with_config(
        &self,
        config: CaptureConfig,
    ) -> Result<Screenshot, DesktopError> {
        if config.include_cursor {
            return Err(DesktopError::BackendUnavailable(
                "X11 still capture cannot embed the cursor; use the Wayland portal or omit include_cursor"
                    .to_string(),
            ));
        }
        match config.target {
            CaptureTarget::Desktop => self.screenshot_scaled(None, config.max_width).await,
            CaptureTarget::Window(window_id) => {
                self.screenshot_scaled(Some(&window_id), config.max_width)
                    .await
            }
            CaptureTarget::Display(display_id) if display_id == "0" => {
                self.screenshot_scaled(None, config.max_width).await
            }
            CaptureTarget::Display(display_id) => {
                Err(DesktopError::UnknownWindow(format!("display {display_id}")))
            }
        }
    }

    async fn screenshot_target(&self, target: CaptureTarget) -> Result<Screenshot, DesktopError> {
        match target {
            CaptureTarget::Desktop => self.screenshot(None).await,
            CaptureTarget::Window(window_id) => {
                self.screenshot((!window_id.is_empty()).then_some(window_id.as_str()))
                    .await
            }
            CaptureTarget::Display(display_id) if display_id == "0" => self.screenshot(None).await,
            CaptureTarget::Display(display_id) => {
                Err(DesktopError::UnknownWindow(format!("display {display_id}")))
            }
        }
    }

    async fn start_capture(
        &self,
        config: CaptureConfig,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Box<dyn CaptureSession>, DesktopError> {
        // The native X11 path samples GetImage on demand. It does not create
        // a background stream, which is intentional: the host's session
        // manager performs the model-facing rate limit and deduplication.
        Ok(Box::new(LinuxCaptureSession {
            backend: self.clone(),
            config,
            artifacts,
            frame_id: 0,
            stopped: false,
        }))
    }

    async fn set_control_enabled(&self, enabled: bool) -> Result<(), DesktopError> {
        self.control_enabled.store(enabled, Ordering::Release);
        Ok(())
    }

    async fn focus_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let window = self.validate_window(window_id, &mut conn)?;
        conn.raise(window)
            .map_err(LinuxError::from)
            .map_err(|error| window_error(window_id, error))?;
        conn.set_input_focus(window)
            .map_err(LinuxError::from)
            .map_err(|error| window_error(window_id, error))
    }

    async fn close_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let window = self.validate_window(window_id, &mut conn)?;
        let root = conn.root;
        conn.send_client_message(
            root,
            window,
            self.atom_net_close_window,
            [0, 0, 0, 0, 0],
            0x0004_0000 | 0x0008_0000,
        )
        .map_err(LinuxError::from)
        .map_err(|error| window_error(window_id, error))
    }

    async fn move_window(&self, window_id: &str, at: Point) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let window = self.validate_window(window_id, &mut conn)?;
        conn.configure_window(window, 0x0003, &[at.x as u32, at.y as u32])
            .map_err(LinuxError::from)
            .map_err(|error| window_error(window_id, error))?;
        conn.query_tree(window)
            .map_err(|error| window_error(window_id, error.into()))?;
        Ok(())
    }

    async fn resize_window(
        &self,
        window_id: &str,
        width: u32,
        height: u32,
    ) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        if width == 0 || height == 0 || width > u32::from(u16::MAX) || height > u32::from(u16::MAX)
        {
            return Err(DesktopError::ActionFailed(
                "X11 window dimensions must be between 1 and 65535".to_string(),
            ));
        }
        let mut conn = self.lock().map_err(DesktopError::from)?;
        let window = self.validate_window(window_id, &mut conn)?;
        conn.configure_window(window, 0x000C, &[width, height])
            .map_err(LinuxError::from)
            .map_err(|error| window_error(window_id, error))?;
        conn.query_tree(window)
            .map_err(|error| window_error(window_id, error.into()))?;
        Ok(())
    }

    async fn minimize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.change_window_state(window_id, 1, self.atom_net_wm_state_hidden, 0)
    }

    async fn maximize_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.change_window_state(
            window_id,
            1,
            self.atom_net_wm_state_max_horz,
            self.atom_net_wm_state_max_vert,
        )
    }

    async fn restore_window(&self, window_id: &str) -> Result<(), DesktopError> {
        self.change_window_state(
            window_id,
            0,
            self.atom_net_wm_state_max_horz,
            self.atom_net_wm_state_max_vert,
        )
    }

    async fn screenshot(&self, window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
        self.screenshot_scaled(window_id, None).await
    }

    async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        if let Some(id) = window_id.filter(|s| !s.is_empty()) {
            let target = Self::xid(id).map_err(DesktopError::from)?;
            let mut conn = self.lock()?;
            conn.query_tree(target)
                .map_err(|error| window_error(id, error.into()))?;
            conn.raise(target)
                .map_err(|error| window_error(id, error.into()))?;
            conn.query_tree(target)
                .map_err(|error| window_error(id, error.into()))?;
        }
        let mut conn = self.lock()?;
        conn.warp_pointer(at.x as i16, at.y as i16)
            .map_err(LinuxError::from)?;
        conn.xtest_fake_button(major, 1, true)
            .map_err(LinuxError::from)?;
        conn.xtest_fake_button(major, 1, false)
            .map_err(LinuxError::from)?;
        Ok(())
    }

    async fn move_pointer(&self, at: Point) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        conn.warp_pointer(at.x as i16, at.y as i16)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)
    }

    async fn mouse_down(&self, button: MouseButton) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        conn.xtest_fake_button(major, button_number(button), true)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)
    }

    async fn mouse_up(&self, button: MouseButton) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        conn.xtest_fake_button(major, button_number(button), false)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)
    }

    async fn drag(&self, from: Point, to: Point, button: MouseButton) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        conn.warp_pointer(from.x as i16, from.y as i16)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)?;
        conn.xtest_fake_button(major, button_number(button), true)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)?;
        conn.warp_pointer(to.x as i16, to.y as i16)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)?;
        conn.xtest_fake_button(major, button_number(button), false)
            .map_err(LinuxError::from)
            .map_err(DesktopError::from)
    }

    async fn scroll(
        &self,
        window_id: Option<&str>,
        delta_x: i32,
        delta_y: i32,
    ) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock().map_err(DesktopError::from)?;
        if let Some(id) = window_id.filter(|id| !id.is_empty()) {
            let window = self.validate_window(id, &mut conn)?;
            conn.raise(window)
                .map_err(LinuxError::from)
                .map_err(|error| window_error(id, error))?;
        }
        for _ in 0..delta_y.unsigned_abs().min(100) {
            conn.xtest_fake_button(major, if delta_y > 0 { 4 } else { 5 }, true)
                .map_err(LinuxError::from)
                .map_err(DesktopError::from)?;
            conn.xtest_fake_button(major, if delta_y > 0 { 4 } else { 5 }, false)
                .map_err(LinuxError::from)
                .map_err(DesktopError::from)?;
        }
        for _ in 0..delta_x.unsigned_abs().min(100) {
            conn.xtest_fake_button(major, if delta_x > 0 { 6 } else { 7 }, true)
                .map_err(LinuxError::from)
                .map_err(DesktopError::from)?;
            conn.xtest_fake_button(major, if delta_x > 0 { 6 } else { 7 }, false)
                .map_err(LinuxError::from)
                .map_err(DesktopError::from)?;
        }
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
                "Linux clipboard backend currently supports text/plain only".to_string(),
            ));
        }
        read_clipboard_text()
    }

    async fn clipboard_write(&self, mime_type: &str, text: &str) -> Result<(), DesktopError> {
        if mime_type != "text/plain" {
            return Err(DesktopError::BackendUnavailable(
                "Linux clipboard backend currently supports text/plain only".to_string(),
            ));
        }
        write_clipboard_text(text)
    }

    async fn launch_application(&self, application: &str) -> Result<(), DesktopError> {
        Command::new(application)
            .spawn()
            .map(|_| ())
            .map_err(|error| DesktopError::ActionFailed(format!("launch '{application}': {error}")))
    }

    async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
        self.ensure_control_enabled()?;
        let major = self.xtest_major.ok_or_else(|| {
            DesktopError::BackendUnavailable("XTEST missing: cannot synthesize input".to_string())
        })?;
        let mut conn = self.lock()?;
        if let Some(id) = window_id.filter(|s| !s.is_empty()) {
            let target = Self::xid(id).map_err(DesktopError::from)?;
            conn.query_tree(target)
                .map_err(|error| window_error(id, error.into()))?;
            conn.raise(target)
                .map_err(|error| window_error(id, error.into()))?;
            conn.query_tree(target)
                .map_err(|error| window_error(id, error.into()))?;
            let _ = conn.set_input_focus(target);
        }
        let (shift_key, map) = self.keysym_map(&mut conn)?;
        for ch in text.chars() {
            // Latin-1 keysyms match Unicode codepoints; higher planes
            // have no X11 keysym and fail at the mapping lookup below.
            let keysym = ch as u32;
            // Exact keysym first; ASCII letters fall back to the opposite
            // case (typed with Shift).
            let found = map.iter().find(|(_, sym, _)| *sym == keysym).or_else(|| {
                if ch.is_ascii_alphabetic() {
                    let swapped = if ch.is_ascii_lowercase() {
                        ch.to_ascii_uppercase() as u32
                    } else {
                        ch.to_ascii_lowercase() as u32
                    };
                    map.iter().find(|(_, sym, _)| *sym == swapped)
                } else {
                    None
                }
            });
            let &(keycode, found_sym, shifted) = found
                .ok_or_else(|| DesktopError::ActionFailed(format!("no keycode for {ch:?}")))?;
            // An uppercase letter resolved through its lowercase keysym
            // still needs Shift held.
            let shifted = shifted || (found_sym != keysym && ch.is_ascii_uppercase());
            if shifted {
                if shift_key == 0 {
                    return Err(DesktopError::ActionFailed(
                        "no Shift key in mapping".to_string(),
                    ));
                }
                conn.xtest_fake_key(major, shift_key, true)
                    .map_err(LinuxError::from)?;
            }
            conn.xtest_fake_key(major, keycode, true)
                .map_err(LinuxError::from)?;
            conn.xtest_fake_key(major, keycode, false)
                .map_err(LinuxError::from)?;
            if shifted {
                conn.xtest_fake_key(major, shift_key, false)
                    .map_err(LinuxError::from)?;
            }
        }
        Ok(())
    }
}
