//! Native Utsuwa host binary.
//!
//! Linux embeds the WebView in a GTK window (`build_gtk`), which works on
//! both X11/XWayland and native Wayland — plain winit windows only carry
//! X11 handles, so they fail on Wayland sessions. Other platforms keep
//! the winit window path. Dev mode (`--dev`) loads the Svelte dev
//! server; otherwise bundled assets are served by Rust over the custom
//! scheme (plan Phase 3).
//!
//! Pass `--debug` to enable structured diagnostics for the native host and
//! write them to the rolling log under the Utsuwa state directory. `--trace`
//! enables the more verbose per-module trace filter. `RUST_LOG` remains
//! supported for normal runs.

use app_host::{
    ipc::{emit_script, Dispatcher},
    protocol::{self, AssetServer},
    runtime::EmitFn,
    AppHostConfig, FrontendSource,
};
use ipc_core::HostEvent;
use policy_core::ApprovalQueue;
use std::sync::{
    mpsc::{Receiver, Sender},
    Arc, Mutex,
};
use wry::WebViewBuilder;

#[cfg(not(target_os = "linux"))]
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Window, WindowAttributes},
};

/// Bridge installed before any page script runs: `window.utsuwa.invoke`
/// for typed requests, `utsuwa-host-event` for host pushes (Task 6).
const BRIDGE_JS: &str = include_str!("bridge.js");

/// Wakes the UI thread after queueing a reply/emit script. The winit
/// path pings the event-loop proxy; the GTK path needs nothing — a
/// timeout source drains the queue several times a second.
type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, Default)]
struct CliOptions {
    dev: bool,
    dev_grant_workspace: bool,
    debug: bool,
    trace: bool,
}

impl CliOptions {
    fn from_env() -> Self {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        Self {
            dev: args.iter().any(|arg| arg == "--dev"),
            dev_grant_workspace: args.iter().any(|arg| arg == "--dev-grant-workspace"),
            debug: args.iter().any(|arg| arg == "--debug"),
            trace: args.iter().any(|arg| arg == "--trace"),
        }
    }
}

const DEBUG_LOG_MODULES: &[&str] = &[
    "app_host",
    "agent_core",
    "tool_core",
    "tool_filesystem",
    "capability_core",
    "policy_core",
    "mcp_runtime",
    "plugin_wasm",
    "tool_process",
    "tool_desktop",
    "desktop_linux",
    "memory",
    "storage_core",
    "secret_core",
    "model_openai_compatible",
];

fn debug_filter(level: &str) -> String {
    DEBUG_LOG_MODULES
        .iter()
        .map(|module| format!("{module}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Initialize stderr logging for normal runs and stderr + a rolling file for
/// `--debug`/`--trace`. The guard must remain alive until process shutdown so
/// the non-blocking writer can flush its final records.
fn init_logging(options: CliOptions) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let verbose = options.debug || options.trace;
    if !verbose {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("app_host=info".parse().expect("static directive")),
            )
            .init();
        return None;
    }

    let level = if options.trace { "trace" } else { "debug" };
    let filter = tracing_subscriber::EnvFilter::new(debug_filter(level));
    let log_dir = storage_core::default_state_dir("utsuwa").join("logs");
    let appender = match std::fs::create_dir_all(&log_dir) {
        Ok(()) => tracing_appender::rolling::daily(&log_dir, "utsuwa.log"),
        Err(error) => {
            eprintln!("could not create native debug log directory {log_dir:?}: {error}");
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(true)
                .with_thread_ids(true)
                .with_file(options.trace)
                .with_line_number(options.trace)
                .init();
            return None;
        }
    };
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let writer = file_writer.and(std::io::stderr);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(options.trace)
        .with_line_number(options.trace)
        .init();
    tracing::info!(
        debug = options.debug,
        trace = options.trace,
        log_dir = %log_dir.display(),
        "native debug logging enabled"
    );
    Some(guard)
}

/// Shared WebView configuration: URL, navigation policy, custom scheme,
/// bridge script, and the typed IPC handler. Replies queue on
/// `reply_tx`; the platform runner drains them on the UI thread (every
/// reply wakes it through `waker`).
fn configure_builder(
    config: &AppHostConfig,
    dispatcher: &Dispatcher,
    reply_tx: Sender<String>,
    waker: Waker,
) -> Option<WebViewBuilder<'static>> {
    let dev_mode = matches!(config.frontend, FrontendSource::DevUrl(_));
    let initial_url = match &config.frontend {
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
                return None;
            }
        }
    };

    let mut builder = WebViewBuilder::new()
        .with_url(&initial_url)
        .with_navigation_handler(move |url| {
            let allowed = protocol::is_navigation_allowed(&url, dev_mode);
            if !allowed {
                tracing::warn!(%url, "blocked navigation outside companion://app");
            }
            allowed
        });
    builder = builder.with_new_window_req_handler(|url, _features| {
        tracing::warn!(%url, "blocked new-window request (needs host.open_external_url)");
        wry::NewWindowResponse::Deny
    });
    if let Some(server) = asset_server {
        builder = builder.with_custom_protocol(protocol::APP_SCHEME.to_string(), move |_, req| {
            server.handle(req)
        });
    }
    let ipc_dispatcher = dispatcher.clone();
    builder = builder.with_initialization_script(BRIDGE_JS);
    Some(builder.with_ipc_handler(move |request| {
        // Typed dispatch: only `IpcMethod` members parse, so raw OS
        // operations can never arrive here (ipc-core has no such
        // variants). Replies go back through the bridge's
        // `__resolve`, keyed by request id.
        let reply_tx = reply_tx.clone();
        let waker = Arc::clone(&waker);
        ipc_dispatcher.handle_message_with_callback(request.body(), move |script| {
            // Queue full / loop gone: log and drop; the bridge
            // promise stays pending rather than resolving wrongly.
            if reply_tx.send(script).is_err() {
                tracing::warn!("dropping ipc reply: reply queue closed");
            } else {
                waker();
            }
        });
    }))
}

