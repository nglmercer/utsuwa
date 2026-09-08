//! Native Utsuwa host binary (Task 3).
//!
//! Opens a winit window with a wry WebView. Dev mode (`--dev`) loads the
//! Svelte dev server; otherwise a bundled placeholder page renders until
//! Task 5 serves the real production assets over the custom scheme.

use app_host::{
    agent_runtime::EmitFn,
    dispatcher::{emit_script, Dispatcher},
    protocol::{self, AssetServer},
    AppHostConfig, FrontendSource,
};
use ipc_core::HostEvent;
use policy_core::ApprovalQueue;
use std::sync::{
    mpsc::{Receiver, Sender},
    Arc, Mutex,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Window, WindowAttributes},
};
use wry::WebViewBuilder;

/// Bridge installed before any page script runs: `window.utsuwa.invoke`
/// for typed requests, `utsuwa-host-event` for host pushes (Task 6).
const BRIDGE_JS: &str = include_str!("bridge.js");

struct HostApp {
    config: AppHostConfig,
    proxy: EventLoopProxy<()>,
    dispatcher: Dispatcher,
    /// Reply/emit scripts queued by the IPC handler thread; drained on the
    /// window thread via `user_event` (woken with `proxy.send_event`).
    reply_tx: Sender<String>,
    reply_rx: Receiver<String>,
    window: Option<Window>,
    webview: Option<wry::WebView>,
}

impl HostApp {
    /// Evaluate every queued reply/emit script. Runs on the window thread.
    fn drain_replies(&mut self) {
        let Some(webview) = &self.webview else {
            return;
        };
        while let Ok(script) = self.reply_rx.try_recv() {
            if let Err(err) = webview.evaluate_script(&script) {
                tracing::warn!(%err, "failed to deliver ipc reply to webview");
            }
        }
    }
}

impl HostApp {
    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title("Utsuwa")
            .with_inner_size(LogicalSize::new(1200, 800));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => window,
            Err(err) => {
                tracing::error!(%err, "failed to create window");
                event_loop.exit();
                return;
            }
        };

        #[cfg(target_os = "linux")]
        if let Err(err) = gtk::init() {
            tracing::error!(%err, "gtk::init failed");
            event_loop.exit();
            return;
        }

        let dev_mode = matches!(self.config.frontend, FrontendSource::DevUrl(_));
        let initial_url = match &self.config.frontend {
            FrontendSource::DevUrl(url) => {
                tracing::info!(%url, "loading dev frontend");
                url.clone()
            }
            FrontendSource::Bundled => {
                tracing::info!(url = protocol::initial_url(), "loading bundled frontend");
                protocol::initial_url()
            }
        };

        // Production assets are served by Rust over the custom scheme —
        // never by an embedded localhost server (plan Phase 3).
        let asset_dir = std::env::var("UTSUWA_ASSET_DIR").unwrap_or_else(|_| "build".to_string());
        let asset_server = match AssetServer::new(asset_dir.clone().into()) {
            Ok(server) => Some(server),
            Err(err) => {
                if dev_mode {
                    tracing::info!(%err, "no asset dir; custom scheme disabled in dev mode");
                    None
                } else {
                    tracing::error!(%err, "cannot serve bundled frontend (set UTSUWA_ASSET_DIR)");
                    event_loop.exit();
                    return;
                }
            }
        };

        let mut builder = WebViewBuilder::new().with_url(&initial_url).with_navigation_handler(
            move |url| {
                let allowed = protocol::is_navigation_allowed(&url, dev_mode);
                if !allowed {
                    tracing::warn!(%url, "blocked navigation outside companion://app");
                }
                allowed
            },
        );
        builder = builder.with_new_window_req_handler(|url| {
            tracing::warn!(%url, "blocked new-window request (needs host.open_external_url)");
            false
        });
        if let Some(server) = asset_server {
            builder = builder.with_custom_protocol(protocol::APP_SCHEME.to_string(), move |_, req| {
                server.handle(req)
            });
        }
        let dispatcher = self.dispatcher.clone();
        let reply_tx = self.reply_tx.clone();
        let proxy = self.proxy.clone();
        builder = builder.with_initialization_script(BRIDGE_JS);
        match builder
            .with_ipc_handler(move |request| {
                // Typed dispatch: only `IpcMethod` members parse, so raw OS
                // operations can never arrive here (ipc-core has no such
                // variants). Replies go back through the bridge's
                // `__resolve`, keyed by request id.
                if let Some(script) = dispatcher.handle_message(request.body()) {
                    // Queue full / loop gone: log and drop; the bridge
                    // promise stays pending rather than resolving wrongly.
                    if reply_tx.send(script).is_err() {
                        tracing::warn!("dropping ipc reply: reply queue closed");
                    } else if proxy.send_event(()).is_err() {
                        tracing::warn!("dropping ipc reply: event loop closed");
                    }
                }
            })
            .build(&window)
        {
            Ok(webview) => {
                self.window = Some(window);
                self.webview = Some(webview);
                // Announce readiness over the same typed event channel the
                // frontend subscribes to (`utsuwa-host-event: app.ready`).
                let ready = emit_script(&HostEvent {
                    event: "app.ready".to_string(),
                    data: serde_json::json!({ "version": self.dispatcher.app_version }),
                });
                if self.reply_tx.send(ready).is_err() {
                    tracing::warn!("dropping app.ready event: reply queue closed");
                }
            }
            Err(err) => {
                tracing::error!(%err, "failed to create webview");
                #[cfg(target_os = "linux")]
                tracing::error!(
                    "on Wayland sessions run under XWayland (env -u WAYLAND_DISPLAY) \
                     until the GTK-embedded backend lands"
                );
                event_loop.exit();
            }
        }
    }
}

