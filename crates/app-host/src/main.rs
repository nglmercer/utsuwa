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
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_os = "linux"))]
use std::sync::mpsc::Receiver;
use std::sync::{mpsc::Sender, Arc, Mutex};
use wry::{PermissionKind, PermissionResponse, WebViewBuilder};

#[cfg(not(target_os = "linux"))]
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Window, WindowAttributes},
};

/// Bridge installed before any page script runs: `window.utsuwa.invoke`
/// for typed requests, `utsuwa-host-event` for host pushes. Buffers early
/// host events until the frontend drains them after registering listeners.
const BRIDGE_JS: &str = include_str!("bridge.js");

/// Debug diagnostics installed right after the bridge (same document-start
/// timing): forwards `window.onerror`, `unhandledrejection`,
/// `console.error`, and load markers to Rust over `diagnostics.report`.
/// Like all wry initialisation scripts this bypasses the page CSP.
const DIAGNOSTICS_JS: &str = include_str!("diagnostics.js");

/// Wakes the UI thread after queueing a reply/emit script. The winit path
/// pings the event-loop proxy; the GTK path needs nothing — queueing into
/// the tokio channel wakes the main-context consumer directly.
type Waker = Arc<dyn Fn() + Send + Sync>;

/// Abstraction over the reply-script queue so Linux can deliver through an
/// event-driven main-context consumer while the winit path keeps its
/// event-loop wakeups. Returns false when the consumer is gone.
trait ScriptQueue: Clone + Send + Sync + 'static {
    fn send_script(&self, script: String) -> bool;
}

impl ScriptQueue for Sender<String> {
    fn send_script(&self, script: String) -> bool {
        self.send(script).is_ok()
    }
}

impl ScriptQueue for tokio::sync::mpsc::UnboundedSender<String> {
    fn send_script(&self, script: String) -> bool {
        self.send(script).is_ok()
    }
}

/// Hook invoked with each custom-protocol request URI (Linux watchdog and
/// first-request logging). Fired from the protocol handler on the UI thread.
type AssetHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Classify a queued script for failure logs without printing payloads
/// (replies may carry settings content or other sensitive data).
fn script_kind(script: &str) -> &'static str {
    if script.contains("__emit") {
        "emit"
    } else if script.contains("__resolve") {
        "resolve"
    } else {
        "other"
    }
}

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
    "audio_capture",
    "model_openai_compatible",
];