/// Announce readiness over the same typed event channel the frontend
/// subscribes to (`utsuwa-host-event: app.ready`).
fn ready_script(dispatcher: &Dispatcher) -> String {
    emit_script(&HostEvent {
        event: "app.ready".to_string(),
        data: serde_json::json!({ "version": dispatcher.app_version }),
    })
}

/// Linux runner: GTK window + embedded WebView. GDK picks the Wayland
/// backend on Wayland sessions and X11 under XWayland, so one binary
/// covers both — no `env -u WAYLAND_DISPLAY` needed anymore.
#[cfg(target_os = "linux")]
fn run_gtk(
    config: AppHostConfig,
    dispatcher: Dispatcher,
    reply_tx: Sender<String>,
    reply_rx: Receiver<String>,
) {
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    if let Err(err) = gtk::init() {
        tracing::error!(%err, "gtk::init failed");
        std::process::exit(1);
    }
    tracing::info!(
        session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        "starting GTK-embedded webview (X11 and Wayland)"
    );

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Utsuwa");
    window.set_default_size(1200, 800);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&vbox);

    let noop: Waker = Arc::new(|| {});
    let builder = match configure_builder(&config, &dispatcher, reply_tx.clone(), noop) {
        Some(builder) => builder,
        None => std::process::exit(1),
    };
    let webview = match builder.build_gtk(&vbox) {
        Ok(webview) => webview,
        Err(err) => {
            tracing::error!(%err, "failed to create webview");
            tracing::error!("XWayland fallback still available: env -u WAYLAND_DISPLAY cargo run");
            std::process::exit(1);
        }
    };
    window.show_all();
    if reply_tx.send(ready_script(&dispatcher)).is_err() {
        tracing::warn!("dropping app.ready event: reply queue closed");
    }

    // Reply/emit scripts drain on the GTK thread. The webview never
    // crosses threads: this source is installed here and only ever runs
    // on the main loop (`timeout_add_local` accepts the non-Send
    // closure, unlike thread-bound sources).
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(8), move || {
        while let Ok(script) = reply_rx.try_recv() {
            if let Err(err) = webview.evaluate_script(&script) {
                tracing::warn!(%err, "failed to deliver ipc reply to webview");
            }
        }
        gtk::glib::ControlFlow::Continue
    });

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });
    gtk::main();
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(not(target_os = "linux"))]
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

#[cfg(not(target_os = "linux"))]
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

        let proxy = self.proxy.clone();
        let waker: Waker = Arc::new(move || {
            if proxy.send_event(()).is_err() {
                tracing::warn!("dropping ipc reply: event loop closed");
            }
        });
        let builder =
            match configure_builder(&self.config, &self.dispatcher, self.reply_tx.clone(), waker) {
                Some(builder) => builder,
                None => {
                    event_loop.exit();
                    return;
                }
            };
        match builder.build(&window) {
            Ok(webview) => {
                self.window = Some(window);
                self.webview = Some(webview);
                if self.reply_tx.send(ready_script(&self.dispatcher)).is_err() {
                    tracing::warn!("dropping app.ready event: reply queue closed");
                }
            }
            Err(err) => {
                tracing::error!(%err, "failed to create webview");
                event_loop.exit();
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
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
}

#[cfg(not(target_os = "linux"))]
fn run_winit(
    config: AppHostConfig,
    dispatcher: Dispatcher,
    reply_tx: Sender<String>,
    reply_rx: Receiver<String>,
    event_loop: EventLoop<()>,
) {
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
        tracing::error!(err = %err, "event loop exited with error");
        std::process::exit(1);
    }
}