impl ApplicationHandler for HostApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.create_window(event_loop);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // Woken by the IPC handler after enqueueing a reply.
        self.drain_replies();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Advance the GTK loop alongside winit (wry platform requirement).
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("app_host=info".parse().expect("static directive")),
        )
        .init();

    let dev = std::env::args().any(|arg| arg == "--dev");
    let config = if dev {
        AppHostConfig::dev(env!("CARGO_PKG_VERSION"))
    } else {
        AppHostConfig {
            frontend: FrontendSource::Bundled,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    };

    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(err) => {
            tracing::error!(%err, "failed to create event loop");
            std::process::exit(1);
        }
    };
    let version = env!("CARGO_PKG_VERSION").to_string();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    // SQLite state: settings KV + persistent grants. A host without
    // storage still runs — approvals go in-memory and every launch
    // re-prompts (fail-closed for authority, open for availability).
    let storage = match storage_core::Storage::open(&storage_core::default_db_path("utsuwa")) {
        Ok(store) => Some(Arc::new(Mutex::new(store))),
        Err(err) => {
            tracing::error!(%err, "failed to open state.db; running without storage");
            None
        }
    };
    // Shared audit sink (plan Phase 35): the permission queue and the
    // agent runtime both record here; the Activity panel reads it back
    // over `activity.list`. Bounded in memory; durable storage is later.
    let audit = Arc::new(audit_core::InMemorySink::new());
    // The live permission kernel state: agent turns submit here, the
    // dialog resolves here, resumed turns read grants from here.
    let approvals = Arc::new(Mutex::new(match &storage {
        Some(store) => {
            let seed = match store.lock().expect("storage lock").load_grants() {
                Ok(rows) => rows.into_iter().map(|row| row.grant).collect(),
                Err(err) => {
                    tracing::error!(%err, "failed to load persistent grants; starting empty");
                    Vec::new()
                }
            };
            ApprovalQueue::new()
                .with_grants(seed)
                .on_persistent_grant(storage_core::persistent_grant_hook(Arc::clone(store)))
                .with_sink(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>)
        }
        None => {
            ApprovalQueue::new().with_sink(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>)
        }
    }));
    // Agent runtime → frontend event path: scripts queue on the reply
    // channel and the proxy wakes the window thread to evaluate them,
    // exactly like IPC replies.
    let emit_tx = reply_tx.clone();
    let emit_proxy = event_loop.create_proxy();
    let emit: EmitFn = Arc::new(move |event| {
        if emit_tx.send(emit_script(&event)).is_err() {
            tracing::warn!("dropping agent event: reply queue closed");
        } else if emit_proxy.send_event(()).is_err() {
            tracing::warn!("dropping agent event: event loop closed");
        }
    });
    let mut dispatcher =
        Dispatcher::new(version).with_approvals(Arc::clone(&approvals)).with_audit(Arc::clone(&audit));
    if let Some(store) = &storage {
        dispatcher = dispatcher.with_storage(Arc::clone(store));
    }
    match app_host::agent_runtime::AgentRuntime::start(
        approvals,
        storage,
        Some(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>),
        emit,
    ) {
        Ok(runtime) => dispatcher = dispatcher.with_agent(runtime),
        Err(err) => tracing::error!(%err, "agent runtime unavailable; agent.* methods will fail"),
    }
    let mut app = HostApp {
        proxy: event_loop.create_proxy(),
        dispatcher,
        config,
        reply_tx,
        reply_rx,
        window: None,
        webview: None,
    };
    if let Err(err) = event_loop.run_app(&mut app) {
        tracing::error!(%err, "event loop exited with error");
        std::process::exit(1);
    }
}
