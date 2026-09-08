//! Native desktop host: GTK-embedded wry WebView on Linux (X11 and
//! Wayland), winit event loop + wry WebView elsewhere, host→frontend events.
//!
//! Task 2 scope: workspace wiring only — the `WindowHost` trait, an
//! in-process channel implementation, and the winit event-loop constructor.
//! Creating the window/WebView and loading the frontend lands in Task 3/4.

use ipc_core::HostEvent;

pub mod agent_runtime;
pub mod dispatcher;
pub mod protocol;
pub use winit;

/// Errors surfaced by the host. No panics cross this boundary.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("event channel closed")]
    ChannelClosed,
    #[error("event loop error: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("webview error: {0}")]
    WebView(String),
}

/// Minimal window-host interface. The rest of the backend talks to the
/// frontend through this trait — never through raw window handles.
#[async_trait::async_trait]
pub trait WindowHost: Send + Sync {
    async fn emit(&self, event: HostEvent) -> Result<(), HostError>;
}

/// How the host should find the frontend.
#[derive(Debug, Clone)]
pub enum FrontendSource {
    /// `http://localhost:*` — development only.
    DevUrl(String),
    /// Bundled assets served by Rust over the custom scheme (Task 5).
    Bundled,
}

/// Host configuration, parsed from CLI/env in `src/main.rs` later.
#[derive(Debug, Clone)]
pub struct AppHostConfig {
    pub frontend: FrontendSource,
    pub app_version: String,
}

impl AppHostConfig {
    pub fn dev(app_version: impl Into<String>) -> Self {
        // `UTSUWA_DEV_URL` override exists so Task 4 can load a deep route
        // (e.g. `/app` for the VRM scene) without changing the default.
        // Dev URLs are http://localhost:* only — enforced by `is_dev_url`.
        let url = std::env::var("UTSUWA_DEV_URL")
            .ok()
            .filter(|u| is_dev_url(u))
            .unwrap_or_else(|| "http://localhost:5173".to_string());
        Self {
            frontend: FrontendSource::DevUrl(url),
            app_version: app_version.into(),
        }
    }
}

/// Dev-exception check (plan Phase 3 navigation policy): only localhost
/// HTTP URLs are loadable in dev mode. Everything else must go through the
/// custom scheme or an explicit `host.open_external_url` approval.
pub fn is_dev_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "http"
        && parsed.port().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

/// In-process host: events go to an unbounded channel consumed by the
/// WebView dispatcher (Task 4). Unbounded is acceptable here because events
/// are small JSON values and the consumer is the local UI loop; large
/// payloads (screenshots, file contents) travel by reference/handle, never
/// as giant channel messages.
#[derive(Debug, Clone)]
pub struct ChannelHost {
    tx: tokio::sync::mpsc::UnboundedSender<HostEvent>,
}

impl ChannelHost {
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<HostEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl Default for ChannelHost {
    fn default() -> Self {
        Self::new().0
    }
}

#[async_trait::async_trait]
impl WindowHost for ChannelHost {
    async fn emit(&self, event: HostEvent) -> Result<(), HostError> {
        self.tx.send(event).map_err(|_| HostError::ChannelClosed)
    }
}

/// Build the winit event loop that will own the main thread (plan Phase 37:
/// the window loop never blocks on async work; bridges use
/// `EventLoopProxy` + tokio channels in Task 3).
pub fn new_event_loop() -> Result<winit::event_loop::EventLoop<()>, HostError> {
    Ok(winit::event_loop::EventLoop::new()?)
}

/// Compile-time proof that the pinned WebView stack links: both crates in
/// the plan's preferred stack resolve here. Full WebView creation (window +
/// `WebViewBuilder` + custom scheme) lands in Task 3.
pub fn webview_stack() -> (&'static str, &'static str) {
    (
        std::any::type_name::<wry::WebView>(),
        std::any::type_name::<winit::event_loop::EventLoop<()>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channel_host_delivers_events() {
        let (host, mut rx) = ChannelHost::new();
        host.emit(HostEvent {
            event: "app.ready".to_string(),
            data: serde_json::json!({"version": "0.1.0"}),
        })
        .await
        .unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.event, "app.ready");
    }

    #[test]
    fn dev_url_policy_allows_only_localhost() {
        use super::is_dev_url;
        assert!(is_dev_url("http://localhost:5173/app"));
        assert!(is_dev_url("http://127.0.0.1:5173/"));
        assert!(is_dev_url("http://[::1]:5173/"));
        assert!(!is_dev_url("https://example.com/"));
        assert!(!is_dev_url("http://192.168.1.2:5173/"));
        assert!(!is_dev_url("http://localhost.attacker.com:5173/"));
        assert!(!is_dev_url("http://127.0.0.1.evil.com:5173/"));
        assert!(!is_dev_url("file:///etc/passwd"));
    }

    #[test]
    fn webview_stack_links() {
        let (webview, event_loop) = webview_stack();
        assert!(webview.contains("wry"));
        assert!(event_loop.contains("winit"));
    }
}
