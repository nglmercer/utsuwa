# Linux desktop startup

How the bundled Linux app boots, what to look at when it doesn't, and why
two non-obvious workarounds exist. Run everything below with debug logging:

```bash
cargo run -- --debug        # bundled production path (companion://app)
cargo run -- --dev --debug  # Vite dev server at http://localhost:5173
```

## Normal boot checkpoints (`--debug`)

A healthy bundled boot prints, in order:

```text
host.boot.begin
host.runtime.ready
host.boot.ready
gtk.init.ok
linux.runtime.diagnostics        (GTK/GDK versions, session, WebKit build+runtime)
gtk.window.created
webview.build_gtk.begin
webview.configure.begin
webview.assets.verified         (asset root, index.html size/mtime, file count)
webview.custom_protocol.registered scheme=companion
webview.configure.ok
webview.build_gtk.ok
webview.custom_scheme.cors_enabled
webview.navigation.request url=companion://app/app
webview.window.shown
host.app_ready.queued
webview.mainloop.enter
webview.navigation.started
webview.asset.first_request     (the document reached the Rust handler)
webview.navigation.committed
webview.navigation.completed
frontend.bootstrap.begin        (forwarded from the page)
host.frontend.ready             (handshake: listeners registered)
frontend.bootstrap.ready        (buffered host events replayed)
webview.navigation.watchdog.ok  (10s after navigation)
```

`--trace` adds per-request `webview.asset.request` lines (method, URI,
status, MIME, bytes, elapsed) and per-call `webview.ipc.request/reply`.

## If the window stays blank

Find the last checkpoint and work forward:

| Last checkpoint | Meaning |
|---|---|
| `webview.navigation.request`, no `asset.first_request` | WebKit never called the custom protocol. See workaround 1. |
| `asset.*` 404/403 lines | Asset root wrong or route fallback broken (`UTSUWA_ASSET_DIR`, `pnpm build:native`). |
| `navigation.completed`, no `frontend.bootstrap.begin` | Page loaded but app JS failed. Check `webview diagnostic kind=window.error` lines (forwarded `window.onerror`) and the CSP meta in `build/index.html`. |
| `bootstrap.begin`, no `host.frontend.ready` | IPC down. Check `webview.ipc.request` lines and bridge timeouts. |
| Ready, then all logging stops | A backend call wedged a thread. A `webview.ipc.request` with no matching `webview.ipc.reply` names the call. See workaround 2. |

After 10s without the initial document, the bundled watchdog logs an error
and replaces the blank window with a diagnostic page (asset root, URL,
WebKit build, error). `load_failed` on the main frame does the same.

## Failure classes: page load vs JS vs web-process crash

These look identical (a stuck window) but have different fixes — separate
them by checkpoint before changing anything:

| Observation | Failure class | Next step |
|---|---|---|
| No `asset.first_request` | HTML never loaded (protocol/scheme) | Workaround 1, asset-root checks |
| `navigation.completed`, no `bootstrap.begin`, `webview diagnostic kind=window.error` | JS modules/exception (CSP, import, runtime error) | Fix the reported error/CSP hash |
| `bootstrap.ready`, then `webview.process.terminated reason=Crashed` | Web-process crash (often GPU/WebGL) | See below; the Rust host is still alive |
| `bootstrap.ready`, then silence, `ipc.request` without `ipc.reply` | Wedged backend call | Workaround 2 |

A `webview.process.terminated` line is never a Rust freeze: the content
process died while the host kept running. When the crash consistently
follows the first 3D avatar paint (Three.js/VRM WebGL init), suspect the
GPU path — but confirm the ordering first; do not start with workarounds.

Diagnostic-only environment variables (verified present in WebKitGTK
2.52; narrow the fault, never ship as defaults, never disable sandboxing
or acceleration permanently without evidence):

```bash
# Force software compositing for one run (rules out GPU compositor faults):
WEBKIT_DISABLE_COMPOSITING_MODE=1 cargo run -- --debug
# Force the non-DMABuf renderer for one run (rules out DMABuf faults):
WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo run -- --debug
```

If either variable turns a deterministic crash into a clean boot, the
fault is in that GPU path — report it with the `webkit_runtime` version
from `linux.runtime.diagnostics`, not with a permanent workaround.

## Workaround 1: custom-scheme CORS registration

wry 0.56.1 registers custom WebKitGTK schemes as *secure* but not as
*CORS-enabled*. WebKitGTK 2.46+ refuses top-level navigation to such
schemes, so `companion://app/app` never reached the Rust handler and the
window sat blank. The host applies the missing call itself, right after
`build_gtk` and before the deferred initial navigation:

```rust
manager.register_uri_scheme_as_cors_enabled("companion");
```

via wry's public `WebViewExtUnix::webview()` handle — same underlying
`WebContext`, no fork or patch needed. This runs before `gtk::main()`, so
nothing is processed until both registrations exist. If a future wry
release registers CORS itself, this call is a harmless duplicate. See the
`webkit2gtk` note in `crates/app-host/Cargo.toml`.

## Workaround 2: one shared AT-SPI connection, desktop IPC off the UI thread

Two related hardening fixes in the screen-sharing status path (polled
every 5s by the frontend):

1. **Shared registry connection.** Every status poll used to create a fresh
   `AccessibilityConnection` and drop it. The second creation in a process
   hangs forever (nested `block_on` inside the constructor's peer-listener
   setup), which froze the calling thread. `AtspiService` now holds one
   cached connection; every live operation runs through
   `run_live()` with a 20s timeout, and bus failures invalidate the cache
   so the next call reconnects.
2. **Desktop IPC on worker threads.** `desktop.*` IPC methods dispatch on a
   dedicated worker instead of the UI/IPC callback thread, so a blocking OS
   round-trip (AT-SPI, X11, portals) can never deadlock the GTK main loop.
   The worker dispatches synchronously *without* entering a Tokio runtime,
   which keeps the nested `block_on_host` calls inside legal.

## Handshake (deterministic readiness)

`app.ready` used to fire before page scripts existed, so it was routinely
lost. Now:

1. Rust's emit path stashes events in `window.__utsuwaEarlyEvents` when the
   bridge script hasn't run yet; `bridge.js` drains that stash on load and
   buffers further events.
2. The page registers `utsuwa-host-event` listeners during mount.
3. `nativeBoot.ensureHandshake()` (root layout, desktop builds only)
   invokes `host.frontend_ready`; Rust marks the frontend ready and answers
   with `host.runtime_state` (versions, platform, backend, capabilities).
4. The handshake calls `bridge.markReady()`, replaying buffered events in
   order, then dispatches live.

Late subscribers must use `getCachedRuntimeState()` instead of the event.
A failed handshake in a desktop build shows a retry/continue overlay; plain
browsers skip the handshake entirely. Bridge protocol version is checked on
both ends (`BRIDGE_PROTOCOL_VERSION` in Rust, `bridgeVersion` in the
script, `EXPECTED_BRIDGE_PROTOCOL` in the frontend).

## Frontend diagnostics channel

`diagnostics.js` (installed right after the bridge, before page scripts)
forwards `window.onerror`, `unhandledrejection`, `console.error`,
`domcontentloaded`, `load`, and bootstrap markers over the typed
`diagnostics.report` IPC method. Rust truncates, throttles (30 warn-level
reports per minute, rest degrade to debug), and log-gates them. The method
performs no host action and returns no privileged data.

## Embedding model laziness

Boot skips the ONNX/Transformers download when no memory facts exist yet;
recall degrades to keyword search. The first stored fact warms the model in
the background, and the boot backfill covers anything missed. Model
failures never break the page.