fn debug_filter(level: &str) -> String {
    DEBUG_LOG_MODULES
        .iter()
        .map(|module| format!("{module}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Native policy for permissions requested by the embedded frontend.
///
/// Microphone access is still requested by the frontend at the point of the
/// user's recording click. The WebView handler only decides how that request
/// is serviced by the platform backend; it does not open a media stream.
/// Camera access stays explicitly denied until Utsuwa has a camera feature.
fn permission_response(kind: PermissionKind) -> PermissionResponse {
    let response = match kind {
        PermissionKind::Microphone => PermissionResponse::Allow,
        PermissionKind::Camera => PermissionResponse::Deny,
        _ => PermissionResponse::Default,
    };
    tracing::info!(?kind, ?response, "webview permission request");
    response
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
/// bridge scripts, and the typed IPC handler. Replies queue on
/// `reply_tx`; the platform runner drains them on the UI thread.
///
/// When `defer_initial_navigation` is set (Linux), the builder is created
/// without an initial URL so the caller can request navigation explicitly
/// after post-build setup (CORS registration, load signals) is complete.
/// Returns the builder plus the initial URL to navigate to.
fn configure_builder<Q: ScriptQueue>(
    config: &AppHostConfig,
    dispatcher: &Dispatcher,
    reply_tx: Q,
    waker: Waker,
    defer_initial_navigation: bool,
    asset_hook: Option<AssetHook>,
) -> Option<(WebViewBuilder<'static>, String)> {
    tracing::debug!("webview.configure.begin");
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
                tracing::info!(%err, "no asset dir; media custom scheme remains enabled in dev mode");
                None
            } else {
                tracing::error!(%err, asset_dir, "cannot serve bundled frontend (run `pnpm build:native` or set UTSUWA_ASSET_DIR)");
                return None;
            }
        }
    };
    if let Some(server) = &asset_server {
        let report = server.report();
        tracing::debug!(
            root = %report.root.display(),
            index_exists = report.index_exists,
            index_bytes = report.index_size,
            index_mtime_unix = report.index_modified_secs,
            files = report.file_count,
            truncated = report.truncated,
            app_version = env!("CARGO_PKG_VERSION"),
            "webview.assets.verified"
        );
        if !dev_mode && !report.index_exists {
            tracing::error!(
                root = %report.root.display(),
                "bundled frontend has no build/index.html; run `pnpm build:native` (or set UTSUWA_ASSET_DIR) instead of opening an empty window"
            );
            return None;
        }
    }

    let mut builder = WebViewBuilder::new();
    if !defer_initial_navigation {
        builder = builder.with_url(&initial_url);
    }
    builder = builder
        .with_permission_handler(permission_response)
        .with_navigation_handler(move |url| {
            let allowed = protocol::is_navigation_allowed(&url, dev_mode);
            if allowed {
                tracing::debug!(%url, "webview.navigation.policy.allow");
            } else {
                tracing::warn!(%url, "blocked navigation outside companion://app");
            }
            allowed
        });
    builder = builder.with_new_window_req_handler(|url, _features| {
        tracing::warn!(%url, "blocked new-window request (needs host.open_external_url)");
        wry::NewWindowResponse::Deny
    });
    let media_registry = dispatcher.media_registry();
    builder = builder.with_custom_protocol(protocol::APP_SCHEME.to_string(), move |_, req| {
        if let Some(hook) = &asset_hook {
            hook(&req.uri().to_string());
        }
        if app_host::audio::is_media_path(req.uri().path()) {
            media_registry.handle(req)
        } else if let Some(server) = &asset_server {
            server.handle(req)
        } else {
            wry::http::Response::builder()
                .status(wry::http::StatusCode::FORBIDDEN)
                .header("Content-Type", "text/plain")
                .body(std::borrow::Cow::Borrowed(b"forbidden" as &[u8]))
                .unwrap_or_else(|_| wry::http::Response::new(std::borrow::Cow::Borrowed(&[])))
        }
    });
    tracing::debug!(
        scheme = protocol::APP_SCHEME,
        "webview.custom_protocol.registered"
    );
    let ipc_dispatcher = dispatcher.clone();
    builder = builder.with_initialization_script(BRIDGE_JS);
    builder = builder.with_initialization_script(DIAGNOSTICS_JS);
    let builder = builder.with_ipc_handler(move |request| {
        // Typed dispatch: only `IpcMethod` members parse, so raw OS
        // operations can never arrive here (ipc-core has no such
        // variants). Replies go back through the bridge's
        // `__resolve`, keyed by request id.
        let reply_tx = reply_tx.clone();
        let waker = Arc::clone(&waker);
        ipc_dispatcher.handle_message_with_callback(request.body(), move |script| {
            // Queue full / loop gone: log and drop; the bridge
            // promise rejects on timeout rather than resolving wrongly.
            if reply_tx.send_script(script) {
                waker();
            } else {
                tracing::warn!("dropping ipc reply: reply queue closed");
            }
        });
    });
    tracing::debug!(
        deferred_navigation = defer_initial_navigation,
        "webview.configure.ok"
    );
    Some((builder, initial_url))
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
///
/// Startup order (each step checkpoint-logged under `--debug`):
/// GTK init → window → builder (custom protocol registered inside
/// `build_gtk`) → CORS fix → load signals → explicit initial navigation →
/// window shown → event-driven script consumer → main loop.
#[cfg(target_os = "linux")]
fn run_gtk(
    config: AppHostConfig,
    dispatcher: Dispatcher,
    script_tx: tokio::sync::mpsc::UnboundedSender<String>,
    mut script_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    if let Err(err) = gtk::init() {
        tracing::error!(%err, "gtk.init.failed");
        std::process::exit(1);
    }
    tracing::debug!("gtk.init.ok");
    log_linux_runtime_diagnostics();
    tracing::info!(
        session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        "starting GTK-embedded webview (X11 and Wayland)"
    );

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Utsuwa");
    window.set_default_size(1200, 800);
    tracing::debug!("gtk.window.created");
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&vbox);

    let dev_mode = matches!(config.frontend, FrontendSource::DevUrl(_));
    // Watchdog state: set when the initial document request reaches Rust.
    let initial_served = Arc::new(AtomicBool::new(false));
    let first_request_seen = Arc::new(AtomicBool::new(false));
    let hook_url = match &config.frontend {
        FrontendSource::DevUrl(url) => url.clone(),
        FrontendSource::Bundled => protocol::initial_url(),
    };
    let hook_served = Arc::clone(&initial_served);
    let hook_first = Arc::clone(&first_request_seen);
    let asset_hook: AssetHook = Arc::new(move |uri| {
        if uri == hook_url {
            hook_served.store(true, Ordering::SeqCst);
        }
        if !hook_first.swap(true, Ordering::SeqCst) {
            tracing::debug!(uri, "webview.asset.first_request");
        }
    });

    let noop: Waker = Arc::new(|| {});
    tracing::debug!("webview.build_gtk.begin");
    let (builder, initial_url) = match configure_builder(
        &config,
        &dispatcher,
        script_tx.clone(),
        noop,
        true,
        Some(asset_hook),
    ) {
        Some(built) => built,
        None => std::process::exit(1),
    };
    let webview = match builder.build_gtk(&vbox) {
        Ok(webview) => {
            tracing::debug!("webview.build_gtk.ok");
            webview
        }
        Err(err) => {
            tracing::error!(%err, "webview.build_gtk.failed");
            tracing::error!("XWayland fallback still available: env -u WAYLAND_DISPLAY cargo run");
            std::process::exit(1);
        }
    };

    // WebKitGTK 2.46+ fix (see the webkit2gtk note in Cargo.toml): wry
    // 0.56.1 registers the custom scheme as secure but not as CORS-enabled,
    // and newer WebKitGTK refuses top-level navigation to such schemes.
    // Registering here runs before the initial navigation is requested and
    // before the main loop starts, which is timing-equivalent to wry doing
    // it during its own setup — nothing is processed until `gtk::main()`.
    if !register_custom_scheme_cors(&webview) {
        tracing::error!(
            "custom-scheme CORS registration failed; on WebKitGTK >= 2.46 bundled companion:// navigation is expected to fail"
        );
    }

    let page_committed = Arc::new(AtomicBool::new(false));
    let asset_root = std::env::var("UTSUWA_ASSET_DIR").unwrap_or_else(|_| "build".to_string());
    connect_load_signals(
        &webview,
        Arc::clone(&page_committed),
        &asset_root,
        &initial_url,
    );

    // Deferred initial navigation: requested only after the custom protocol
    // (registered during `build_gtk`, before any load) and the CORS fix are
    // both in place. Verified against wry 0.56.1 sources: its builder calls
    // `register_uri_scheme` before `load_uri`, so protocol-before-navigation
    // holds either way; deferring additionally orders our CORS fix first.
    tracing::debug!(url = %initial_url, "webview.navigation.request");
    if let Err(err) = webview.load_url(&initial_url) {
        tracing::error!(%err, url = %initial_url, "webview.navigation.request_failed");
        // Show diagnostics instead of exiting: a window explaining the
        // failure beats a dead process for a scheduling failure.
        use webkit2gtk::WebViewExt as _;
        use wry::WebViewExtUnix as _;
        webview.webview().load_html(
            &fatal_page_html(
                &asset_root,
                &initial_url,
                &format!("initial navigation could not be scheduled: {err}"),
            ),
            None,
        );
    }

    window.show_all();
    tracing::debug!("webview.window.shown");
    if !script_tx.send_script(ready_script(&dispatcher)) {
        tracing::warn!("dropping app.ready event: reply queue closed");
    }
    tracing::debug!("host.app_ready.queued");

    // Clone the inner handle before `webview` moves into the consumer: the
    // watchdog below needs it to show the fatal page on total failure.
    let watchdog_view = {
        use wry::WebViewExtUnix as _;
        webview.webview()
    };

    // Event-driven script delivery: producers on any thread push into the
    // unbounded channel and each send wakes this main-context task — no
    // polling, no idle wakeups. The WebView never crosses threads: the
    // consumer future is `spawn_local`, so it only ever runs on the GTK
    // thread. (Dropping the JoinHandle detaches; only `abort()` cancels.)
    {
        let context = gtk::glib::MainContext::default();
        let _consumer = context.spawn_local(async move {
            while let Some(script) = script_rx.recv().await {
                if let Err(err) = webview.evaluate_script(&script) {
                    tracing::warn!(
                        %err,
                        kind = script_kind(&script),
                        "failed to deliver ipc reply to webview"
                    );
                }
            }
        });
    }

    // Bundled-mode watchdog: if the initial document never reaches the Rust
    // handler, the window would otherwise sit blank with no explanation.
    if !dev_mode {
        let seen = Arc::clone(&initial_served);
        let committed = Arc::clone(&page_committed);
        let fatal_url = initial_url.clone();
        let fatal_root = asset_root.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_secs(10), move || {
            tracing::debug!(
                seen = seen.load(Ordering::SeqCst),
                committed = committed.load(Ordering::SeqCst),
                "webview.navigation.watchdog.fired"
            );
            if !seen.load(Ordering::SeqCst) && !committed.load(Ordering::SeqCst) {
                tracing::error!(
                    url = %fatal_url,
                    asset_root = %fatal_root,
                    "bundled navigation watchdog: the initial document never reached the Rust protocol handler; \
                     likely WebKit/Wry custom-scheme registration failure (run with --trace for webview.asset.request lines)"
                );
                use webkit2gtk::WebViewExt as _;
                watchdog_view.load_html(
                    &fatal_page_html(
                        &fatal_root,
                        &fatal_url,
                        "the initial document request never reached the host protocol handler",
                    ),
                    None,
                );
            } else {
                tracing::debug!("webview.navigation.watchdog.ok");
            }
        });
    }

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });
    tracing::debug!("webview.mainloop.enter");
    gtk::main();
    tracing::debug!("webview.mainloop.exited");
}

