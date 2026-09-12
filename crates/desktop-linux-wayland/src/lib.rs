//! Native Linux Wayland desktop backend.
//!
//! Screen visibility is obtained through the XDG ScreenCast portal and the
//! selected PipeWire node is consumed in a dedicated native capture thread.
//! Xwayland remains a separate fallback for window hierarchy and input; it is
//! never used to infer that a portal screen-sharing approval granted control.

#[cfg(target_os = "linux")]
mod linux {
    use artifact_core::ArtifactStore;
    use ashpd::desktop::{
        remote_desktop::{DeviceType, KeyState, RemoteDesktop},
        screencast::{CursorMode, Screencast, SourceType},
        PersistMode, Session,
    };
    use async_trait::async_trait;
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tool_desktop::{
        plugin::{DesktopCapability, DesktopPlugin, DesktopPluginManifest},
        AccessibilitySnapshot, CaptureConfig, CaptureSession, CaptureTarget, DesktopBackend,
        DesktopError, DisplayInfo, ElementNode, MouseButton, Point, Screenshot, VideoFrame,
        WindowInfo,
    };

    /// A portal-backed backend is meaningful only in a Wayland session. The
    /// actual portal is still probed when a capture session is started; a
    /// compositor is allowed to reject the request or require a fresh choice.
    pub fn plugin() -> Option<DesktopPlugin> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return None;
        }

        let x11_plugin = desktop_linux::plugin();
        let x11 = x11_plugin.as_ref().map(|plugin| plugin.backend.clone());
        let mut capabilities = vec![
            DesktopCapability::Screenshot,
            DesktopCapability::CaptureStart,
            DesktopCapability::CaptureFrame,
            DesktopCapability::CaptureStop,
            DesktopCapability::CaptureStatus,
            // RemoteDesktop supports these without requiring an Xwayland
            // server. Absolute pointer placement and window management still
            // require an explicit Xwayland fallback below.
            DesktopCapability::MouseDown,
            DesktopCapability::MouseUp,
            DesktopCapability::Scroll,
            DesktopCapability::TypeText,
            DesktopCapability::KeyDown,
            DesktopCapability::KeyUp,
            DesktopCapability::Hotkey,
            DesktopCapability::PressKey,
        ];
        if let Some(x11_plugin) = x11_plugin {
            capabilities.push(DesktopCapability::ListDisplays);
            for capability in x11_plugin.manifest.capabilities {
                if !capabilities.contains(&capability) {
                    capabilities.push(capability);
                }
            }
        }

        Some(DesktopPlugin::new(
            DesktopPluginManifest {
                id: "desktop.linux-portal".to_string(),
                name: "Linux Wayland portal backend".to_string(),
                version: "0.1.0".to_string(),
                platforms: vec!["linux".to_string()],
                capabilities,
                description: "XDG ScreenCast portal plus PipeWire for user-selected Wayland capture; Xwayland is used only as an explicitly separate fallback for legacy windows and input.".to_string(),
            },
            Arc::new(WaylandBackend {
                x11,
                remote: Arc::new(tokio::sync::Mutex::new(None)),
                control_enabled: Arc::new(AtomicBool::new(true)),
            }),
        ))
    }

    struct PortalRemoteDesktop {
        portal: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
    }

    #[derive(Clone)]
    struct WaylandBackend {
        x11: Option<Arc<dyn DesktopBackend>>,
        remote: Arc<tokio::sync::Mutex<Option<PortalRemoteDesktop>>>,
        control_enabled: Arc<AtomicBool>,
    }

    impl WaylandBackend {
        fn no_x11(operation: &str) -> DesktopError {
            DesktopError::BackendUnavailable(format!(
                "{operation} has no native Wayland implementation in this session"
            ))
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

        async fn start_remote_desktop() -> Result<PortalRemoteDesktop, DesktopError> {
            let portal: RemoteDesktop<'static> = RemoteDesktop::new().await.map_err(|error| {
                DesktopError::BackendUnavailable(format!("RemoteDesktop portal: {error}"))
            })?;
            let session: Session<'static, RemoteDesktop<'static>> = portal
                .create_session()
                .await
                .map_err(|error| {
                DesktopError::BackendUnavailable(format!("create RemoteDesktop session: {error}"))
            })?;
            portal
                .select_devices(
                    &session,
                    DeviceType::Keyboard | DeviceType::Pointer,
                    None,
                    PersistMode::DoNot,
                )
                .await
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!(
                        "select RemoteDesktop devices: {error}"
                    ))
                })?
                .response()
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!(
                        "RemoteDesktop device approval: {error}"
                    ))
                })?;
            portal
                .start(&session, None)
                .await
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!(
                        "start RemoteDesktop session: {error}"
                    ))
                })?
                .response()
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!(
                        "RemoteDesktop control approval: {error}"
                    ))
                })?;
            Ok(PortalRemoteDesktop { portal, session })
        }

        async fn ensure_remote_desktop(&self) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let mut remote = self.remote.lock().await;
            if remote.is_none() {
                *remote = Some(Self::start_remote_desktop().await?);
            }
            Ok(())
        }

        fn keysym(key: &str) -> Result<i32, DesktopError> {
            let trimmed = key.trim();
            if trimmed.chars().count() == 1 {
                let value = trimmed.chars().next().unwrap() as u32;
                return i32::try_from(value).map_err(|_| {
                    DesktopError::ActionFailed(format!(
                        "key '{key}' is outside the portal keysym range"
                    ))
                });
            }
            let symbol = match trimmed.to_ascii_uppercase().as_str() {
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
                    return Err(DesktopError::ActionFailed(format!(
                        "unknown key '{other}'; use a single character or a named key"
                    )))
                }
            };
            Ok(symbol)
        }

        fn pointer_button(button: MouseButton) -> i32 {
            // Linux evdev button codes used by XDP.
            match button {
                MouseButton::Left => 0x110,
                MouseButton::Right => 0x111,
                MouseButton::Middle => 0x112,
            }
        }

        async fn remote_key(&self, key: &str, state: KeyState) -> Result<(), DesktopError> {
            self.ensure_remote_desktop().await?;
            let keysym = Self::keysym(key)?;
            let remote = self.remote.lock().await;
            let remote = remote.as_ref().ok_or_else(|| {
                DesktopError::BackendUnavailable("RemoteDesktop session is not active".to_string())
            })?;
            remote
                .portal
                .notify_keyboard_keysym(&remote.session, keysym, state)
                .await
                .map_err(|error| {
                    DesktopError::ActionFailed(format!("RemoteDesktop keyboard event: {error}"))
                })
        }

        async fn remote_type_text(&self, text: &str) -> Result<(), DesktopError> {
            self.ensure_remote_desktop().await?;
            let remote = self.remote.lock().await;
            let remote = remote.as_ref().ok_or_else(|| {
                DesktopError::BackendUnavailable("RemoteDesktop session is not active".to_string())
            })?;
            for ch in text.chars() {
                let keysym = Self::keysym(&ch.to_string())?;
                remote
                    .portal
                    .notify_keyboard_keysym(&remote.session, keysym, KeyState::Pressed)
                    .await
                    .map_err(|error| {
                        DesktopError::ActionFailed(format!("RemoteDesktop text key press: {error}"))
                    })?;
                remote
                    .portal
                    .notify_keyboard_keysym(&remote.session, keysym, KeyState::Released)
                    .await
                    .map_err(|error| {
                        DesktopError::ActionFailed(format!(
                            "RemoteDesktop text key release: {error}"
                        ))
                    })?;
            }
            Ok(())
        }

        async fn portal_capture(
            &self,
            config: CaptureConfig,
            artifacts: Arc<dyn ArtifactStore>,
        ) -> Result<Box<dyn CaptureSession>, DesktopError> {
            if !matches!(config.target, CaptureTarget::Desktop) {
                return Err(DesktopError::BackendUnavailable(
                    "the Wayland portal owns display/window selection; start a Desktop target and choose the exact source in the OS dialog, or use an Xwayland window id explicitly".to_string(),
                ));
            }
            let capture = PortalCaptureSession::start(config, artifacts).await?;
            Ok(Box::new(capture))
        }

        async fn one_shot_portal(&self, config: CaptureConfig) -> Result<Screenshot, DesktopError> {
            let store: Arc<dyn ArtifactStore> =
                Arc::new(artifact_core::InMemoryArtifactStore::new());
            let mut session = PortalCaptureSession::start(config, store.clone()).await?;
            let frame = session.next_frame().await?;
            let bytes = store
                .get(&frame.image.artifact.id)
                .await
                .map_err(|error| DesktopError::ActionFailed(error.to_string()))?;
            session.stop().await?;
            Ok(Screenshot {
                width: frame.width,
                height: frame.height,
                png_bytes: bytes,
            })
        }

        async fn x11<F, Fut, T>(&self, operation: &str, call: F) -> Result<T, DesktopError>
        where
            F: FnOnce(Arc<dyn DesktopBackend>) -> Fut,
            Fut: std::future::Future<Output = Result<T, DesktopError>>,
        {
            let backend = self.x11.clone().ok_or_else(|| Self::no_x11(operation))?;
            call(backend).await
        }
    }

    #[async_trait]
    impl DesktopBackend for WaylandBackend {
        fn is_available(&self) -> bool {
            std::env::var_os("WAYLAND_DISPLAY").is_some()
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
            self.x11("window enumeration", |backend| async move {
                backend.list_windows().await
            })
            .await
        }

        async fn list_displays(&self) -> Result<Vec<DisplayInfo>, DesktopError> {
            if let Some(backend) = &self.x11 {
                return backend.list_displays().await;
            }
            Err(Self::no_x11("display enumeration"))
        }

        async fn accessibility_tree(
            &self,
            window_id: &str,
        ) -> Result<Vec<ElementNode>, DesktopError> {
            self.x11("accessibility tree", |backend| {
                let window_id = window_id.to_string();
                async move { backend.accessibility_tree(&window_id).await }
            })
            .await
        }

        async fn accessibility_snapshot(
            &self,
            window_id: &str,
            since: Option<&str>,
        ) -> Result<AccessibilitySnapshot, DesktopError> {
            self.x11("accessibility snapshot", |backend| {
                let window_id = window_id.to_string();
                let since = since.map(str::to_string);
                async move {
                    backend
                        .accessibility_snapshot(&window_id, since.as_deref())
                        .await
                }
            })
            .await
        }

        async fn invoke_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("element invocation", |backend| {
                let window_id = window_id.to_string();
                let element_id = element_id.to_string();
                async move { backend.invoke_element(&window_id, &element_id).await }
            })
            .await
        }

        async fn set_value(
            &self,
            window_id: &str,
            element_id: &str,
            value: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("semantic value setting", |backend| {
                let window_id = window_id.to_string();
                let element_id = element_id.to_string();
                let value = value.to_string();
                async move { backend.set_value(&window_id, &element_id, &value).await }
            })
            .await
        }

        async fn screenshot(&self, window_id: Option<&str>) -> Result<Screenshot, DesktopError> {
            match window_id.filter(|id| !id.is_empty()) {
                Some(window_id) => {
                    self.x11("window screenshot", |backend| {
                        let window_id = window_id.to_string();
                        async move { backend.screenshot(Some(&window_id)).await }
                    })
                    .await
                }
                None => self.one_shot_portal(CaptureConfig::default()).await,
            }
        }

        async fn screenshot_with_config(
            &self,
            config: CaptureConfig,
        ) -> Result<Screenshot, DesktopError> {
            match config.target {
                CaptureTarget::Desktop => self.one_shot_portal(config).await,
                CaptureTarget::Display(_) | CaptureTarget::Window(_) => {
                    if let Some(backend) = &self.x11 {
                        backend.screenshot_with_config(config).await
                    } else {
                        Err(DesktopError::BackendUnavailable(
                            "Wayland still capture target must be selected in the ScreenCast portal; no Xwayland fallback is available for this explicit target".to_string(),
                        ))
                    }
                }
            }
        }

        async fn screenshot_target(
            &self,
            target: CaptureTarget,
        ) -> Result<Screenshot, DesktopError> {
            match target {
                CaptureTarget::Desktop => self.one_shot_portal(CaptureConfig::default()).await,
                CaptureTarget::Window(window_id) => self.screenshot(Some(&window_id)).await,
                CaptureTarget::Display(display_id) => {
                    if let Some(backend) = &self.x11 {
                        backend
                            .screenshot_target(CaptureTarget::Display(display_id))
                            .await
                    } else {
                        Err(DesktopError::BackendUnavailable(
                            "Wayland portal display selection is user-controlled; start a Desktop target and choose the exact display in the OS dialog".to_string(),
                        ))
                    }
                }
            }
        }

        async fn start_capture(
            &self,
            config: CaptureConfig,
            artifacts: Arc<dyn ArtifactStore>,
        ) -> Result<Box<dyn CaptureSession>, DesktopError> {
            match config.target {
                CaptureTarget::Desktop => self.portal_capture(config, artifacts).await,
                CaptureTarget::Display(_) | CaptureTarget::Window(_) => {
                    if let Some(backend) = &self.x11 {
                        backend.start_capture(config, artifacts).await
                    } else {
                        self.portal_capture(config, artifacts).await
                    }
                }
            }
        }

        async fn set_control_enabled(&self, enabled: bool) -> Result<(), DesktopError> {
            self.control_enabled.store(enabled, Ordering::Release);
            if !enabled {
                if let Some(remote) = self.remote.lock().await.take() {
                    remote.session.close().await.map_err(|error| {
                        DesktopError::ActionFailed(format!("close RemoteDesktop session: {error}"))
                    })?;
                }
            }
            if let Some(backend) = &self.x11 {
                backend.set_control_enabled(enabled).await?;
            }
            Ok(())
        }

        async fn focus_window(&self, window_id: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window focus", |backend| {
                let window_id = window_id.to_string();
                async move { backend.focus_window(&window_id).await }
            })
            .await
        }

        async fn close_window(&self, window_id: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window close", |backend| {
                let window_id = window_id.to_string();
                async move { backend.close_window(&window_id).await }
            })
            .await
        }

        async fn move_window(&self, window_id: &str, at: Point) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window move", |backend| {
                let window_id = window_id.to_string();
                async move { backend.move_window(&window_id, at).await }
            })
            .await
        }

        async fn resize_window(
            &self,
            window_id: &str,
            width: u32,
            height: u32,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window resize", |backend| {
                let window_id = window_id.to_string();
                async move { backend.resize_window(&window_id, width, height).await }
            })
            .await
        }

        async fn minimize_window(&self, window_id: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window minimize", |backend| {
                let window_id = window_id.to_string();
                async move { backend.minimize_window(&window_id).await }
            })
            .await
        }

        async fn maximize_window(&self, window_id: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window maximize", |backend| {
                let window_id = window_id.to_string();
                async move { backend.maximize_window(&window_id).await }
            })
            .await
        }

        async fn restore_window(&self, window_id: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("window restore", |backend| {
                let window_id = window_id.to_string();
                async move { backend.restore_window(&window_id).await }
            })
            .await
        }

        async fn click(&self, window_id: Option<&str>, at: Point) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let window_id = window_id.map(str::to_string);
            self.x11("pointer click", |backend| async move {
                backend.click(window_id.as_deref(), at).await
            })
            .await
        }

        async fn move_pointer(&self, at: Point) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("pointer motion", |backend| async move {
                backend.move_pointer(at).await
            })
            .await
        }

        async fn mouse_down(&self, button: MouseButton) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                return backend.mouse_down(button).await;
            }
            self.ensure_remote_desktop().await?;
            let remote = self.remote.lock().await;
            let remote = remote.as_ref().ok_or_else(|| Self::no_x11("mouse down"))?;
            remote
                .portal
                .notify_pointer_button(
                    &remote.session,
                    Self::pointer_button(button),
                    KeyState::Pressed,
                )
                .await
                .map_err(|error| {
                    DesktopError::ActionFailed(format!("RemoteDesktop mouse down: {error}"))
                })
        }

        async fn mouse_up(&self, button: MouseButton) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                return backend.mouse_up(button).await;
            }
            self.ensure_remote_desktop().await?;
            let remote = self.remote.lock().await;
            let remote = remote.as_ref().ok_or_else(|| Self::no_x11("mouse up"))?;
            remote
                .portal
                .notify_pointer_button(
                    &remote.session,
                    Self::pointer_button(button),
                    KeyState::Released,
                )
                .await
                .map_err(|error| {
                    DesktopError::ActionFailed(format!("RemoteDesktop mouse up: {error}"))
                })
        }

        async fn drag(
            &self,
            from: Point,
            to: Point,
            button: MouseButton,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            self.x11("pointer drag", |backend| async move {
                backend.drag(from, to, button).await
            })
            .await
        }

        async fn scroll(
            &self,
            window_id: Option<&str>,
            delta_x: i32,
            delta_y: i32,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                let window_id = window_id.map(str::to_string);
                return backend.scroll(window_id.as_deref(), delta_x, delta_y).await;
            }
            self.ensure_remote_desktop().await?;
            let remote = self.remote.lock().await;
            let remote = remote.as_ref().ok_or_else(|| Self::no_x11("scroll"))?;
            remote
                .portal
                .notify_pointer_axis(&remote.session, delta_x as f64, delta_y as f64, true)
                .await
                .map_err(|error| {
                    DesktopError::ActionFailed(format!("RemoteDesktop scroll: {error}"))
                })
        }

        async fn type_text(&self, window_id: Option<&str>, text: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                let window_id = window_id.map(str::to_string);
                let text = text.to_string();
                return backend.type_text(window_id.as_deref(), &text).await;
            }
            if window_id.is_some_and(|id| !id.is_empty()) {
                return Err(Self::no_x11("window-scoped text input"));
            }
            self.remote_type_text(text).await
        }

        async fn key_down(&self, key: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                return backend.key_down(key).await;
            }
            self.remote_key(key, KeyState::Pressed).await
        }

        async fn key_up(&self, key: &str) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            if let Some(backend) = &self.x11 {
                return backend.key_up(key).await;
            }
            self.remote_key(key, KeyState::Released).await
        }

        async fn clipboard_read(&self, mime_type: &str) -> Result<String, DesktopError> {
            let mime_type = mime_type.to_string();
            self.x11("clipboard read", |backend| async move {
                backend.clipboard_read(&mime_type).await
            })
            .await
        }

        async fn clipboard_write(&self, mime_type: &str, text: &str) -> Result<(), DesktopError> {
            let mime_type = mime_type.to_string();
            let text = text.to_string();
            self.x11("clipboard write", |backend| async move {
                backend.clipboard_write(&mime_type, &text).await
            })
            .await
        }

        async fn launch_application(&self, application: &str) -> Result<(), DesktopError> {
            let application = application.to_string();
            self.x11("application launch", |backend| async move {
                backend.launch_application(&application).await
            })
            .await
        }
    }

    #[derive(Debug)]
    struct RawFrame {
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    }

    enum FrameMessage {
        Frame(RawFrame),
        Error(String),
    }

    struct PortalCaptureSession {
        artifacts: Arc<dyn ArtifactStore>,
        config: CaptureConfig,
        frames: mpsc::Receiver<FrameMessage>,
        stop_sender: Option<pw::channel::Sender<()>>,
        worker: Option<JoinHandle<()>>,
        portal_session: Option<Session<'static, Screencast<'static>>>,
        frame_id: u64,
        stopped: bool,
    }

    impl PortalCaptureSession {
        async fn start(
            config: CaptureConfig,
            artifacts: Arc<dyn ArtifactStore>,
        ) -> Result<Self, DesktopError> {
            let proxy: Screencast<'static> = Screencast::new().await.map_err(|error| {
                DesktopError::BackendUnavailable(format!("ScreenCast portal: {error}"))
            })?;
            let session: Session<'static, Screencast<'static>> =
                proxy.create_session().await.map_err(|error| {
                    DesktopError::BackendUnavailable(format!("create portal session: {error}"))
                })?;
            let cursor = if config.include_cursor {
                CursorMode::Embedded
            } else {
                CursorMode::Hidden
            };
            proxy
                .select_sources(
                    &session,
                    cursor,
                    (SourceType::Monitor | SourceType::Window).into(),
                    false,
                    None,
                    PersistMode::DoNot,
                )
                .await
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!("select portal source: {error}"))
                })?
                .response()
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!("portal source selection: {error}"))
                })?;
            let response = proxy
                .start(&session, None)
                .await
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!("start portal capture: {error}"))
                })?
                .response()
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!("portal capture approval: {error}"))
                })?;
            let stream = response.streams().first().ok_or_else(|| {
                DesktopError::BackendUnavailable("portal returned no capture stream".to_string())
            })?;
            let node_id = stream.pipe_wire_node_id();
            let fd = proxy
                .open_pipe_wire_remote(&session)
                .await
                .map_err(|error| {
                    DesktopError::BackendUnavailable(format!("open PipeWire remote: {error}"))
                })?;

            let (frame_sender, frames) = mpsc::sync_channel(3);
            let (stop_sender, stop_receiver) = pw::channel::channel();
            let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
            let max_width = config.max_width;
            let max_fps = config.max_fps.clamp(1, 60);
            let worker = std::thread::Builder::new()
                .name("utsuwa-wayland-capture".to_string())
                .spawn(move || {
                    run_pipewire(
                        node_id,
                        fd,
                        max_width,
                        max_fps,
                        frame_sender,
                        stop_receiver,
                        ready_sender,
                    )
                })
                .map_err(|error| {
                    DesktopError::ActionFailed(format!("start PipeWire thread: {error}"))
                })?;

            match tokio::time::timeout(Duration::from_secs(10), ready_receiver).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    let _ = stop_sender.send(());
                    let _ = worker.join();
                    let _ = session.close().await;
                    return Err(DesktopError::BackendUnavailable(error));
                }
                Ok(Err(_)) | Err(_) => {
                    let _ = stop_sender.send(());
                    let _ = worker.join();
                    let _ = session.close().await;
                    return Err(DesktopError::BackendUnavailable(
                        "PipeWire capture thread did not initialize".to_string(),
                    ));
                }
            }

            Ok(Self {
                artifacts,
                config,
                frames,
                stop_sender: Some(stop_sender),
                worker: Some(worker),
                portal_session: Some(session),
                frame_id: 0,
                stopped: false,
            })
        }
    }

    #[async_trait]
    impl CaptureSession for PortalCaptureSession {
        async fn next_frame(&mut self) -> Result<VideoFrame, DesktopError> {
            if self.stopped {
                return Err(DesktopError::BackendUnavailable(
                    "capture session is stopped".to_string(),
                ));
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let raw = loop {
                match self.frames.try_recv() {
                    Ok(FrameMessage::Frame(frame)) => break frame,
                    Ok(FrameMessage::Error(error)) => {
                        return Err(DesktopError::BackendUnavailable(error))
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        if tokio::time::Instant::now() >= deadline {
                            return Err(DesktopError::ActionFailed(
                                "PipeWire capture produced no frame within 5 seconds".to_string(),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(8)).await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(DesktopError::BackendUnavailable(
                            "PipeWire capture stream ended".to_string(),
                        ))
                    }
                }
            };
            let (width, height, rgb) = downscale_rgb(raw, self.config.max_width);
            let png = encode_png(width, height, &rgb);
            let artifact = self
                .artifacts
                .put("image/png", png)
                .await
                .map_err(|error| DesktopError::ActionFailed(format!("artifact store: {error}")))?;
            self.frame_id = self.frame_id.saturating_add(1);
            Ok(VideoFrame {
                frame_id: self.frame_id,
                timestamp_ms: now_ms(),
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
            if let Some(sender) = self.stop_sender.take() {
                let _ = sender.send(());
            }
            if let Some(worker) = self.worker.take() {
                tokio::task::spawn_blocking(move || worker.join())
                    .await
                    .map_err(|error| {
                        DesktopError::ActionFailed(format!("join PipeWire thread: {error}"))
                    })?
                    .map_err(|_| {
                        DesktopError::ActionFailed("PipeWire thread panicked".to_string())
                    })?;
            }
            if let Some(session) = self.portal_session.take() {
                session.close().await.map_err(|error| {
                    DesktopError::ActionFailed(format!("close portal session: {error}"))
                })?;
            }
            Ok(())
        }
    }

    struct PipewireUserData {
        format: spa::param::video::VideoInfoRaw,
        sender: mpsc::SyncSender<FrameMessage>,
        error_reported: bool,
    }

    fn run_pipewire(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        max_width: Option<u32>,
        max_fps: u32,
        sender: mpsc::SyncSender<FrameMessage>,
        stop_receiver: pw::channel::Receiver<()>,
        ready_sender: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) {
        pw::init();
        let mut ready_sender = Some(ready_sender);
        let result = (|| -> Result<(), String> {
            let mainloop =
                pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
            let context =
                pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
            let core = context
                .connect_fd_rc(fd, None)
                .map_err(|error| error.to_string())?;
            let stream = pw::stream::StreamBox::new(
                &core,
                "utsuwa-wayland-capture",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Screen",
                },
            )
            .map_err(|error| error.to_string())?;
            let _listener = stream
                .add_local_listener_with_user_data(PipewireUserData {
                    format: Default::default(),
                    sender: sender.clone(),
                    error_reported: false,
                })
                .param_changed(|_, user_data, id, param| {
                    let Some(param) = param else { return };
                    if id != pw::spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let Ok((media_type, media_subtype)) =
                        pw::spa::param::format_utils::parse_format(param)
                    else {
                        return;
                    };
                    if media_type != pw::spa::param::format::MediaType::Video
                        || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                    {
                        return;
                    }
                    let _ = user_data.format.parse(param);
                })
                .process(|stream, user_data| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let datas = buffer.datas_mut();
                    let Some(data) = datas.first_mut() else {
                        return;
                    };
                    let format = user_data.format.format();
                    let Some(bytes_per_pixel) = bytes_per_pixel(format) else {
                        if !user_data.error_reported {
                            let _ = user_data.sender.try_send(FrameMessage::Error(format!(
                                "unsupported PipeWire pixel format: {format:?}"
                            )));
                            user_data.error_reported = true;
                        }
                        return;
                    };
                    let size = user_data.format.size();
                    let width = size.width;
                    let height = size.height;
                    if width == 0 || height == 0 {
                        return;
                    }
                    let stride = data.chunk().stride();
                    let stride_abs = if stride == 0 {
                        width as usize * bytes_per_pixel
                    } else {
                        stride.unsigned_abs() as usize
                    };
                    let offset = data.chunk().offset() as usize;
                    let size = data.chunk().size() as usize;
                    let Some(bytes) = data.data() else {
                        if !user_data.error_reported {
                            let _ = user_data.sender.try_send(FrameMessage::Error(
                                "PipeWire delivered an unmapped video buffer".to_string(),
                            ));
                            user_data.error_reported = true;
                        }
                        return;
                    };
                    let start = offset.min(bytes.len());
                    let end = start.saturating_add(size).min(bytes.len());
                    let raw = &bytes[start..end];
                    let Some(rgb) = convert_frame(
                        raw,
                        width,
                        height,
                        stride_abs,
                        stride < 0,
                        format,
                        bytes_per_pixel,
                    ) else {
                        return;
                    };
                    let _ = user_data.sender.try_send(FrameMessage::Frame(RawFrame {
                        width,
                        height,
                        rgb,
                    }));
                })
                .register()
                .map_err(|error| error.to_string())?;
            let _stop = stop_receiver.attach(mainloop.loop_(), {
                let mainloop = mainloop.clone();
                move |_| mainloop.quit()
            });
            let values = video_params(max_width, max_fps);
            let pod = pw::spa::pod::Pod::from_bytes(&values)
                .ok_or_else(|| "invalid serialized PipeWire format pod".to_string())?;
            let mut params = [pod];
            stream
                .connect(
                    spa::utils::Direction::Input,
                    Some(node_id),
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .map_err(|error| error.to_string())?;
            if let Some(sender) = ready_sender.take() {
                let _ = sender.send(Ok(()));
            }
            mainloop.run();
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(sender) = ready_sender.take() {
                let _ = sender.send(Err(error.clone()));
            }
            let _ = sender.try_send(FrameMessage::Error(error));
        }
    }

    fn video_params(max_width: Option<u32>, max_fps: u32) -> Vec<u8> {
        let max = max_width.unwrap_or(4096).clamp(1, 4096);
        let obj = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                pw::spa::param::video::VideoFormat::BGRx,
                pw::spa::param::video::VideoFormat::RGBx,
                pw::spa::param::video::VideoFormat::RGBA,
                pw::spa::param::video::VideoFormat::BGRA,
                pw::spa::param::video::VideoFormat::RGB,
                pw::spa::param::video::VideoFormat::BGR
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: max,
                    height: max
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: max,
                    height: max
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction {
                    num: max_fps,
                    denom: 1,
                },
                pw::spa::utils::Fraction { num: 1, denom: 1 },
                pw::spa::utils::Fraction {
                    num: max_fps,
                    denom: 1,
                }
            )
        );
        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .expect("PipeWire format serialization")
        .0
        .into_inner()
    }

    #[derive(Clone, Copy)]
    enum ChannelOrder {
        Rgb,
        Bgr,
    }

    fn bytes_per_pixel(format: spa::param::video::VideoFormat) -> Option<usize> {
        if matches!(
            format,
            spa::param::video::VideoFormat::RGB | spa::param::video::VideoFormat::BGR
        ) {
            Some(3)
        } else if matches!(
            format,
            spa::param::video::VideoFormat::RGBx
                | spa::param::video::VideoFormat::BGRx
                | spa::param::video::VideoFormat::RGBA
                | spa::param::video::VideoFormat::BGRA
        ) {
            Some(4)
        } else {
            None
        }
    }

    fn channel_order(format: spa::param::video::VideoFormat) -> ChannelOrder {
        match format {
            spa::param::video::VideoFormat::BGR
            | spa::param::video::VideoFormat::BGRx
            | spa::param::video::VideoFormat::BGRA => ChannelOrder::Bgr,
            _ => ChannelOrder::Rgb,
        }
    }

    fn convert_frame(
        raw: &[u8],
        width: u32,
        height: u32,
        stride: usize,
        bottom_up: bool,
        format: spa::param::video::VideoFormat,
        bytes_per_pixel: usize,
    ) -> Option<Vec<u8>> {
        let row_bytes = width as usize * bytes_per_pixel;
        if stride < row_bytes || raw.len() < stride.saturating_mul(height as usize) {
            return None;
        }
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        let order = channel_order(format);
        for row in 0..height as usize {
            let source_row = if bottom_up {
                height as usize - 1 - row
            } else {
                row
            };
            let source = &raw[source_row * stride..source_row * stride + row_bytes];
            for pixel in source.chunks_exact(bytes_per_pixel) {
                match order {
                    ChannelOrder::Rgb => rgb.extend_from_slice(&pixel[..3]),
                    ChannelOrder::Bgr => rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]),
                }
            }
        }
        Some(rgb)
    }

    fn downscale_rgb(frame: RawFrame, max_width: Option<u32>) -> (u32, u32, Vec<u8>) {
        let Some(max_width) = max_width.filter(|width| *width > 0) else {
            return (frame.width, frame.height, frame.rgb);
        };
        if frame.width <= max_width || frame.width == 0 || frame.height == 0 {
            return (frame.width, frame.height, frame.rgb);
        }
        let width = max_width;
        let height = ((frame.height as u64 * width as u64) / frame.width as u64)
            .max(1)
            .min(u32::MAX as u64) as u32;
        let mut rgb = vec![0u8; width as usize * height as usize * 3];
        for y in 0..height {
            let source_y = (y as u64 * frame.height as u64 / height as u64) as usize;
            for x in 0..width {
                let source_x = (x as u64 * frame.width as u64 / width as u64) as usize;
                let source = (source_y * frame.width as usize + source_x) * 3;
                let dest = (y as usize * width as usize + x as usize) * 3;
                rgb[dest..dest + 3].copy_from_slice(&frame.rgb[source..source + 3]);
            }
        }
        (width, height, rgb)
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
        fn u32b(value: u32) -> [u8; 4] {
            value.to_be_bytes()
        }
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(12 + data.len());
            out.extend_from_slice(&u32b(data.len() as u32));
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&u32b(crc32(&[kind.as_slice(), data].concat())));
            out
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&u32b(width));
        ihdr.extend_from_slice(&u32b(height));
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend(chunk(b"IHDR", &ihdr));
        let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
        for row in rgb.chunks(width as usize * 3) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut zlib = vec![0x78, 0x01];
        if raw.is_empty() {
            zlib.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
        } else {
            let mut rest = raw.as_slice();
            while !rest.is_empty() {
                let block_len = rest.len().min(65_535);
                let (block, tail) = rest.split_at(block_len);
                rest = tail;
                zlib.push(u8::from(rest.is_empty()));
                zlib.extend_from_slice(&(block_len as u16).to_le_bytes());
                zlib.extend_from_slice(&(!(block_len as u16)).to_le_bytes());
                zlib.extend_from_slice(block);
            }
        }
        let mut a = 1u32;
        let mut b = 0u32;
        for byte in &raw {
            a = (a + u32::from(*byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        zlib.extend_from_slice(&u32b((b << 16) | a));
        png.extend(chunk(b"IDAT", &zlib));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in data {
            crc ^= u32::from(*byte);
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
}

#[cfg(target_os = "linux")]
pub use linux::plugin;

#[cfg(not(target_os = "linux"))]
pub fn plugin() -> Option<tool_desktop::plugin::DesktopPlugin> {
    None
}