/// Shared host startup: storage, audit, approvals, dispatcher, and the
/// agent runtime. The UI runner (GTK or winit) takes over afterwards.
fn start_host(emit: EmitFn, dev_grant_workspace: bool) -> Dispatcher {
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
    if dev_grant_workspace {
        match std::env::current_dir().and_then(|path| path.canonicalize()) {
            Ok(workspace) => {
                let is_secret_root = policy_core::is_secret_path(&capability_core::Resource::Path(
                    workspace.clone(),
                ));
                let is_home = std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .and_then(|home| std::path::PathBuf::from(home).canonicalize().ok())
                    .is_some_and(|home| home == workspace);
                if !is_secret_root && !is_home {
                    if let Ok(queue) = approvals.lock() {
                        if let Err(err) = queue.grant_direct(
                            capability_core::PrincipalKind::Agent,
                            capability_core::Capability::FilesystemRead,
                            capability_core::ResourceScope::new(vec![
                                capability_core::Resource::Path(workspace.clone()),
                            ]),
                            policy_core::GrantLifetime::Session,
                            format!("developer workspace grant for {}", workspace.display()),
                        ) {
                            tracing::error!(%err, "could not install developer workspace grant");
                        }
                    }
                } else {
                    tracing::warn!(path = %workspace.display(), "refusing developer grant for home or secret root");
                }
            }
            Err(err) => {
                tracing::warn!(%err, "cannot resolve current directory for developer grant")
            }
        }
    }
    let version = env!("CARGO_PKG_VERSION").to_string();
    let secrets = secret_core::system("utsuwa");
    let mut dispatcher = Dispatcher::new(version)
        .with_approvals(Arc::clone(&approvals))
        .with_audit(Arc::clone(&audit))
        .with_secret_store(Arc::clone(&secrets));
    if let Some(store) = &storage {
        dispatcher = dispatcher.with_storage(Arc::clone(store));
    }
    match app_host::runtime::AgentRuntime::start_with_secrets(
        approvals,
        storage,
        Some(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>),
        emit,
        secrets,
    ) {
        Ok(runtime) => {
            // Durable memory beside state.db; an unopenable file falls
            // back to the runtime's isolated in-memory store (logged).
            let memory_path = storage_core::default_state_dir("utsuwa").join("memory.db");
            match memory::MemoryStore::open(&memory_path) {
                Ok(store) => runtime.set_memory_store(Arc::new(store)),
                Err(err) => {
                    tracing::error!(%err, "failed to open memory.db; using in-memory memory")
                }
            }
            dispatcher = dispatcher.with_agent(runtime);
        }
        Err(err) => tracing::error!(%err, "agent runtime unavailable; agent.* methods will fail"),
    }
    dispatcher
}

fn main() {
    let options = CliOptions::from_env();
    let _log_guard = init_logging(options);
    let dev = options.dev;
    let dev_grant_workspace = options.dev_grant_workspace;
    let config = if dev {
        AppHostConfig::dev(env!("CARGO_PKG_VERSION"))
    } else {
        AppHostConfig {
            frontend: FrontendSource::Bundled,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    };

    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    // Agent runtime → frontend event path: scripts queue on the reply
    // channel and the UI runner evaluates them on its thread, exactly
    // like IPC replies.
    let emit_tx = reply_tx.clone();

    #[cfg(target_os = "linux")]
    {
        let emit: EmitFn = Arc::new(move |event| {
            if emit_tx.send(emit_script(&event)).is_err() {
                tracing::warn!("dropping agent event: reply queue closed");
            }
        });
        let dispatcher = start_host(emit, dev_grant_workspace);
        run_gtk(config, dispatcher, reply_tx, reply_rx);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let event_loop = match EventLoop::new() {
            Ok(event_loop) => event_loop,
            Err(err) => {
                tracing::error!(%err, "failed to create event loop");
                std::process::exit(1);
            }
        };
        let emit_proxy = event_loop.create_proxy();
        let emit: EmitFn = Arc::new(move |event| {
            if emit_tx.send(emit_script(&event)).is_err() {
                tracing::warn!("dropping agent event: reply queue closed");
            } else if emit_proxy.send_event(()).is_err() {
                tracing::warn!("dropping agent event: event loop closed");
            }
        });
        let dispatcher = start_host(emit, dev_grant_workspace);
        run_winit(config, dispatcher, reply_tx, reply_rx, event_loop);
    }
}