/// Connect WebKit load signals for navigation checkpoints and the native
/// fatal-error page. `load_failed` on the WebView fires for main-frame
/// loads only, so subresource 404s never trigger the fatal page; the
/// `page_committed` guard additionally prevents clobbering a page that
/// already committed (e.g. a later reload failing).
#[cfg(target_os = "linux")]
fn connect_load_signals(
    webview: &wry::WebView,
    page_committed: Arc<AtomicBool>,
    asset_root: &str,
    initial_url: &str,
) {
    use webkit2gtk::WebViewExt as _;
    use wry::WebViewExtUnix as _;

    let inner = webview.webview();
    let committed = Arc::clone(&page_committed);
    inner.connect_load_changed(move |view, event| {
        use webkit2gtk::LoadEvent;
        let uri = view.uri().as_deref().unwrap_or("?").to_string();
        match event {
            LoadEvent::Started => {
                committed.store(false, Ordering::SeqCst);
                tracing::debug!(uri, "webview.navigation.started");
            }
            LoadEvent::Redirected => {
                tracing::debug!(uri, "webview.navigation.redirected");
            }
            LoadEvent::Committed => {
                committed.store(true, Ordering::SeqCst);
                tracing::debug!(uri, "webview.navigation.committed");
            }
            LoadEvent::Finished => {
                committed.store(true, Ordering::SeqCst);
                tracing::debug!(uri, "webview.navigation.completed");
            }
            _ => {}
        }
    });
    let fatal_root = asset_root.to_string();
    let fatal_url = initial_url.to_string();
    inner.connect_load_failed(move |view, event, uri, error| {
        tracing::error!(%uri, %error, event = ?event, "webview.navigation.failed");
        if !page_committed.load(Ordering::SeqCst) {
            view.load_html(
                &fatal_page_html(&fatal_root, &fatal_url, &error.to_string()),
                None,
            );
        }
        // Handled: our diagnostic page replaces WebKit's default error page.
        true
    });
}

