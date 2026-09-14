---
title: Computer Use
description: Screen sharing, desktop control, browser automation, and local-machine tools — permissions, safety, and backend limits.
---

# Computer Use

An authorized model can see the screen, understand UI structure, drive applications, browse the web semantically, and use local-machine tools (files, HTTP, archives, Git, notifications, camera, microphone, media analysis). Every capability is a **separate permission** — there is no single "AI access" switch.

## Separate permissions

| Permission | What it allows | Where you grant it |
|---|---|---|
| Screen visible to AI | Sampled screenshots, capture sessions, accessibility snapshots | Share Screen button |
| Computer control | Pointer, keyboard, semantic UI/window actions | Allow Control toggle (off by default) |
| Accessibility state | Window/element inspection (no pixels) | Share Screen (observation scope) |
| Camera | Still photos and frame sessions | Per-device approval dialog |
| Microphone | Audio capture sessions and recordings | Per-device approval dialog |
| Browser navigation | Opening/navigating URLs (per domain) | Per-domain approval dialog |
| Filesystem | Reading/writing files (per scope) | Per-scope approval dialog |
| Network | HTTP requests, downloads, Git remotes (per host) | Per-host approval dialog |

Screen sharing and control stay independent: starting a share never enables control, and either can be stopped instantly.

## Screen sharing

- **Share Screen** starts observation of the entire desktop, one display, or one window (on Wayland the system portal owns the chooser).
- The model receives **sampled observations** (~1 FPS idle, up to ~3 FPS briefly after an action), never a raw video stream. Full screenshots happen on explicit request.
- **Pause / Resume** freezes frame delivery; **Stop** ends the session, deletes temporary frame artifacts, disables control, clears the application allowlist, and revokes session state.
- You can scope a session to **allowed applications**: actions targeting anything outside the list are rejected with `application_not_allowed`.

## Emergency stop

The red **Emergency stop** button (and its keyboard shortcut) immediately disables all pointer, keyboard, and semantic UI actions while keeping observation on. It also revokes standing control grants and clears per-session control flags. Only you can clear it — the model cannot.

- Default shortcut: **Ctrl/⌘ + Alt/⌥ + Shift + X**.
- The letter is configurable in the Share Screen panel (modifiers are fixed). It is stored locally in your browser settings.

## Camera and microphone

Camera and microphone are never activated silently. While either runs, the indicator shows `Camera: ON` / `Microphone: ON` alongside screen/control state. Photos, frames, and recordings are expiring sensitive artifacts: they are never persisted automatically, never logged, and never enter audit records.

## Browser automation

Browser tools work on **semantic state** (tabs, accessibility/DOM nodes), not screenshots. The bundled backend speaks the Chrome DevTools Protocol:

1. Launch Chrome yourself with remote debugging: `chrome --remote-debugging-port=9222`.
2. Utsuwa connects to `http://localhost:9222` — it never launches or downloads a browser.
3. The model snapshots tabs (`browser.snapshot`), queries nodes (`browser.query`), and acts on node ids (`browser.click`, `browser.type`, `browser.set_value`, …).

Navigation needs per-domain network authorization. Cookie writes are high-risk and always ask explicitly. Password and secret fields are masked before they reach the model.

## Local-machine tools

- **HTTP** (`http.get`, `http.head`, `http.request`, `http.download`): per-host authorization plus SSRF protection (loopback, private LAN, link-local, and metadata addresses are blocked unless the host explicitly allows them). Responses and downloads are size-bounded; downloads land as artifacts first and reach disk only through a separate authorized file write.
- **Archives** (`archive.list`, `archive.extract`, `archive.create`): zip/tar/tar.gz with path-traversal, absolute-path, symlink, and decompression-bomb guards.
- **Git** (`git.status` … `git.push`): read-only inspection is cheap; mutations are destructive-classified; network operations need remote-host authorization. Creating a commit never implies pushing.
- **Notifications** (`notification.show`): native toasts with no action callbacks.
- **System** (`system.cpu`, `system.memory`, …): read-only host facts; environment variables are curated, with secrets redacted.
- **Clipboard** (`clipboard.read/write/clear`): explicit per-call tickets; contents never enter prompts implicitly.
- **Applications** (`application.list/launch/quit/activate`): validated identities only — no shell strings, no launch arguments.
- **Media** (`media.metadata`, `media.video_frame(s)`, `media.video_keyframes`, `media.thumbnail`, `media.waveform`): file analysis with bounded output. Video understanding samples at 1 FPS, deduplicates by perceptual hash, and keeps only visually distinct frames within a strict budget. Frame decoding needs an `ffmpeg` binary; without one these tools report `backend_unavailable`.

## Backend limitations

- **Linux X11**: full capture, control, and accessibility enumeration.
- **Linux Wayland**: capture via XDG ScreenCast/PipeWire; control via RemoteDesktop portal; window/accessibility enumeration depends on compositor support and may honestly report `backend_unavailable` where the portal exposes nothing.
- **Windows**: Graphics Capture, UI Automation semantic actions with SendInput fallback, multi-display and DPI-aware coordinates.
- **macOS**: ScreenCaptureKit, AX accessibility (secure text fields are natively classified), CGEvent fallback. Missing Screen Recording / Accessibility / Camera / Microphone grants produce permission-specific errors, not generic failures.

Native backends never fake success: an unsupported operation returns `backend_unavailable` or `unsupported_operation` with a recovery hint.

## Model multimedia requirements

Providers declare capabilities (tool calls, image/audio/video input, image tool results, structured output, streaming). Endpoints without native image tool-result support receive explicit metadata text instead of images — media is never silently dropped. Video is always reduced to sampled timestamped frames before it reaches a model without native video input.

## Tool profiles

Settings → agent profile selects the visible tool surface: **Minimal** (clock + limited reads), **Standard** (files, HTTP, archives, notifications), **Developer** (plus processes and Git), **ComputerUse** (desktop, browser, clipboard, applications, camera, audio, media), **Full** (everything). Capabilities still gate every call — profiles only change what the model sees.
