# Utsuwa computer use — permissions, sessions, and platform notes

Every privileged action flows through one architecture:

```text
model
  ↓
ToolRegistry
  ↓
policy authorization
  ↓
scoped CapabilityTicket(s)
  ↓
tool broker / native backend
  ↓
OS / network / filesystem
```

There is no model-to-OS shortcut. Tools declare every ticket they
enforce (`required_capabilities`); the agent loop mints those tickets
before execution. A tool that needs more authority mid-call fails with
a structured, retriable `permission_required` naming the exact
capability, host, and port — authority is never silently widened.

## Separate permissions

These are different capabilities and never imply each other:

| Grant | Covers |
| --- | --- |
| `ScreenCapture` | taking screenshots / frames of the shared screen |
| `DesktopObserve` | accessibility tree, snapshots, window listing |
| `DesktopControl` | clicks, typing, window management (one action per model response, always after a fresh observation) |
| `CameraObserve` | one camera device, matched exactly |
| `MicrophoneCapture` | one microphone device, matched exactly — never the default as a silent fallback |
| `ClipboardRead` / `ClipboardWrite` | the system clipboard |
| `NotificationSend` | showing an OS notification (user-visible, so explicit even though low-friction) |
| `FilesystemRead` / `FilesystemWrite` / `FilesystemCreate` | file trees |
| `NetworkConnect` | one `host:port` — a ticket for one origin never covers another |
| Browser tab control | `DesktopControl` scoped to `BrowserTab(id)` — desktop-window grants never authorize tabs and vice versa |

Microphone and camera tickets name the exact device (`"default"` is
its own scope, not a wildcard). Requesting a missing microphone fails
with `invalid_target` and points at `audio.list_devices`; the backend
opens exactly the authorized device or nothing.

## Screen sharing sessions

Starting a share creates a session. Approvals for session-derived
authority (capture, observation, control) granted while sharing are
bound to that session.

Stopping the share deterministically tears the session down:

- capture stops and session frame artifacts are deleted,
- control is disabled and the application allowlist clears,
- exactly the session-bound grants are revoked — unrelated
  filesystem, network, camera, and microphone grants survive.

A stopped session id cannot be reused; a new share requires new
authority where appropriate.

## Emergency stop

The emergency stop immediately disables mouse, keyboard, and semantic
UI actions while leaving screen observation active. Control grants are
revoked. The user clears it explicitly in the UI, after which control
may be re-authorized per call. Ordinary share termination performs the
same teardown for its session (above); emergency stop is the
interrupt-everything path.

## Application allowlist

`desktop.capture_start` accepts an `allowed_applications` scope. While
an allowlist is active, every window-targeted control action and the
`application.launch`, `application.activate`, and `application.quit` tools
must be attributable to an allowed application. Application names are
validated as bare identities, compared using platform-aware canonical keys,
and never interpreted as shell commands. A PID used by `application.quit`
is resolved to its executable identity before the process can be signalled.
Unknown windows, process identities, and backend listing failures are
rejected with `application_identity_unverified` — the gate fails closed,
never open. With no allowlist, legacy capability behavior applies.

`application.list` is intentionally read-only and unrestricted: it reports
process metadata for discovery, while every application action remains
capability- and scope-gated. An empty allowlist means unrestricted; a
non-empty list is an explicit restriction and is never widened implicitly.
Scope denials are recorded as denied audit decisions with the tool and
canonical application identity (or PID plus the verification failure),
without recording application contents or sensor data.

The native host also publishes authoritative camera and microphone activity
events. Persistent indicators in the app chrome remain visible for the full
capture lifetime, including multiple simultaneous sessions and automatic or
abnormal termination; model-facing `camera.status` and `audio.status` calls
are not required for human visibility.

## HTTP redirects

Redirects are followed explicitly, never automatically. Every hop —
the initial URL and each redirect target — independently passes URL
parsing, scheme validation, the SSRF/private-IP guard, and an exact
`NetworkConnect` ticket check. A cross-origin redirect without its own
ticket returns `redirect_authorization_required` (with host/port) so
the agent can authorize and retry. `Authorization`, `Cookie`, and proxy
credential headers are stripped when crossing origins. Loops and
over-long chains fail instead of spinning.