/// Register the app custom scheme as CORS-enabled on the WebView's context.
///
/// wry 0.56.1 (`webkitgtk/web_context.rs`) calls only
/// `register_uri_scheme_as_secure`; WebKitGTK 2.46+ additionally requires
/// `register_uri_scheme_as_cors_enabled` for top-level custom-scheme
/// navigation. Applied here through wry's public `WebViewExtUnix::webview`
/// handle — same underlying context, no fork or patch needed.
#[cfg(target_os = "linux")]
fn register_custom_scheme_cors(webview: &wry::WebView) -> bool {
    use webkit2gtk::{SecurityManagerExt, WebContextExt, WebViewExt};
    use wry::WebViewExtUnix as _;

    let inner = webview.webview();
    let Some(context) = inner.context() else {
        tracing::error!("webview.custom_scheme.cors_failed: no WebContext on inner WebView");
        return false;
    };
    let Some(manager) = context.security_manager() else {
        tracing::error!("webview.custom_scheme.cors_failed: no SecurityManager on WebContext");
        return false;
    };
    manager.register_uri_scheme_as_cors_enabled(protocol::APP_SCHEME);
    tracing::debug!(
        scheme = protocol::APP_SCHEME,
        "webview.custom_scheme.cors_enabled"
    );
    true
}

