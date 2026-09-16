//! Native Linux Wayland desktop backend.
//!
//! Screen visibility is obtained through the XDG portals: still captures
//! use the Screenshot portal (one D-Bus round trip, no streaming), while
//! video sessions use the ScreenCast portal with the selected PipeWire
//! node consumed in a dedicated native capture thread.
//! Semantic accessibility prefers native AT-SPI2/D-Bus and falls back to
//! Xwayland only for non-AT-SPI ids; Xwayland is never used to infer that
//! a portal screen-sharing approval granted control.

#[cfg(target_os = "linux")]
mod linux {
    use artifact_core::ArtifactStore;
    use ashpd::desktop::{
        remote_desktop::{DeviceType, KeyState, RemoteDesktop},
        screencast::{CursorMode, Screencast, SourceType, Streams},
        PersistMode, Session,
    };
    use async_trait::async_trait;
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
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
    /// Approvals persist: the restore token the compositor issues is kept in
    /// the OS keychain, so repeat captures restore the approved source
    /// without another dialog until the user revokes the grant.
    /// The AT-SPI service is supplied by the host so semantic availability is
    /// discovered once and shared with the standalone semantic plugin.
    pub fn plugin_with_atspi(
        atspi_service: desktop_linux_atspi::live::AtspiService,
    ) -> Option<DesktopPlugin> {
        std::env::var_os("WAYLAND_DISPLAY")?;

        let x11_plugin = desktop_linux::plugin();
        let x11 = x11_plugin.as_ref().map(|plugin| plugin.backend.clone());
        // Native AT-SPI semantics win over Xwayland inference whenever the
        // registry answers; X11 stays as the legacy fallback. Ids carry
        // their backend (`atspi://…` vs hex), so routing stays exact.
        let atspi_plugin = desktop_linux_atspi::plugin_with_service(atspi_service);
        let atspi = Some(atspi_plugin.backend.clone());
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
        if x11_plugin.is_some() {
            capabilities.push(DesktopCapability::ListDisplays);
            if let Some(x11_plugin) = &x11_plugin {
                for capability in &x11_plugin.manifest.capabilities {
                    if !capabilities.contains(capability) {
                        capabilities.push(*capability);
                    }
                }
            }
        }
        for capability in &atspi_plugin.manifest.capabilities {
            if !capabilities.contains(capability) {
                capabilities.push(*capability);
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
                atspi,
                remote: Arc::new(tokio::sync::Mutex::new(None)),
                control_enabled: Arc::new(AtomicBool::new(true)),
                capture_secrets: Arc::new(Mutex::new(None)),
            }),
        ))
    }

    /// Compatibility constructor. The host should prefer
    /// [`plugin_with_atspi`] to avoid duplicate AT-SPI discovery.
    pub fn plugin() -> Option<DesktopPlugin> {
        plugin_with_atspi(desktop_linux_atspi::live::AtspiService::new())
    }

    struct PortalRemoteDesktop {
        portal: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
    }

    #[derive(Clone)]
    struct WaylandBackend {
        x11: Option<Arc<dyn DesktopBackend>>,
        /// Native semantic backend, preferred over Xwayland below.
        atspi: Option<Arc<dyn DesktopBackend>>,
        remote: Arc<tokio::sync::Mutex<Option<PortalRemoteDesktop>>>,
        control_enabled: Arc<AtomicBool>,
        /// Lazily created secret store for the ScreenCast restore token.
        /// The OS keychain is touched only when a capture actually
        /// starts, never at plugin discovery.
        capture_secrets: Arc<Mutex<Option<Arc<dyn secret_core::SecretStore>>>>,
    }

    impl WaylandBackend {
        fn no_x11(operation: &str) -> DesktopError {
            DesktopError::BackendUnavailable(format!(
                "{operation} has no native Wayland implementation in this session"
            ))
        }

        /// Shared secret store for the ScreenCast restore token, created
        /// on first capture. `secret_core::system` resolves the OS
        /// keychain (Secret Service on Linux) and falls back to
        /// process memory where no keychain exists; either way the
        /// token survives at least for the life of this backend.
        ///
        /// The blocking keychain probe runs on the blocking pool: the
        /// Linux secret-service client pumps D-Bus through its own
        /// tokio `block_on`, which panics on an async worker and would
        /// silently downgrade every capture to a memory-only store.
        async fn capture_secret_store(&self) -> Arc<dyn secret_core::SecretStore> {
            {
                let slot = self
                    .capture_secrets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(store) = slot.as_ref() {
                    return Arc::clone(store);
                }
            }
            let store = tokio::task::spawn_blocking(|| secret_core::system("utsuwa"))
                .await
                .unwrap_or_else(|_| {
                    Arc::new(secret_core::MemoryStore::default())
                        as Arc<dyn secret_core::SecretStore>
                });
            let mut slot = self
                .capture_secrets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // A concurrent capture may have initialized first; prefer the
            // winner so both agree on one store instance.
            if let Some(existing) = slot.as_ref() {
                return Arc::clone(existing);
            }
            *slot = Some(Arc::clone(&store));
            store
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
            let secrets = self.capture_secret_store().await;
            let capture = PortalCaptureSession::start(config, artifacts, Some(secrets)).await?;
            Ok(Box::new(capture))
        }

        async fn one_shot_portal(&self, config: CaptureConfig) -> Result<Screenshot, DesktopError> {
            // Still captures prefer the Screenshot portal: one D-Bus round
            // trip, no PipeWire streaming negotiation, and a natively
            // compressed PNG. Streaming (ScreenCast + PipeWire) stays as
            // the fallback for compositors without the Screenshot portal
            // and as the only path for video sessions (`portal_capture`).
            match self.screenshot_via_portal().await {
                Ok(shot) => {
                    // The Screenshot portal cannot resize: honor an
                    // explicit max_width by falling through to the
                    // PipeWire path, which negotiates a smaller format.
                    // Without a resize request the portal still is final.
                    let needs_resize = config.max_width.is_some_and(|max| shot.width > max);
                    if !needs_resize {
                        return Ok(shot);
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "Screenshot portal still failed; trying PipeWire");
                }
            }
            let store: Arc<dyn ArtifactStore> =
                Arc::new(artifact_core::InMemoryArtifactStore::new());
            let secrets = self.capture_secret_store().await;
            let mut session =
                PortalCaptureSession::start(config, store.clone(), Some(secrets)).await?;
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

        /// One still image through `org.freedesktop.portal.Screenshot`.
        /// Non-interactive and non-modal: the compositor captures without
        /// a source picker. Bounded like the ScreenCast negotiation so an
        /// unanswered OS dialog fails the tool call with an actionable
        /// message instead of hanging the agent turn. (The wait happens
        /// inside `send()`: ashpd joins the portal Response signal there,
        /// while the sync `response()` only takes the arrived value.)
        async fn screenshot_via_portal(&self) -> Result<Screenshot, DesktopError> {
            use ashpd::desktop::screenshot::Screenshot as PortalScreenshot;
            let send = PortalScreenshot::request()
                .interactive(false)
                .modal(false)
                .send();
            let request = match tokio::time::timeout(PORTAL_APPROVAL_TIMEOUT, send).await {
                Ok(Ok(request)) => request,
                Ok(Err(error)) => {
                    return Err(DesktopError::BackendUnavailable(format!(
                        "Screenshot portal request: {error}"
                    )));
                }
                Err(_) => {
                    return Err(DesktopError::ActionFailed(format!(
                        "screenshot approval timed out after {}s: answer the OS screenshot dialog (or decline it) and try again",
                        PORTAL_APPROVAL_TIMEOUT.as_secs(),
                    )));
                }
            };
            let response = request.response().map_err(|error| {
                DesktopError::BackendUnavailable(format!("Screenshot portal capture: {error}"))
            })?;
            let path = response.uri().to_file_path().map_err(|_| {
                DesktopError::BackendUnavailable(format!(
                    "Screenshot portal returned a non-file URI: {}",
                    response.uri()
                ))
            })?;
            let bytes = tokio::fs::read(&path).await.map_err(|error| {
                DesktopError::ActionFailed(format!("read portal screenshot file: {error}"))
            })?;
            // Best-effort cleanup of the portal's temp file.
            let _ = tokio::fs::remove_file(&path).await;
            let (width, height) = parse_png_dimensions(&bytes)?;
            Ok(Screenshot {
                width,
                height,
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

        /// Semantic backend for one id: `atspi://…` ids always go native,
        /// everything else goes to Xwayland. Native wins only for its own
        /// ids — never by guessing.
        fn semantic_for(
            &self,
            operation: &str,
            id: &str,
        ) -> Result<Arc<dyn DesktopBackend>, DesktopError> {
            if id.starts_with("atspi://") {
                return self.atspi.clone().ok_or_else(|| {
                    DesktopError::BackendUnavailable(format!(
                        "{operation} targets an AT-SPI object but the registry is unreachable"
                    ))
                });
            }
            self.x11.clone().ok_or_else(|| Self::no_x11(operation))
        }
    }

    #[async_trait]
    impl DesktopBackend for WaylandBackend {
        fn is_available(&self) -> bool {
            std::env::var_os("WAYLAND_DISPLAY").is_some()
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
            // Native first, Xwayland legacy second. Id schemes keep the two
            // worlds apart downstream, so concatenation cannot alias.
            let mut windows = Vec::new();
            if let Some(atspi) = &self.atspi {
                windows.extend(atspi.list_windows().await?);
            }
            if let Some(x11) = &self.x11 {
                windows.extend(x11.list_windows().await?);
            }
            if self.atspi.is_none() && self.x11.is_none() {
                return Err(Self::no_x11("window enumeration"));
            }
            Ok(windows)
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
            let backend = self.semantic_for("accessibility tree", window_id)?;
            backend.accessibility_tree(window_id).await
        }

        async fn accessibility_snapshot(
            &self,
            window_id: &str,
            since: Option<&str>,
        ) -> Result<AccessibilitySnapshot, DesktopError> {
            let backend = self.semantic_for("accessibility snapshot", window_id)?;
            backend.accessibility_snapshot(window_id, since).await
        }

        async fn invoke_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("element invocation", element_id)?;
            backend.invoke_element(window_id, element_id).await
        }

        async fn focus_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("element focus", element_id)?;
            backend.focus_element(window_id, element_id).await
        }

        async fn set_value(
            &self,
            window_id: &str,
            element_id: &str,
            value: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("semantic value setting", element_id)?;
            backend.set_value(window_id, element_id, value).await
        }

        async fn select_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("semantic selection", element_id)?;
            backend.select_element(window_id, element_id).await
        }

        async fn expand_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("semantic expansion", element_id)?;
            backend.expand_element(window_id, element_id).await
        }

        async fn collapse_element(
            &self,
            window_id: &str,
            element_id: &str,
        ) -> Result<(), DesktopError> {
            self.ensure_control_enabled()?;
            let backend = self.semantic_for("semantic collapse", element_id)?;
            backend.collapse_element(window_id, element_id).await
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
            let backend = self.semantic_for("window focus", window_id)?;
            backend.focus_window(window_id).await
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

    /// Latest-frame slot shared between the PipeWire worker thread and the
    /// async consumer. The producer overwrites unconditionally, so a slow
    /// model round trip can never leave the consumer reading stale queued
    /// frames: `take` always returns the newest frame produced so far (or
    /// waits for the next one). A FIFO here would hold the oldest
    /// unconsumed frames while the worker dropped every newer one.
    #[derive(Debug, Default)]
    struct LatestFrameSlot {
        state: Mutex<SlotState>,
    }

    #[derive(Debug, Default)]
    struct SlotState {
        frame: Option<RawFrame>,
        /// Take-once worker error. A transient fault surfaces on one
        /// `take` and clears; the consumer keeps waiting for the next
        /// good frame instead of failing every call afterwards.
        error: Option<String>,
    }

    enum SlotTake {
        Frame(RawFrame),
        Error(String),
        Empty,
    }

    impl LatestFrameSlot {
        fn store(&self, frame: RawFrame) {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| {
                // A panicking producer must not wedge capture forever.
                poisoned.into_inner()
            });
            state.frame = Some(frame);
        }

        fn report_error(&self, message: String) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.error = Some(message);
        }

        /// Take the newest frame, else a pending worker error, else `Empty`.
        /// Taking removes the frame so a second immediate call waits for a
        /// genuinely new capture instead of re-reading the same pixels.
        fn take(&self) -> SlotTake {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(error) = state.error.take() {
                return SlotTake::Error(error);
            }
            match state.frame.take() {
                Some(frame) => SlotTake::Frame(frame),
                None => SlotTake::Empty,
            }
        }
    }

    struct PortalCaptureSession {
        artifacts: Arc<dyn ArtifactStore>,
        config: CaptureConfig,
        slot: Arc<LatestFrameSlot>,
        stop_sender: Option<pw::channel::Sender<()>>,
        worker: Option<JoinHandle<()>>,
        portal_session: Option<Session<'static, Screencast<'static>>>,
        frame_id: u64,
        stopped: bool,
    }

    /// Best-effort restore-token persistence. Every keychain failure
    /// degrades to the pre-token behavior (the OS dialog appears); a
    /// broken or locked secret store must never break screen capture.
    /// Each call runs on the blocking pool for the same reason as
    /// [`WaylandBackend::capture_secret_store`]: the sync secret API
    /// would panic on an async worker.
    async fn load_restore_token(store: &Arc<dyn secret_core::SecretStore>) -> Option<String> {
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            store
                .get(secret_core::ACCOUNT_PORTAL_RESTORE_TOKEN)
                .ok()
                .flatten()
                .filter(|token| !token.is_empty())
        })
        .await
        .ok()
        .flatten()
    }

    async fn store_restore_token(store: &Arc<dyn secret_core::SecretStore>, token: &str) {
        if token.is_empty() {
            return;
        }
        let store = Arc::clone(store);
        let token = token.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            store.set(secret_core::ACCOUNT_PORTAL_RESTORE_TOKEN, &token)
        })
        .await;
    }

    async fn clear_restore_token(store: &Arc<dyn secret_core::SecretStore>) {
        let store = Arc::clone(store);
        let _ = tokio::task::spawn_blocking(move || {
            store.delete(secret_core::ACCOUNT_PORTAL_RESTORE_TOKEN)
        })
        .await;
    }

    /// One source-selection plus approval round trip against the
    /// ScreenCast portal. With a valid restore token the compositor
    /// re-selects the previously approved source silently; without one
    /// (or with a stale one the compositor rejects) the OS dialog
    /// appears. `ExplicitlyRevoked` asks the compositor to keep the
    /// grant until the user revokes it in system settings.
    async fn negotiate_source(
        proxy: &Screencast<'static>,
        session: &Session<'static, Screencast<'static>>,
        cursor: CursorMode,
        restore_token: Option<&str>,
    ) -> Result<Streams, DesktopError> {
        proxy
            .select_sources(
                session,
                cursor,
                SourceType::Monitor | SourceType::Window,
                false,
                restore_token,
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!("select portal source: {error}"))
            })?
            .response()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!("portal source selection: {error}"))
            })?;
        proxy
            .start(session, None)
            .await
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!("start portal capture: {error}"))
            })?
            .response()
            .map_err(|error| {
                DesktopError::BackendUnavailable(format!("portal capture approval: {error}"))
            })
    }

    impl PortalCaptureSession {
        async fn start(
            config: CaptureConfig,
            artifacts: Arc<dyn ArtifactStore>,
            secrets: Option<Arc<dyn secret_core::SecretStore>>,
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
            // The first attempt presents the stored restore token, if any,
            // so repeat captures skip the OS dialog. A stale or revoked
            // token fails the attempt; it is dropped and the negotiation
            // retries once as a fresh interactive selection, which shows
            // the dialog again. Both attempts wait on a dialog the user
            // may never answer, so the whole negotiation stays bounded:
            // an unanswered dialog fails the tool call with an actionable
            // error instead of hanging the agent turn forever.
            let restore_token = match secrets.as_ref() {
                Some(store) => load_restore_token(store).await,
                None => None,
            };
            let negotiation = async {
                match negotiate_source(&proxy, &session, cursor, restore_token.as_deref()).await {
                    Ok(streams) => Ok(streams),
                    Err(_) if restore_token.is_some() => {
                        if let Some(store) = secrets.as_ref() {
                            clear_restore_token(store).await;
                        }
                        negotiate_source(&proxy, &session, cursor, None).await
                    }
                    Err(first) => Err(first),
                }
            };
            let response = match tokio::time::timeout(PORTAL_APPROVAL_TIMEOUT, negotiation).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    let _ = session.close().await;
                    return Err(DesktopError::ActionFailed(format!(
                        "portal capture approval timed out after {}s: answer the OS screen-share dialog (or decline it) and try again",
                        PORTAL_APPROVAL_TIMEOUT.as_secs(),
                    )));
                }
            };
            // Persist the fresh grant before consuming the stream: the next
            // capture restores silently. Compositors that do not implement
            // restore return no token, which keeps the old ask-every-time
            // behavior without any special casing here.
            if let (Some(store), Some(token)) = (secrets.as_ref(), response.restore_token()) {
                store_restore_token(store, token).await;
            }
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

            let slot = Arc::new(LatestFrameSlot::default());
            let (stop_sender, stop_receiver) = pw::channel::channel();
            let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
            let max_width = config.max_width;
            let max_fps = config.max_fps.clamp(1, 60);
            let worker_slot = Arc::clone(&slot);
            let worker = std::thread::Builder::new()
                .name("utsuwa-wayland-capture".to_string())
                .spawn(move || {
                    run_pipewire(
                        node_id,
                        fd,
                        max_width,
                        max_fps,
                        worker_slot,
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
                slot,
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
                match self.slot.take() {
                    // The slot always holds the newest produced frame, so a
                    // slow consumer observes current screen state, never a
                    // stale head-of-queue frame.
                    SlotTake::Frame(frame) => break frame,
                    SlotTake::Error(error) => return Err(DesktopError::BackendUnavailable(error)),
                    SlotTake::Empty => {
                        if self
                            .worker
                            .as_ref()
                            .is_some_and(|worker| worker.is_finished())
                        {
                            return Err(DesktopError::BackendUnavailable(
                                "PipeWire capture stream ended".to_string(),
                            ));
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return Err(DesktopError::ActionFailed(
                                "PipeWire capture produced no frame within 5 seconds; the OS screen-share grant may have selected an empty source, PipeWire may not be running, or the compositor negotiated a pixel format the client cannot map (see the host log for stream errors)".to_string(),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(8)).await;
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
        slot: Arc<LatestFrameSlot>,
        /// Consecutive buffers the client could not map (typically
        /// DMA-BUF). A lone blip is skipped; a persistent run is
        /// reported so the consumer fails fast instead of timing out.
        unmapped_streak: u32,
    }

    /// Buffers the client may fail to map before the worker reports an
    /// error. At 30fps this is ~1s of consecutive failures.
    const MAX_UNMAPPED_STREAK: u32 = 30;

    /// Bound for the whole portal select+start negotiation, both steps of
    /// which wait on the OS screen-share dialog. Generous for a human
    /// answering the dialog; bounded so an agent turn cannot hang forever
    /// on an unanswered prompt.
    const PORTAL_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

    fn run_pipewire(
        node_id: u32,
        fd: std::os::fd::OwnedFd,
        max_width: Option<u32>,
        max_fps: u32,
        slot: Arc<LatestFrameSlot>,
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
                    slot: Arc::clone(&slot),
                    unmapped_streak: 0,
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
                        // Persistent: every buffer carries the same format,
                        // so every consumer call fails explicitly instead
                        // of timing out on an empty slot.
                        user_data.slot.report_error(format!(
                            "unsupported PipeWire pixel format: {format:?}"
                        ));
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
                        // DMA-BUF (or otherwise unmappable) buffer. Skip lone
                        // blips; report a persistent run so the consumer
                        // fails fast with a cause instead of a bare timeout.
                        user_data.unmapped_streak =
                            user_data.unmapped_streak.saturating_add(1);
                        if user_data.unmapped_streak >= MAX_UNMAPPED_STREAK {
                            let buffer_type = data.type_();
                            user_data.slot.report_error(format!(
                                "PipeWire delivered {buffer_type:?} video buffers the client cannot map"
                            ));
                        }
                        return;
                    };
                    user_data.unmapped_streak = 0;
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
                    user_data.slot.store(RawFrame {
                        width,
                        height,
                        rgb,
                    });
                })
                .state_changed(|_, user_data, _old, new| {
                    // Surface negotiation/stream failures with their cause:
                    // without this the consumer only sees the 5s "no frame"
                    // timeout even when PipeWire already knows the reason.
                    if let pw::stream::StreamState::Error(message) = new {
                        user_data.slot.report_error(format!(
                            "PipeWire stream error: {message}"
                        ));
                    }
                })
                .register()
                .map_err(|error| error.to_string())?;
            let _stop = stop_receiver.attach(mainloop.loop_(), {
                let mainloop = mainloop.clone();
                move |_| mainloop.quit()
            });
            let format_values = video_params(max_width, max_fps);
            let format_pod = pw::spa::pod::Pod::from_bytes(&format_values)
                .ok_or_else(|| "invalid serialized PipeWire format pod".to_string())?;
            let buffer_values = buffers_params();
            let buffer_pod = pw::spa::pod::Pod::from_bytes(&buffer_values)
                .ok_or_else(|| "invalid serialized PipeWire buffers pod".to_string())?;
            let mut params = [format_pod, buffer_pod];
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
            slot.report_error(error);
        }
    }

    /// `SPA_PARAM_BUFFERS_dataType` from `spa/param/buffers.h`: "possible
    /// memory types", a flags-choice Int mask of `spa_data_type` bits.
    /// libspa 0.10 exposes no typed Buffers-properties enum, so the stable
    /// SPA ABI value is spelled out (START=0, buffers=1, blocks=2, size=3,
    /// stride=4, align=5, dataType=6).
    const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;

    /// Buffers param requesting CPU-readable memory. The worker only
    /// understands mapped buffers, so DMA-BUF-only negotiation would
    /// surface as unmapped buffers at capture time; PipeWire converts
    /// from the source as needed to satisfy this mask.
    fn buffers_param_object() -> spa::pod::Object {
        // Bits are (1 << spa_data_type), per spa/param/buffers.h.
        let memfd = 1i32 << spa::buffer::DataType::MemFd.as_raw();
        let memptr = 1i32 << spa::buffer::DataType::MemPtr.as_raw();
        spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![spa::pod::Property {
                key: SPA_PARAM_BUFFERS_DATA_TYPE,
                flags: spa::pod::PropertyFlags::empty(),
                value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Flags {
                        default: memfd | memptr,
                        flags: vec![memfd, memptr],
                    },
                ))),
            }],
        }
    }

    fn buffers_params() -> Vec<u8> {
        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(buffers_param_object()),
        )
        .expect("PipeWire buffers serialization")
        .0
        .into_inner()
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

    /// Dimensions of a PNG without a decoder: signature plus the IHDR
    /// width/height (big-endian `u32` at bytes 16..24). The Screenshot
    /// portal hands back an encoded file, and stills need no pixel
    /// access — only the size header for the `Screenshot` value.
    fn parse_png_dimensions(bytes: &[u8]) -> Result<(u32, u32), DesktopError> {
        const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
        if bytes.len() < 33 || bytes[..8] != SIGNATURE[..] || &bytes[12..16] != b"IHDR" {
            return Err(DesktopError::ActionFailed(
                "Screenshot portal returned a file that is not a PNG image".to_string(),
            ));
        }
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        if width == 0 || height == 0 {
            return Err(DesktopError::ActionFailed(
                "Screenshot portal returned a PNG with zero width or height".to_string(),
            ));
        }
        Ok((width, height))
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn raw(width: u32) -> RawFrame {
            RawFrame {
                width,
                height: 1,
                rgb: vec![0; width as usize * 3],
            }
        }

        #[test]
        fn slot_returns_newest_frame_not_oldest_queued() {
            let slot = LatestFrameSlot::default();
            assert!(matches!(slot.take(), SlotTake::Empty));
            // A burst of producer frames with no consumer in between: only
            // the newest survives, so a slow model round trip observes
            // current screen state.
            slot.store(raw(1));
            slot.store(raw(2));
            slot.store(raw(3));
            match slot.take() {
                SlotTake::Frame(frame) => assert_eq!(frame.width, 3),
                _ => panic!("expected the newest frame"),
            }
            // Taking removes the frame: an immediate second call waits for
            // a genuinely new capture instead of re-reading pixels.
            assert!(matches!(slot.take(), SlotTake::Empty));
        }

        #[test]
        fn buffers_param_requests_cpu_readable_memory() {
            let object = buffers_param_object();
            assert_eq!(object.id, spa::param::ParamType::Buffers.as_raw());
            assert_eq!(object.properties.len(), 1);
            let property = &object.properties[0];
            assert_eq!(property.key, SPA_PARAM_BUFFERS_DATA_TYPE);
            match &property.value {
                spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(choice)) => match &choice.1 {
                    spa::utils::ChoiceEnum::Flags { default, flags } => {
                        let memfd = 1i32 << spa::buffer::DataType::MemFd.as_raw();
                        let memptr = 1i32 << spa::buffer::DataType::MemPtr.as_raw();
                        let dmabuf = 1i32 << spa::buffer::DataType::DmaBuf.as_raw();
                        assert_eq!(*default, memfd | memptr);
                        assert!(flags.contains(&memfd) && flags.contains(&memptr));
                        assert_eq!(*default & dmabuf, 0, "DMA-BUF must stay out of the mask");
                    }
                    _ => panic!("dataType must be a flags choice"),
                },
                _ => panic!("dataType must be an Int choice"),
            }
            // And it serializes to a parseable pod for stream.connect.
            let bytes = buffers_params();
            assert!(!bytes.is_empty());
            assert!(spa::pod::Pod::from_bytes(&bytes).is_some());
        }

        #[test]
        fn slot_error_is_take_once_and_prioritized() {
            let slot = LatestFrameSlot::default();
            slot.report_error("unmapped".to_string());
            slot.store(raw(9));
            match slot.take() {
                SlotTake::Error(message) => assert_eq!(message, "unmapped"),
                _ => panic!("expected the worker error first"),
            }
            // The error cleared; the frame that arrived alongside it is
            // still consumable.
            match slot.take() {
                SlotTake::Frame(frame) => assert_eq!(frame.width, 9),
                _ => panic!("expected the frame after the error cleared"),
            }
            assert!(matches!(slot.take(), SlotTake::Empty));
        }

        fn memory_secrets() -> Arc<dyn secret_core::SecretStore> {
            Arc::new(secret_core::MemoryStore::default())
        }

        fn minimal_png(width: u32, height: u32) -> Vec<u8> {
            let mut bytes = vec![
                0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, // signature
                0, 0, 0, 13, // IHDR length
                b'I', b'H', b'D', b'R', // IHDR kind
            ];
            bytes.extend_from_slice(&width.to_be_bytes());
            bytes.extend_from_slice(&height.to_be_bytes());
            bytes.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit truecolor
            bytes.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
            bytes
        }

        #[test]
        fn portal_png_dimensions_come_from_ihdr() {
            assert_eq!(
                parse_png_dimensions(&minimal_png(1920, 1080)).unwrap(),
                (1920, 1080)
            );
            assert_eq!(parse_png_dimensions(&minimal_png(1, 1)).unwrap(), (1, 1));
        }

        #[test]
        fn portal_png_dimensions_reject_non_png() {
            assert!(parse_png_dimensions(&[]).is_err());
            assert!(parse_png_dimensions(b"definitely not a png").is_err());
            let mut truncated = minimal_png(800, 600);
            truncated.truncate(20);
            assert!(parse_png_dimensions(&truncated).is_err());
            // Zero-size images are corrupt captures, not screenshots.
            assert!(parse_png_dimensions(&minimal_png(0, 600)).is_err());
            assert!(parse_png_dimensions(&minimal_png(800, 0)).is_err());
            // Wrong chunk kind where IHDR belongs.
            let mut bad_kind = minimal_png(800, 600);
            bad_kind[12..16].copy_from_slice(b"IDAT");
            assert!(parse_png_dimensions(&bad_kind).is_err());
        }

        #[tokio::test]
        async fn restore_token_round_trip_through_secret_store() {
            let store = memory_secrets();
            assert_eq!(load_restore_token(&store).await, None);
            store_restore_token(&store, "token-1").await;
            assert_eq!(load_restore_token(&store).await.as_deref(), Some("token-1"));
            store_restore_token(&store, "token-2").await;
            assert_eq!(load_restore_token(&store).await.as_deref(), Some("token-2"));
            clear_restore_token(&store).await;
            assert_eq!(load_restore_token(&store).await, None);
        }

        #[tokio::test]
        async fn restore_token_helpers_ignore_empty_and_broken_backends() {
            struct FailingStore;
            impl secret_core::SecretStore for FailingStore {
                fn get(&self, _account: &str) -> Result<Option<String>, secret_core::SecretError> {
                    Err(secret_core::SecretError::Backend("locked".to_string()))
                }
                fn set(
                    &self,
                    _account: &str,
                    _secret: &str,
                ) -> Result<(), secret_core::SecretError> {
                    Err(secret_core::SecretError::Backend("locked".to_string()))
                }
                fn delete(&self, _account: &str) -> Result<(), secret_core::SecretError> {
                    Err(secret_core::SecretError::Backend("locked".to_string()))
                }
            }
            // Empty tokens are never persisted or returned: presenting an
            // empty restore token would be a corrupt negotiation.
            let store = memory_secrets();
            store_restore_token(&store, "").await;
            assert_eq!(load_restore_token(&store).await, None);
            // A locked or missing keychain degrades to ask-every-time; it
            // must never panic or poison the capture path.
            let broken: Arc<dyn secret_core::SecretStore> = Arc::new(FailingStore);
            assert_eq!(load_restore_token(&broken).await, None);
            store_restore_token(&broken, "token-1").await;
            clear_restore_token(&broken).await;
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{plugin, plugin_with_atspi};

#[cfg(not(target_os = "linux"))]
pub fn plugin() -> Option<tool_desktop::plugin::DesktopPlugin> {
    None
}
