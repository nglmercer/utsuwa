---
title: Desktop Guide
description: How to run the Utsuwa native desktop application.
---

# Desktop Guide

Utsuwa Desktop is a native application (`crates/app-host`, Rust + WebView) that runs the same SvelteKit frontend as the web version — plus the agent runtime (model chat, tools, plugins, memory) over a typed IPC bridge. Your save files are compatible between both.

It runs on Linux today (X11/XWayland and native Wayland) with the same binary covering both display servers. macOS and Windows hosts are planned.

## Running from Source

### Prerequisites

- Node.js 22+
- [Rust toolchain](https://rustup.rs/)
- pnpm
- Linux: WebKitGTK system libraries (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`)

```bash
# Clone the repo
git clone https://github.com/JuiceBoxxGames/utsuwa.git
cd utsuwa

# Install frontend dependencies and build the bundled assets
pnpm install
pnpm build

# Run the desktop app
cargo run
```

`cargo run` serves the bundled frontend from `build/`. For frontend development with hot reload, run the native Vite script alongside the host:

```bash
pnpm dev:native
cargo run -- --dev
```

`pnpm dev` remains the browser-only workflow. It does not expose the Rust
filesystem or process runtime when opened in a normal browser. The native host
installs `window.utsuwa`; Utsuwa uses that runtime bridge as the authoritative
signal for native chat and routes native turns through AgentRuntime, including
its policy, capability-ticket, and approval checks. A native WebView can still
use AgentRuntime if it is pointed at a plain `pnpm dev` server, but
`pnpm dev:native` also sets the packaging hint used for static/build-specific
behavior.

## Updating

There is no auto-updater and no installer pipeline yet. Update by pulling the repo and rebuilding:

```bash
git pull
pnpm install
pnpm build
cargo build --release -p app-host
```

## Features

The desktop window provides the full Utsuwa experience — same as the web version with all features:

- VRM avatar with animations
- Chat interface, including the agent runtime when a model is configured
- Settings and configuration
- Memory and relationship systems

The previous overlay mode, global hotkeys, and in-app updates belonged to the removed Tauri shell and are not available in the native host.

## Known Limitations

| Feature | Status |
|---------|--------|
| Linux support (X11 + Wayland) | ✅ Available |
| macOS / Windows hosts | ⏳ Planned |
| Overlay mode | ❌ Removed with the Tauri shell |
| Global hotkeys | ❌ Removed with the Tauri shell |
| In-app auto-updates | ❌ No updater; rebuild from source |
| Installer packages | ❌ No pipeline yet |

## Troubleshooting

### App won't start

If you built from source, make sure Rust is installed:

```bash
rustc --version
```

If not installed, run:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Linux, a missing `libwebkit2gtk-4.1` at launch means the system libraries aren't installed (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev` on Debian/Ubuntu).

If the window fails on a Wayland session, the XWayland fallback still works:

```bash
env -u WAYLAND_DISPLAY cargo run
```

### Voice input not working

The native webview does not implement the browser's Web Speech API. For voice input on desktop, configure a local Whisper server, a Groq API key, or an OpenAI API key in **Settings > Character** under the Voice Input (STT) section.

## Technical Details

The desktop app uses:

- **app-host** — Rust native host: agent runtime, tools, plugins, policy, storage
- **wry + GTK** — WebView embedded in a GTK window (Wayland and X11 from one binary)
- **Same SvelteKit codebase** — No fork, shared components
- **Runtime detection** — `isNativeRuntimeAvailable()` checks the injected `window.utsuwa` bridge; `UTSUWA_NATIVE=1` remains a packaging/build hint

For architecture details, see [Architecture Overview](/docs/technology/architecture).