/// Debug-only Linux runtime diagnostics. Optional evidence only — nothing
/// here is a hard requirement for startup.
#[cfg(target_os = "linux")]
fn log_linux_runtime_diagnostics() {
    let display_backend = gtk::gdk::Display::default()
        .map(|display| {
            use gtk::glib::ObjectExt as _;
            display.type_().name().to_string()
        })
        .unwrap_or_else(|| "none".to_string());
    tracing::debug!(
        gtk = format!(
            "{}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version()
        ),
        gdk_backend = %display_backend,
        session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        wayland_display_present = std::env::var_os("WAYLAND_DISPLAY").is_some(),
        display_present = std::env::var_os("DISPLAY").is_some(),
        webkit_build = env!("UTSUWA_WEBKIT_PC_VERSION"),
        app_version = env!("CARGO_PKG_VERSION"),
        "linux.runtime.diagnostics"
    );
}

/// Minimal native diagnostic page shown when bundled navigation fails,
/// instead of leaving a blank window. Static HTML, no scripts, no secrets.
#[cfg(target_os = "linux")]
fn fatal_page_html(asset_root: &str, initial_url: &str, error: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Utsuwa failed to load</title>\
         <style>body{{font-family:sans-serif;max-width:46rem;margin:4rem auto;padding:0 1rem;\
         color:#e8e8e8;background:#1a1a1a}}code{{background:#333;padding:.1rem .3rem;\
         border-radius:.25rem}}h1{{font-size:1.4rem}}</style></head><body>\
         <h1>Utsuwa failed to load its bundled frontend</h1>\
         <p>Asset root: <code>{root}</code></p>\
         <p>Initial URL: <code>{url}</code></p>\
         <p>WebKitGTK (build): <code>{webkit}</code> · GTK (build): <code>{gtk}</code></p>\
         <p>Error: <code>{error}</code></p>\
         <p>Run with <code>cargo run -- --trace</code> and look for \
         <code>webview.asset.request</code> lines to see whether document \
         requests reach the host.</p></body></html>",
        root = escape_html(asset_root),
        url = escape_html(initial_url),
        webkit = escape_html(env!("UTSUWA_WEBKIT_PC_VERSION")),
        gtk = escape_html(env!("UTSUWA_GTK_PC_VERSION")),
        error = escape_html(error),
    )
}