## Browser / CDP

Browser control needs a Chrome/Chromium debugger endpoint. Configure
it with the `browser.cdp_endpoint` setting (default
`http://localhost:9222`):

```text
chrome --remote-debugging-port=9222
```

Security rules:

- Only loopback `http` endpoints are honored. A remote or non-HTTP
  value warns and falls back to the default — the model never selects
  the endpoint, and remote debugging targets are never used.
- `browser.status` is always advertised; tab control tools appear only
  while a browser answers at the endpoint.
- Sensitive nodes (passwords, PINs, OTPs, payment fields, API keys,
  secrets, private keys, seed phrases, …) are masked before model
  output, mirroring desktop redaction.
- `browser.cookies.list` returns metadata (name/domain/path/flags)
  with values redacted — there is no raw opt-out. Cookie mutation
  (`set`/`delete`) is destructive and needs a tab control ticket.
- Navigating an existing tab needs both the destination network ticket
  and a control ticket for that tab.

## FFmpeg / media

`media.metadata`, `media.video_metadata`, and `media.audio_metadata`
run in-process and are always available. Frame extraction, keyframes,
thumbnails, and waveforms need an `ffmpeg` binary; those tools are
hidden while it is missing instead of failing per call. The process
runner drains pipes on dedicated threads (no pipe-buffer deadlock),
caps stdout (8 MiB) and stderr (64 KiB), enforces a hard timeout, and
kills/reaps the child on timeout or overflow.

## Archives and Git

- `archive.extract` needs read (archive) + create/write
  (destination); `archive.create` needs read (source) + create/write
  (output). Traversal, symlink escape, and decompression bombs are
  rejected.
- Reads (`status`, `diff`, `log`, `show`, …) need repository read;
  mutations need read + write. `fetch`/`pull`/`push` additionally need
  a `NetworkConnect` ticket for the resolved remote (`host:port`,
  retriable) — one remote never covers another. `git.checkout`
  switches branches (`git switch`) or detaches at tags/commits; file
  paths and option-shaped targets are rejected, never restored.

## Wayland (Linux)

Native capture/control uses the XDG ScreenCast portal (PipeWire) and
XDG RemoteDesktop — never X11 calls on a Wayland session. Semantic accessibility (roles, names, trees, element actions)
prefers native AT-SPI2/D-Bus (`desktop.linux-atspi`: `atspi://…` ids,
bounded walks, explicit `BackendUnavailable` with no registry) and
falls back to XWayland only for non-AT-SPI ids. Capture,
control, and accessibility are three separate layers: portal
permission does not imply XWayland presence.

## Errors and audit

Machine-readable codes are stable across tools:
`permission_required`, `permission_denied`, `backend_unavailable`,
`invalid_target`, `stale_window`, `element_not_found`,
`session_not_found`, `application_not_allowed`,
`application_identity_unverified`, `network_denied`,
`redirect_authorization_required`, `response_too_large`,
`archive_path_traversal`, `archive_too_large`,
`unsupported_operation`, `action_failed`, `emergency_stop_active`.
Error details never carry passwords, tokens, cookies, clipboard
secrets, media bytes, or private file contents; audit records carry
metadata (tool, capability, resource class, outcome, session id, error
class, duration) with secret-shaped values redacted.

## Validation

Install frontend dependencies once with `pnpm install --frozen-lockfile`,
then run the canonical local suite:

```bash
./scripts/verify.sh
```

On Windows, run the equivalent PowerShell entry point:

```powershell
.\scripts\verify.ps1
```

The scripts run formatting, locked workspace check/tests/clippy, and the
configured frontend check/tests with `UTSUWA_SKIP_WEB_BUILD=1` for the Rust
commands. Hosted GitHub Actions may not execute while repository billing
restrictions are active; local verification is the acceptance path for this
project. Hardware paths (camera, microphone, capture, accessibility) are
covered by fake-backend tests and platform compile checks; real devices need
manual validation.

When billing is restored, GitHub CI validates the web build and the Rust
workspace on Linux, Windows, and macOS.