/// Minimal HTML escaping for the fatal page (untrusted error text).
#[cfg(target_os = "linux")]
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
                tracing::warn!(
                    %err,
                    kind = script_kind(&script),
                    "failed to deliver ipc reply to webview"
                );
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
        let (builder, _initial_url) = match configure_builder(
            &self.config,
            &self.dispatcher,
            self.reply_tx.clone(),
            waker,
            false,
            None,
        ) {
            Some(built) => built,
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
    tracing::debug!("host.boot.begin");
    let boot_started = std::time::Instant::now();
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
    // Host-owned sensor hub: privacy indicators stay authoritative even if
    // the agent runtime below fails to initialize (degraded mode). The
    // runtime constructor attaches event publication on success; attach here
    // only for the degraded path so events are never duplicated.
    let sensors = Arc::new(app_host::runtime::SensorActivityHub::new());
    let mut dispatcher = Dispatcher::new(version)
        .with_approvals(Arc::clone(&approvals))
        .with_audit(Arc::clone(&audit))
        .with_sensors(Arc::clone(&sensors))
        .with_secret_store(Arc::clone(&secrets));
    let runtime = match app_host::runtime::AgentRuntime::start_with_secrets_and_sensors(
        approvals,
        storage.clone(),
        Some(Arc::clone(&audit) as Arc<dyn audit_core::AuditSink>),
        Arc::clone(&emit),
        secrets,
        Arc::clone(&sensors),
    ) {
        Ok(runtime) => Some(runtime),
        Err(err) => {
            tracing::error!(%err, "agent runtime unavailable; agent.* methods will fail");
            sensors.attach_event_publisher(&emit);
            None
        }
    };
    let audio_activity = sensors.microphone();
    let audio_manager = Arc::new(app_host::audio::AudioCaptureManager::new(
        dispatcher.media_registry(),
        Arc::clone(&emit),
        audio_activity,
    ));
    dispatcher = dispatcher.with_audio_capture(audio_manager);
    if let Some(store) = &storage {
        dispatcher = dispatcher.with_storage(Arc::clone(store));
    }
    if let Some(runtime) = runtime {
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
    tracing::debug!(
        elapsed_ms = boot_started.elapsed().as_millis() as u64,
        "host.boot.ready"
    );
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

    #[cfg(target_os = "linux")]
    {
        // Agent runtime → frontend event path: scripts queue on the script
        // channel and the GTK main-context consumer evaluates them on the
        // UI thread, exactly like IPC replies. Unbounded async channel:
        // `send` is synchronous and thread-safe, and each send wakes the
        // consumer task — no polling, no waker needed.
        let (script_tx, script_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let emit_tx = script_tx.clone();
        let emit: EmitFn = Arc::new(move |event| {
            if emit_tx.send(emit_script(&event)).is_err() {
                tracing::warn!("dropping agent event: reply queue closed");
            }
        });
        let dispatcher = start_host(emit, dev_grant_workspace);
        run_gtk(config, dispatcher, script_tx, script_rx);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let emit_tx = reply_tx.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_policy_allows_microphone_and_denies_camera() {
        assert_eq!(
            permission_response(PermissionKind::Microphone),
            PermissionResponse::Allow
        );
        assert_eq!(
            permission_response(PermissionKind::Camera),
            PermissionResponse::Deny
        );
    }

    #[test]
    fn permission_policy_leaves_other_permissions_at_the_platform_default() {
        assert_eq!(
            permission_response(PermissionKind::DisplayCapture),
            PermissionResponse::Default
        );
        assert_eq!(
            permission_response(PermissionKind::Geolocation),
            PermissionResponse::Default
        );
    }

    #[test]
    fn script_kind_classifies_without_payloads() {
        assert_eq!(script_kind("window.utsuwa && x.__emit(\"a\",{})\n"), "emit");
        assert_eq!(script_kind("__resolve(\"1\", true, {})"), "resolve");
        assert_eq!(script_kind("alert(1)"), "other");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fatal_page_contains_diagnostics_and_escapes_html() {
        let html = fatal_page_html("build", "companion://app/app", "<oops>&\"");
        assert!(html.contains("Utsuwa failed to load"), "{html}");
        assert!(html.contains("companion://app/app"), "{html}");
        assert!(html.contains("&lt;oops&gt;&amp;&quot;"), "{html}");
        assert!(!html.contains("<oops>"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }
}
